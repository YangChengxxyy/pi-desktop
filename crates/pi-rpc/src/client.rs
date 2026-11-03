//! Blocking JSONL client for one `pi --mode rpc` process.
//!
//! A reader thread splits stdout on LF only (JSON strings may hold U+2028),
//! routes `response` records to the waiting call by `id`, and forwards every
//! other record as a [`Notification`]. Commands are handled concurrently by
//! pi, so calls from several threads may be outstanding at once.

use std::{
	collections::HashMap,
	fmt,
	io::{self, BufRead as _, BufReader, Read, Write},
	process::{Child, ChildStdin, Command as Process, Stdio},
	sync::{
		Arc, Mutex,
		atomic::{AtomicU64, Ordering},
		mpsc::{self, Receiver, Sender},
	},
	thread::{self, JoinHandle},
	time::{Duration, Instant},
};

use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

use crate::wire::Notification;

/// Deadline for commands that do not pass their own.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);
/// How long teardown waits for pi to exit after stdin closes.
const EXIT_GRACE: Duration = Duration::from_millis(1500);
const KILL_GRACE: Duration = Duration::from_secs(1);
const POLL: Duration = Duration::from_millis(10);

#[derive(Debug)]
pub enum Error {
	Io(io::Error),
	Json(serde_json::Error),
	/// pi answered `success: false`.
	Command { command: String, error: String },
	Timeout { command: String },
	/// The process exited or the client was closed.
	Closed,
}

impl fmt::Display for Error {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Io(error) => write!(f, "io: {error}"),
			Self::Json(error) => write!(f, "json: {error}"),
			Self::Command { command, error } => write!(f, "{command} failed: {error}"),
			Self::Timeout { command } => write!(f, "{command} timed out"),
			Self::Closed => f.write_str("pi exited"),
		}
	}
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
	fn from(error: io::Error) -> Self {
		Self::Io(error)
	}
}

impl From<serde_json::Error> for Error {
	fn from(error: serde_json::Error) -> Self {
		Self::Json(error)
	}
}

type Reply = Result<Value, Error>;

#[derive(Default)]
struct Pending {
	waiting: HashMap<String, Sender<Reply>>,
	closed:  bool,
}

pub struct Client {
	stdin:   Mutex<Option<ChildStdin>>,
	pending: Arc<Mutex<Pending>>,
	child:   Mutex<Option<Child>>,
	pid:     Option<u32>,
	next_id: AtomicU64,
	reader:  Mutex<Option<JoinHandle<()>>>,
}

impl Client {
	/// Spawns `process` with piped stdin/stdout as the leader of a new
	/// process group, so teardown reaches the tools pi started too.
	pub fn spawn(mut process: Process) -> Result<(Self, Receiver<Notification>), Error> {
		#[cfg(unix)]
		std::os::unix::process::CommandExt::process_group(&mut process, 0);
		let mut child = process.stdin(Stdio::piped()).stdout(Stdio::piped()).spawn()?;
		let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
			_ = child.kill();
			return Err(Error::Closed);
		};
		let pid = Some(child.id());
		let (client, events) = Self::connect(stdout, Some(stdin), pid);
		*client.child.lock().unwrap() = Some(child);
		Ok((client, events))
	}

	fn connect(stdout: impl Read + Send + 'static, stdin: Option<ChildStdin>, pid: Option<u32>) -> (Self, Receiver<Notification>) {
		let pending = Arc::new(Mutex::new(Pending::default()));
		let (tx, events) = mpsc::channel();
		let shared = pending.clone();
		let reader = thread::Builder::new()
			.name("pi-rpc-reader".into())
			.spawn(move || read_loop(stdout, &shared, &tx))
			.expect("spawn reader thread");
		let client = Self {
			stdin: Mutex::new(stdin),
			pending,
			child: Mutex::new(None),
			pid,
			next_id: AtomicU64::new(1),
			reader: Mutex::new(Some(reader)),
		};
		(client, events)
	}

	pub fn pid(&self) -> Option<u32> {
		self.pid
	}

	/// Writes one record (an `extension_ui_response`, or a command whose
	/// response nobody waits for).
	pub fn send(&self, record: &Value) -> Result<(), Error> {
		let mut line = serde_json::to_vec(record)?;
		line.push(b'\n');
		let mut stdin = self.stdin.lock().unwrap();
		let stdin = stdin.as_mut().ok_or(Error::Closed)?;
		stdin.write_all(&line).and_then(|()| stdin.flush()).map_err(|_| Error::Closed)
	}

	/// Sends `{"type": command, ...params}` and waits for its `data`
	/// (`Value::Null` when the response carries none).
	pub fn request(&self, command: &str, params: Value, timeout: Option<Duration>) -> Result<Value, Error> {
		let id = format!("d{}", self.next_id.fetch_add(1, Ordering::Relaxed));
		let mut frame = match params {
			Value::Object(map) => map,
			Value::Null => Map::new(),
			other => return Err(Error::Json(serde::ser::Error::custom(format!("params must be an object: {other}")))),
		};
		frame.insert("id".into(), Value::String(id.clone()));
		frame.insert("type".into(), Value::String(command.to_owned()));
		let (tx, rx) = mpsc::channel();
		{
			let mut pending = self.pending.lock().unwrap();
			if pending.closed {
				return Err(Error::Closed);
			}
			pending.waiting.insert(id.clone(), tx);
		}
		if let Err(error) = self.send(&Value::Object(frame)) {
			self.pending.lock().unwrap().waiting.remove(&id);
			return Err(error);
		}
		let reply = match timeout {
			Some(timeout) => rx.recv_timeout(timeout).map_err(|error| match error {
				mpsc::RecvTimeoutError::Timeout => Error::Timeout { command: command.to_owned() },
				mpsc::RecvTimeoutError::Disconnected => Error::Closed,
			}),
			None => rx.recv().map_err(|_| Error::Closed),
		};
		self.pending.lock().unwrap().waiting.remove(&id);
		let response = reply??;
		if response.get("success").and_then(Value::as_bool) == Some(true) {
			Ok(response.get("data").cloned().unwrap_or(Value::Null))
		} else {
			let error = response.get("error").and_then(Value::as_str).unwrap_or("unknown error").to_owned();
			Err(Error::Command { command: command.to_owned(), error })
		}
	}

	/// [`Client::request`] with the default deadline, decoding `data`.
	pub fn call<T: DeserializeOwned>(&self, command: &str, params: Value) -> Result<T, Error> {
		Ok(serde_json::from_value(self.request(command, params, Some(DEFAULT_TIMEOUT))?)?)
	}

	/// Closes stdin (pi's orderly shutdown), then escalates SIGTERM →
	/// SIGKILL on the process group if pi does not exit in time.
	pub fn shutdown(&self) {
		self.stdin.lock().unwrap().take();
		if let Some(mut child) = self.child.lock().unwrap().take() {
			terminate(&mut child);
		}
		if let Some(reader) = self.reader.lock().unwrap().take() {
			let deadline = Instant::now() + KILL_GRACE;
			while !reader.is_finished() && Instant::now() < deadline {
				thread::sleep(POLL);
			}
			if reader.is_finished() {
				_ = reader.join();
			}
		}
	}
}

impl Drop for Client {
	fn drop(&mut self) {
		self.shutdown();
	}
}

fn read_loop(stdout: impl Read, pending: &Mutex<Pending>, events: &Sender<Notification>) {
	let mut reader = BufReader::with_capacity(64 * 1024, stdout);
	let mut line = Vec::new();
	loop {
		line.clear();
		match reader.read_until(b'\n', &mut line) {
			Ok(0) | Err(_) => break,
			Ok(_) => {},
		}
		let text = line.strip_suffix(b"\n").unwrap_or(&line);
		let text = text.strip_suffix(b"\r").unwrap_or(text);
		if text.iter().all(u8::is_ascii_whitespace) {
			continue;
		}
		let Ok(value) = serde_json::from_slice::<Value>(text) else {
			eprintln!("pi-rpc: ignoring a non-JSON stdout line ({} bytes)", text.len());
			continue;
		};
		if value.get("type").and_then(Value::as_str) == Some("response") {
			let id = value.get("id").and_then(Value::as_str).map(str::to_owned);
			let waiter = id.and_then(|id| pending.lock().unwrap().waiting.remove(&id));
			if let Some(waiter) = waiter {
				_ = waiter.send(Ok(value));
			}
			continue;
		}
		// The UI may have dropped the receiver; keep draining so pi never
		// blocks on a full pipe.
		_ = events.send(Notification::from_value(value));
	}
	let mut pending = pending.lock().unwrap();
	pending.closed = true;
	for (_, waiter) in pending.waiting.drain() {
		_ = waiter.send(Err(Error::Closed));
	}
}

fn exited(child: &mut Child, within: Duration) -> bool {
	let deadline = Instant::now() + within;
	loop {
		if !matches!(child.try_wait(), Ok(None)) {
			return true;
		}
		if Instant::now() >= deadline {
			return false;
		}
		thread::sleep(POLL);
	}
}

fn terminate(child: &mut Child) {
	if exited(child, EXIT_GRACE) {
		signal_group(child.id(), Signal::Term);
		return;
	}
	signal_group(child.id(), Signal::Term);
	if exited(child, KILL_GRACE) {
		return;
	}
	signal_group(child.id(), Signal::Kill);
	_ = child.kill();
	_ = exited(child, KILL_GRACE);
}

#[derive(Clone, Copy)]
pub enum Signal {
	Term,
	Kill,
}

/// Signals the process group led by `pid` (a client process, see
/// [`Client::spawn`]). ESRCH (already gone) is ignored.
pub fn signal_group(pid: u32, signal: Signal) {
	#[cfg(unix)]
	if let Ok(pgid) = libc::pid_t::try_from(pid) {
		let signal = match signal {
			Signal::Term => libc::SIGTERM,
			Signal::Kill => libc::SIGKILL,
		};
		// SAFETY: `kill` has no memory-safety preconditions.
		unsafe { libc::kill(-pgid, signal) };
	}
	#[cfg(not(unix))]
	let _ = (pid, signal);
}

#[cfg(test)]
mod tests {
	use std::io::Cursor;

	use serde_json::json;

	use super::*;

	#[test]
	fn reader_splits_on_lf_only_and_forwards_events() {
		// U+2028 inside a string must not split the record.
		let stream = "{\"type\":\"agent_start\"}\r\nnot json\n{\"type\":\"notify\",\"text\":\"a\u{2028}b\"}\n";
		let pending = Mutex::new(Pending::default());
		let (tx, rx) = mpsc::channel();
		read_loop(Cursor::new(stream.as_bytes().to_vec()), &pending, &tx);
		assert_eq!(rx.recv().unwrap(), Notification::AgentStart);
		let Notification::Unknown(value) = rx.recv().unwrap() else { panic!() };
		assert_eq!(value["text"], "a\u{2028}b");
		assert!(pending.lock().unwrap().closed);
	}

	#[test]
	fn responses_reach_their_caller_and_eof_fails_the_rest() {
		let pending = Mutex::new(Pending::default());
		let (first, first_rx) = mpsc::channel();
		let (second, second_rx) = mpsc::channel();
		pending.lock().unwrap().waiting.insert("d1".into(), first);
		pending.lock().unwrap().waiting.insert("d2".into(), second);
		let stream = format!("{}\n", json!({"id":"d1","type":"response","command":"get_state","success":true,"data":{"x":1}}));
		let (tx, _rx) = mpsc::channel();
		read_loop(Cursor::new(stream.into_bytes()), &pending, &tx);
		assert_eq!(first_rx.recv().unwrap().unwrap()["data"]["x"], 1);
		assert!(matches!(second_rx.recv().unwrap(), Err(Error::Closed)));
	}
}
