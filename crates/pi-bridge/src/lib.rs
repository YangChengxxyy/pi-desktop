//! Process side of pi-desktop: spawns `pi --mode rpc`, checks that the
//! installed pi speaks the protocol this client was written against, and
//! moves the blocking `pi_rpc` event receiver onto an [`async_channel`] the
//! UI executor can await.

use std::{
	fmt,
	io::Read as _,
	path::{Path, PathBuf},
	process::Command as Process,
	sync::{Arc, Mutex},
	thread,
	time::{Duration, Instant},
};

use pi_rpc::{Client, Model, Notification, SessionState};

/// Oldest pi whose RPC protocol matches `pi-rpc`: `prompt` responds with a
/// `disposition`, delta-only `message_update`, `agent_settled`.
pub const MIN_PI_VERSION: Version = Version(1, 5, 0);

/// The accounts bridge extension, written next to the app's cache on first
/// use (see [`accounts_extension`]).
pub const ACCOUNTS_EXTENSION: &str = include_str!("../extension/accounts.ts");
/// The slash command the accounts extension registers.
pub const ACCOUNTS_COMMAND: &str = "pi-desktop-accounts";
/// The widget key it reports through.
pub const ACCOUNTS_WIDGET: &str = "pi-desktop/accounts";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub u32, pub u32, pub u32);

impl fmt::Display for Version {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "{}.{}.{}", self.0, self.1, self.2)
	}
}

impl Version {
	/// Parses the first line of `pi --version` (`1.5.0`).
	pub fn parse(output: &str) -> Option<Self> {
		let line = output.lines().map(str::trim).find(|l| !l.is_empty())?;
		let version = line.rsplit(['/', ' ']).next().unwrap_or(line).trim_start_matches('v');
		let mut parts = version.split(['.', '-', '+']).map(str::parse::<u32>);
		Some(Self(parts.next()?.ok()?, parts.next()?.ok()?, parts.next()?.ok()?))
	}
}

#[derive(Debug)]
pub enum BridgeError {
	/// `pi` could not be executed.
	NotInstalled(std::io::Error),
	/// `pi --version` printed something unparseable.
	UnknownVersion(String),
	TooOld { found: Version, required: Version },
	/// The process exited during startup; `stderr` is the tail it printed.
	Exited { stderr: String },
	/// pi started, but no model has usable credentials.
	NoModel,
	Rpc(pi_rpc::Error),
}

impl BridgeError {
	/// The tab should offer the accounts panel rather than a reconnect.
	pub fn needs_login(&self) -> bool {
		match self {
			Self::NoModel => true,
			Self::Exited { stderr } => {
				["No models available", "No model available", "No API key found", "No default model"]
					.iter()
					.any(|marker| stderr.contains(marker))
			},
			_ => false,
		}
	}
}

impl fmt::Display for BridgeError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::NotInstalled(error) => write!(f, "pi is not installed or not on PATH: {error}"),
			Self::UnknownVersion(output) => write!(f, "cannot parse `pi --version` output: {output:?}"),
			Self::TooOld { found, required } => {
				write!(f, "pi {found} is too old; pi-desktop needs {required} or newer (run `pi update`)")
			},
			Self::Exited { stderr } if stderr.trim().is_empty() => f.write_str("pi exited during startup"),
			Self::Exited { stderr } => write!(f, "pi exited during startup: {}", stderr.trim()),
			Self::NoModel => f.write_str("pi has no model with usable credentials"),
			Self::Rpc(error) => write!(f, "pi rpc: {error}"),
		}
	}
}

impl std::error::Error for BridgeError {}

impl From<pi_rpc::Error> for BridgeError {
	fn from(error: pi_rpc::Error) -> Self {
		Self::Rpc(error)
	}
}

/// How to launch one session process.
#[derive(Debug, Clone)]
pub struct SessionOptions {
	pub pi:          PathBuf,
	pub cwd:         Option<PathBuf>,
	/// Session file to open (`--session <file>`); `None` starts a new one.
	pub resume:      Option<PathBuf>,
	/// Open `resume` as a fork: pi copies it into a new file (`--fork`) and
	/// leaves the original untouched.
	pub fork:        bool,
	/// Session storage root (`--session-dir`). `None` keeps pi's default
	/// (`~/.pi/agent/sessions/--<encoded-cwd>--/`); a custom root is flat.
	pub session_dir: Option<PathBuf>,
	/// Extra launch flags (`--no-session`, `--thinking off`, …).
	pub extra_args:  Vec<String>,
	/// Extra environment for the process (tests point `PI_CODING_AGENT_DIR`
	/// at a scratch directory).
	pub env:         Vec<(String, String)>,
	/// Fail with [`BridgeError::NoModel`] when pi starts without a usable
	/// model. The accounts process sets this to `false`.
	pub require_model: bool,
}

impl Default for SessionOptions {
	fn default() -> Self {
		Self {
			pi:            PathBuf::from("pi"),
			cwd:           None,
			resume:        None,
			session_dir:   None,
			fork:          false,
			extra_args:    Vec::new(),
			env:           Vec::new(),
			require_model: true,
		}
	}
}

impl SessionOptions {
	/// Arguments after `pi`.
	pub fn args(&self) -> Vec<std::ffi::OsString> {
		let mut args: Vec<std::ffi::OsString> = vec!["--mode".into(), "rpc".into()];
		if let Some(dir) = &self.session_dir {
			args.extend(["--session-dir".into(), dir.into()]);
		}
		if let Some(file) = &self.resume {
			args.extend([if self.fork { "--fork" } else { "--session" }.into(), file.into()]);
		}
		args.extend(self.extra_args.iter().map(Into::into));
		args
	}
}

/// A connected session process.
pub struct Session {
	pub client:  Arc<Client>,
	pub events:  async_channel::Receiver<Notification>,
	pub version: Version,
	/// `get_state` right after startup.
	pub state:   SessionState,
}

/// Runs `pi --version` and checks it against [`MIN_PI_VERSION`].
pub fn check_version(pi: &Path, env: &[(String, String)]) -> Result<Version, BridgeError> {
	let output = Process::new(pi).arg("--version").envs(env.iter().map(|(k, v)| (k, v))).output().map_err(BridgeError::NotInstalled)?;
	let text = String::from_utf8_lossy(&output.stdout).into_owned();
	let version = Version::parse(&text).ok_or(BridgeError::UnknownVersion(text))?;
	if version < MIN_PI_VERSION {
		return Err(BridgeError::TooOld { found: version, required: MIN_PI_VERSION });
	}
	Ok(version)
}

impl Session {
	/// Blocking: spawns the process and waits until it answers `get_state`.
	/// Run it off the UI thread.
	pub fn spawn(options: &SessionOptions) -> Result<Self, BridgeError> {
		let version = check_version(&options.pi, &options.env)?;
		let mut process = Process::new(&options.pi);
		process.args(options.args());
		if let Some(cwd) = &options.cwd {
			process.current_dir(cwd);
		}
		process.envs(options.env.iter().map(|(k, v)| (k, v)));
		let stderr = capture_stderr(&mut process).map_err(|error| BridgeError::Rpc(pi_rpc::Error::Io(error)))?;
		let (client, receiver) = Client::spawn(process)?;
		// Startup (extensions, MCP servers, model catalogs) finishes before
		// pi reads stdin, so the first answer doubles as the ready signal.
		let state: SessionState = match client.request("get_state", serde_json::Value::Null, Some(STARTUP_TIMEOUT)) {
			Ok(data) => serde_json::from_value(data).map_err(pi_rpc::Error::Json)?,
			Err(pi_rpc::Error::Closed) => return Err(BridgeError::Exited { stderr: stderr.wait_tail() }),
			Err(error) => return Err(error.into()),
		};
		if options.require_model && state.model.as_ref().is_none_or(Model::is_unknown) {
			return Err(BridgeError::NoModel);
		}

		let (tx, events) = async_channel::unbounded();
		thread::Builder::new()
			.name("pi-events".into())
			.spawn(move || {
				// Ends when the client closes (sender dropped) or the UI drops
				// the receiver.
				while let Ok(event) = receiver.recv() {
					if tx.send_blocking(event).is_err() {
						break;
					}
				}
			})
			.map_err(|error| BridgeError::Rpc(pi_rpc::Error::Io(error)))?;

		Ok(Self { client: Arc::new(client), events, version, state })
	}
}

/// Extensions, MCP servers, and model catalog refreshes run before pi
/// answers the first command.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(90);

/// Writes [`ACCOUNTS_EXTENSION`] under `dir` (once per content) and returns
/// its path, for `-e <path>`.
pub fn accounts_extension(dir: &Path) -> std::io::Result<PathBuf> {
	std::fs::create_dir_all(dir)?;
	let path = dir.join("pi-desktop-accounts.ts");
	if std::fs::read_to_string(&path).ok().as_deref() != Some(ACCOUNTS_EXTENSION) {
		std::fs::write(&path, ACCOUNTS_EXTENSION)?;
	}
	Ok(path)
}

/// Sends SIGTERM to the process group of a session spawned by
/// [`Session::spawn`] (it leads its own group). Use it when a blocking call
/// holds the client and the process must stop now.
pub fn terminate_group(pid: u32) {
	pi_rpc::signal_group(pid, pi_rpc::Signal::Term);
}

/// The last bytes pi wrote to stderr. A thread drains the pipe (so pi never
/// blocks on it) and copies everything to this process's stderr.
struct StderrTail {
	tail:   Arc<Mutex<Vec<u8>>>,
	reader: thread::JoinHandle<()>,
}

const STDERR_TAIL_BYTES: usize = 8 * 1024;

impl StderrTail {
	/// Waits for the pipe to close (the process exited) up to a second and
	/// returns the tail without ANSI colour codes.
	fn wait_tail(self) -> String {
		let deadline = Instant::now() + Duration::from_secs(1);
		while !self.reader.is_finished() && Instant::now() < deadline {
			thread::sleep(Duration::from_millis(10));
		}
		let bytes = self.tail.lock().map(|t| t.clone()).unwrap_or_default();
		strip_ansi(&String::from_utf8_lossy(&bytes))
	}
}

fn capture_stderr(process: &mut Process) -> std::io::Result<StderrTail> {
	use std::io::Write as _;
	let (mut reader, writer) = std::io::pipe()?;
	process.stderr(writer);
	let tail = Arc::new(Mutex::new(Vec::new()));
	let sink = tail.clone();
	let reader = thread::Builder::new().name("pi-stderr".into()).spawn(move || {
		let mut buffer = [0u8; 4096];
		while let Ok(read) = reader.read(&mut buffer) {
			if read == 0 {
				break;
			}
			_ = std::io::stderr().write_all(&buffer[..read]);
			if let Ok(mut tail) = sink.lock() {
				tail.extend_from_slice(&buffer[..read]);
				let excess = tail.len().saturating_sub(STDERR_TAIL_BYTES);
				tail.drain(..excess);
			}
		}
	})?;
	Ok(StderrTail { tail, reader })
}

fn strip_ansi(text: &str) -> String {
	let mut out = String::with_capacity(text.len());
	let mut chars = text.chars().peekable();
	while let Some(c) = chars.next() {
		if c == '\u{1b}' && chars.peek() == Some(&'[') {
			chars.next();
			// CSI: parameters, then one final byte in @..~.
			for c in chars.by_ref() {
				if ('@'..='~').contains(&c) {
					break;
				}
			}
		} else {
			out.push(c);
		}
	}
	out
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parses_pi_version_output() {
		assert_eq!(Version::parse("1.5.0\n"), Some(Version(1, 5, 0)));
		assert_eq!(Version::parse("pi/2.0.1-beta.1"), Some(Version(2, 0, 1)));
		assert_eq!(Version::parse("garbage"), None);
		assert!(Version(1, 4, 9) < MIN_PI_VERSION);
		assert!(Version(1, 10, 0) > MIN_PI_VERSION);
	}

	#[test]
	fn startup_exit_reports_missing_credentials() {
		assert_eq!(strip_ansi("\u{1b}[31mNo models\u{1b}[39m."), "No models.");
		assert!(BridgeError::Exited { stderr: strip_ansi("\u{1b}[31mNo API key found for x\u{1b}[39m") }.needs_login());
		assert!(BridgeError::NoModel.needs_login());
		assert!(!BridgeError::Exited { stderr: "boom".into() }.needs_login());
	}

	/// A process that exits before answering yields its stderr.
	#[test]
	fn exited_process_returns_stderr_tail() {
		let mut process = Process::new("/bin/sh");
		process.args(["-c", "echo 'No models available.' >&2; exit 1"]);
		let stderr = capture_stderr(&mut process).unwrap();
		let (client, _events) = Client::spawn(process).unwrap();
		let error = match client.request("get_state", serde_json::Value::Null, Some(Duration::from_secs(5))) {
			Err(pi_rpc::Error::Closed) => BridgeError::Exited { stderr: stderr.wait_tail() },
			other => panic!("unexpected {other:?}"),
		};
		assert!(error.needs_login(), "{error}");
	}

	#[test]
	fn resume_and_fork_flags() {
		let options = SessionOptions { resume: Some("/tmp/a.jsonl".into()), session_dir: Some("/s".into()), ..Default::default() };
		let args = |o: &SessionOptions| o.args().into_iter().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>();
		assert_eq!(args(&options), ["--mode", "rpc", "--session-dir", "/s", "--session", "/tmp/a.jsonl"]);
		assert_eq!(args(&SessionOptions { fork: true, ..options })[4], "--fork");
	}
}
