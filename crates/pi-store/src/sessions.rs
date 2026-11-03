//! Read-only listing of saved pi sessions.
//!
//! The RPC protocol has no "list sessions" command, so this reads the session
//! directory itself (format: pi's `docs/session-format.md`). Two layouts:
//! - default (`~/.pi/agent/sessions`): `<root>/--<encoded-cwd>--/<file>.jsonl`;
//! - `--session-dir <root>`: `<root>/<file>.jsonl`.
//!
//! The first line is the header `{"type":"session","version","id","cwd",…}`.
//! The title is the latest `session_info` name, else the first user message,
//! as pi's own `/resume` list shows it. Both can sit anywhere in the file, so
//! [`Scanner`] reads whole files but caches them by length and mtime: a
//! rescan only reads files that changed. Files that do not parse, or carry
//! an unsupported `version`, are skipped and counted.

use std::{
	collections::HashMap,
	fs,
	path::{Path, PathBuf},
	time::SystemTime,
};

use serde_json::Value;

/// Session header versions this parser understands (pi migrates older
/// files to 3 when it opens them).
pub const SUPPORTED_VERSIONS: &[u64] = &[1, 2, 3];
/// Longest title kept, in characters.
const TITLE_CHARS: usize = 120;

#[derive(Debug, Clone, PartialEq)]
pub struct SessionInfo {
	pub path:     PathBuf,
	pub id:       String,
	pub cwd:      PathBuf,
	/// Session name, else the first user message (one line, trimmed).
	pub title:    Option<String>,
	/// Header `timestamp` (ISO 8601), when present.
	pub created:  Option<String>,
	/// Session file this one was forked from.
	pub parent:   Option<String>,
	pub modified: SystemTime,
}

impl SessionInfo {
	/// Last four characters of the id, for display (`#8f3a`).
	pub fn short_id(&self) -> &str {
		let start = self.id.char_indices().rev().nth(3).map_or(0, |(i, _)| i);
		&self.id[start..]
	}

	fn matches(&self, needle: &str) -> bool {
		needle.is_empty()
			|| self.title.as_deref().is_some_and(|t| t.to_lowercase().contains(needle))
			|| self.cwd.to_string_lossy().to_lowercase().contains(needle)
			|| self.id.to_lowercase().contains(needle)
	}
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Scan {
	/// Newest first.
	pub sessions: Vec<SessionInfo>,
	/// Non-empty files that could not be parsed or have an unsupported version.
	pub skipped:  usize,
}

/// Where pi stores sessions: `$PI_CODING_AGENT_SESSION_DIR`, else an
/// absolute `sessionDir` in `<agent dir>/settings.json`, else
/// `<agent dir>/sessions`, where the agent dir is `$PI_CODING_AGENT_DIR` or
/// `~/.pi/agent`.
pub fn default_root() -> Option<PathBuf> {
	let env = |key: &str| std::env::var_os(key).filter(|v| !v.is_empty()).map(PathBuf::from);
	if let Some(dir) = env("PI_CODING_AGENT_SESSION_DIR") {
		return Some(dir);
	}
	let home = env("HOME");
	let agent = env("PI_CODING_AGENT_DIR").or_else(|| home.as_ref().map(|h| h.join(".pi/agent")))?;
	let configured = fs::read_to_string(agent.join("settings.json"))
		.ok()
		.and_then(|text| serde_json::from_str::<Value>(&text).ok())
		.and_then(|settings| settings.get("sessionDir")?.as_str().map(str::to_owned))
		.and_then(|dir| match (dir.strip_prefix("~/"), &home) {
			(Some(rest), Some(home)) => Some(home.join(rest)),
			_ => Some(PathBuf::from(dir)).filter(|p| p.is_absolute()),
		});
	Some(configured.unwrap_or_else(|| agent.join("sessions")))
}

#[derive(Debug, Clone, PartialEq)]
struct Cached {
	len:      u64,
	modified: SystemTime,
	parsed:   Option<Parsed>,
}

/// Lists sessions under one root, re-reading only files that changed.
#[derive(Debug, Default)]
pub struct Scanner {
	cache: HashMap<PathBuf, Cached>,
}

impl Scanner {
	/// Lists every session under `root`. A missing root is an empty list.
	pub fn scan(&mut self, root: &Path) -> Scan {
		// Canonical paths, so they compare equal to an open tab's session file.
		let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
		let mut files = Vec::new();
		if let Ok(entries) = fs::read_dir(&root) {
			for entry in entries.flatten() {
				let path = entry.path();
				let Ok(kind) = entry.file_type() else { continue };
				if kind.is_dir() {
					let Ok(children) = fs::read_dir(&path) else { continue };
					files.extend(children.flatten().map(|child| child.path()));
				} else {
					files.push(path);
				}
			}
		}
		let mut scan = Scan::default();
		let mut cache = HashMap::with_capacity(files.len());
		for path in files {
			let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
			if name.starts_with('.') || !name.ends_with(".jsonl") {
				continue;
			}
			let Ok(meta) = fs::metadata(&path) else { continue };
			if !meta.is_file() || meta.len() == 0 {
				continue;
			}
			let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
			let entry = match self.cache.remove(&path) {
				Some(cached) if cached.len == meta.len() && cached.modified == modified => cached,
				_ => Cached {
					len: meta.len(),
					modified,
					parsed: fs::read(&path).ok().and_then(|bytes| parse(&String::from_utf8_lossy(&bytes))),
				},
			};
			match &entry.parsed {
				Some(parsed) => scan.sessions.push(SessionInfo {
					path: path.clone(),
					id: parsed.id.clone(),
					cwd: parsed.cwd.clone(),
					title: parsed.title.clone(),
					created: parsed.created.clone(),
					parent: parsed.parent.clone(),
					modified,
				}),
				None => scan.skipped += 1,
			}
			cache.insert(path, entry);
		}
		self.cache = cache;
		scan.sessions.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| a.path.cmp(&b.path)));
		scan
	}
}

/// What a session file says about itself.
#[derive(Debug, Clone, PartialEq)]
pub struct Parsed {
	pub id:      String,
	pub cwd:     PathBuf,
	pub title:   Option<String>,
	pub created: Option<String>,
	pub parent:  Option<String>,
}

/// Parses a session file: the header line, then `session_info` names and the
/// first user message. Lines that do not parse are ignored (a torn last line
/// while pi appends), but a bad header rejects the file.
pub fn parse(text: &str) -> Option<Parsed> {
	let mut lines = text.split('\n').filter(|line| !line.trim().is_empty());
	let header: Value = serde_json::from_str(lines.next()?).ok()?;
	if header.get("type").and_then(Value::as_str) != Some("session") {
		return None;
	}
	let version = header.get("version").and_then(Value::as_u64).unwrap_or(1);
	if !SUPPORTED_VERSIONS.contains(&version) {
		return None;
	}
	let text_field = |key: &str| header.get(key).and_then(Value::as_str).map(str::to_owned);
	let mut name: Option<String> = None;
	let mut first_user: Option<String> = None;
	for line in lines {
		// Cheap filter before parsing: most lines are messages and tool output.
		let is_info = line.contains("\"session_info\"");
		if !is_info && (first_user.is_some() || !line.contains("\"user\"")) {
			continue;
		}
		let Ok(entry) = serde_json::from_str::<Value>(line) else { continue };
		match entry.get("type").and_then(Value::as_str) {
			// The latest one wins, including an explicit clear.
			Some("session_info") => name = entry.get("name").and_then(Value::as_str).and_then(one_line),
			Some("message") if first_user.is_none() => {
				let message = entry.get("message");
				if message.and_then(|m| m.get("role")).and_then(Value::as_str) == Some("user") {
					first_user = message.and_then(|m| m.get("content")).and_then(content_text).and_then(|t| one_line(&t));
				}
			},
			_ => {},
		}
	}
	Some(Parsed {
		id:      text_field("id").filter(|id| !id.is_empty())?,
		cwd:     PathBuf::from(text_field("cwd")?),
		title:   name.or(first_user),
		created: text_field("timestamp"),
		parent:  text_field("parentSession"),
	})
}

fn content_text(content: &Value) -> Option<String> {
	match content {
		Value::String(text) => Some(text.clone()),
		Value::Array(parts) => {
			let text: Vec<&str> = parts
				.iter()
				.filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
				.filter_map(|p| p.get("text").and_then(Value::as_str))
				.collect();
			(!text.is_empty()).then(|| text.join(" "))
		},
		_ => None,
	}
}

/// Whitespace collapsed to single spaces, cut to [`TITLE_CHARS`]; `None`
/// when blank.
fn one_line(text: &str) -> Option<String> {
	let mut out = String::new();
	for (count, word) in text.split_whitespace().enumerate() {
		if count > 0 {
			out.push(' ');
		}
		out.push_str(word);
		if out.chars().count() > TITLE_CHARS {
			let cut: String = out.chars().take(TITLE_CHARS).collect();
			return Some(format!("{cut}…"));
		}
	}
	(!out.is_empty()).then_some(out)
}

/// Short age for a list row: `刚刚`, `5m`, `3h`, `昨天`, `4天`.
pub fn relative_age(modified: SystemTime, now: SystemTime) -> String {
	let secs = now.duration_since(modified).map_or(0, |age| age.as_secs());
	match secs {
		0..60 => "刚刚".into(),
		60..3_600 => format!("{}m", secs / 60),
		3_600..86_400 => format!("{}h", secs / 3_600),
		86_400..172_800 => "昨天".into(),
		_ => format!("{}天", secs / 86_400),
	}
}

/// Sessions of one working directory, newest first.
#[derive(Debug, PartialEq)]
pub struct Group<'a> {
	pub cwd:      &'a Path,
	pub sessions: Vec<&'a SessionInfo>,
}

/// Groups `sessions` (newest first) by cwd, keeping only those matching
/// `query` (case-insensitive substring of title, cwd, or id). Groups are
/// ordered by their newest session.
pub fn group<'a>(sessions: &'a [SessionInfo], query: &str) -> Vec<Group<'a>> {
	let needle = query.trim().to_lowercase();
	let mut groups: Vec<Group<'a>> = Vec::new();
	for session in sessions.iter().filter(|s| s.matches(&needle)) {
		match groups.iter_mut().find(|g| g.cwd == session.cwd) {
			Some(group) => group.sessions.push(session),
			None => groups.push(Group { cwd: &session.cwd, sessions: vec![session] }),
		}
	}
	groups
}

#[cfg(test)]
mod tests {
	use std::time::Duration;

	use super::*;

	fn session(id: &str, cwd: &str, rest: &str) -> String {
		format!(
			"{{\"type\":\"session\",\"version\":3,\"id\":\"{id}\",\"timestamp\":\"2026-11-01T00:00:00.000Z\",\"cwd\":\"{cwd}\"}}\n\
			 {{\"type\":\"model_change\",\"id\":\"a\",\"parentId\":null,\"provider\":\"p\",\"modelId\":\"m\"}}\n{rest}"
		)
	}

	fn user(text: &str) -> String {
		format!("{{\"type\":\"message\",\"id\":\"u\",\"parentId\":\"a\",\"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"text\",\"text\":\"{text}\"}}]}}}}\n")
	}

	#[test]
	fn title_is_latest_session_name_else_first_user_message() {
		let p = parse(&session("s", "/w", &format!("{}{}", user("  fix   the\\nbuild "), user("second")))).unwrap();
		assert_eq!((p.id.as_str(), p.cwd.as_path()), ("s", Path::new("/w")));
		assert_eq!(p.title.as_deref(), Some("fix the build"));

		let named = format!(
			"{}{{\"type\":\"session_info\",\"id\":\"i\",\"name\":\"Old\"}}\n{{\"type\":\"session_info\",\"id\":\"j\",\"name\":\"Refactor auth\"}}\n",
			user("hello")
		);
		assert_eq!(parse(&session("s", "/w", &named)).unwrap().title.as_deref(), Some("Refactor auth"));

		let cleared = format!("{}{{\"type\":\"session_info\",\"id\":\"i\",\"name\":\"Old\"}}\n{{\"type\":\"session_info\",\"id\":\"j\"}}\n", user("hello"));
		assert_eq!(parse(&session("s", "/w", &cleared)).unwrap().title.as_deref(), Some("hello"), "a cleared name falls back");

		assert_eq!(parse(&session("s", "/w", "")).unwrap().title, None);
		let long = "x".repeat(300);
		assert_eq!(parse(&session("s", "/w", &user(&long))).unwrap().title.unwrap().chars().count(), TITLE_CHARS + 1);
	}

	#[test]
	fn rejects_bad_headers_but_tolerates_a_torn_tail() {
		assert_eq!(parse("{\"type\":\"session\",\"version\":9,\"id\":\"x\",\"cwd\":\"/w\"}\n"), None);
		assert_eq!(parse("not json\n"), None);
		assert_eq!(parse("{\"type\":\"message\"}\n"), None);
		let torn = format!("{}{{\"type\":\"message\",\"mess", user("ok"));
		assert_eq!(parse(&session("x", "/w", &torn)).unwrap().title.as_deref(), Some("ok"));
	}

	#[test]
	fn scans_both_layouts_skips_artifacts_and_rereads_changed_files() {
		let root = std::env::temp_dir().join(format!("pi-store-scan-{}", std::process::id()));
		_ = fs::remove_dir_all(&root);
		let project = root.join("--w-a--");
		fs::create_dir_all(project.join("sol-pi")).unwrap();
		fs::write(project.join("2026_s1.jsonl"), session("s1", "/w/a", &user("one"))).unwrap();
		// Extension state inside the project dir: not a session.
		fs::write(project.join("sol-pi").join("state.jsonl"), session("sub", "/w/a", "")).unwrap();
		fs::write(project.join(".2026_s1.jsonl.lock"), "").unwrap();
		std::thread::sleep(Duration::from_millis(20));
		// Flat layout: a session file directly under root.
		fs::write(root.join("2026_s2.jsonl"), session("s2", "/w/b", "")).unwrap();
		fs::write(project.join("broken.jsonl"), "garbage\n").unwrap();
		fs::write(project.join("empty.jsonl"), "").unwrap();

		let mut scanner = Scanner::default();
		let scan = scanner.scan(&root);
		let ids: Vec<_> = scan.sessions.iter().map(|s| s.id.as_str()).collect();
		assert_eq!(ids, ["s2", "s1"], "newest first");
		assert_eq!(scan.skipped, 1);
		assert_eq!(scan.sessions[1].title.as_deref(), Some("one"));

		// Appending a name changes the title on the next scan.
		let file = project.join("2026_s1.jsonl");
		let mut text = fs::read_to_string(&file).unwrap();
		text.push_str("{\"type\":\"session_info\",\"id\":\"n\",\"name\":\"Named\"}\n");
		fs::write(&file, text).unwrap();
		let scan = scanner.scan(&root);
		assert_eq!(scan.sessions.iter().find(|s| s.id == "s1").unwrap().title.as_deref(), Some("Named"));

		assert_eq!(Scanner::default().scan(&root.join("missing")), Scan::default());
		_ = fs::remove_dir_all(&root);
	}

	#[test]
	fn groups_by_cwd_and_filters() {
		let at = |id: &str, cwd: &str, title: Option<&str>, age: u64| SessionInfo {
			path:     PathBuf::from(format!("/s/{id}.jsonl")),
			id:       id.into(),
			cwd:      PathBuf::from(cwd),
			title:    title.map(Into::into),
			created:  None,
			parent:   None,
			modified: SystemTime::UNIX_EPOCH + Duration::from_secs(1_000 - age),
		};
		let sessions = vec![at("a1", "/w/a", Some("Fix login"), 1), at("b1", "/w/b", None, 2), at("a2", "/w/a", Some("Docs"), 3)];
		let groups = group(&sessions, "");
		assert_eq!(groups.len(), 2);
		assert_eq!(groups[0].sessions.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), ["a1", "a2"]);
		assert_eq!(group(&sessions, "LOGIN")[0].sessions.len(), 1);
		assert_eq!(group(&sessions, "/w/b")[0].cwd, Path::new("/w/b"));
		assert!(group(&sessions, "nothing").is_empty());
		assert_eq!(sessions[0].short_id(), "a1");
	}

	#[test]
	fn relative_age_buckets() {
		let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10_000_000);
		let ago = |secs| now - Duration::from_secs(secs);
		assert_eq!(relative_age(ago(5), now), "刚刚");
		assert_eq!(relative_age(ago(300), now), "5m");
		assert_eq!(relative_age(ago(7_200), now), "2h");
		assert_eq!(relative_age(ago(90_000), now), "昨天");
		assert_eq!(relative_age(ago(400_000), now), "4天");
		assert_eq!(relative_age(now + Duration::from_secs(9), now), "刚刚", "clock skew");
	}
}
