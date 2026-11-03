//! Slash commands for the composer: completion over pi's `get_commands`
//! catalog (extension commands, prompt templates, skills) plus the desktop's
//! own commands, which map TUI built-ins onto RPC commands. Free of GPUI so
//! it can be tested directly.

use pi_rpc::SlashCommand;

/// A command the desktop runs itself instead of sending it to pi as a prompt
/// (pi's TUI built-ins are not available in RPC mode).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Builtin {
	/// `compact [instructions]`
	Compact,
	/// `new`: a fresh session in this tab.
	New,
	/// `name <name>`
	Name,
	/// `export [path]`: the session as HTML.
	Export,
	/// `copy`: the last reply to the clipboard.
	Copy,
	/// `session`: token and cost totals.
	Session,
	/// `clone`: continue in a copy of this session.
	Clone,
}

pub const BUILTINS: &[(Builtin, &str, &str, Option<&str>)] = &[
	(Builtin::Compact, "compact", "压缩上下文", Some("[说明]")),
	(Builtin::New, "new", "在本标签开始新会话", None),
	(Builtin::Name, "name", "设置会话名称", Some("<名称>")),
	(Builtin::Export, "export", "导出会话为 HTML", Some("[路径]")),
	(Builtin::Copy, "copy", "复制最后一条回复", None),
	(Builtin::Session, "session", "会话统计：消息、token、费用", None),
	(Builtin::Clone, "clone", "复制当前分支为新会话并继续", None),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
	Desktop,
	Extension,
	Prompt,
	Skill,
	Other,
}

impl Source {
	fn of(command: &SlashCommand) -> Self {
		match command.source.as_str() {
			"extension" => Self::Extension,
			"prompt" => Self::Prompt,
			"skill" => Self::Skill,
			_ => Self::Other,
		}
	}

	pub fn label(self) -> &'static str {
		match self {
			Self::Desktop => "内置",
			Self::Extension => "扩展",
			Self::Prompt => "模板",
			Self::Skill => "skill",
			Self::Other => "其他",
		}
	}
}

/// One completion row.
#[derive(Debug, Clone, PartialEq)]
pub struct Suggestion {
	/// Text that replaces the composer value when accepted (`/name `).
	pub insert:      String,
	/// `/name`, as shown.
	pub label:       String,
	pub description: Option<String>,
	/// Argument hint.
	pub hint:        Option<String>,
	pub source:      Source,
}

/// Most rows the popup shows.
pub const LIMIT: usize = 50;

/// The desktop command `text` invokes, with its argument text.
pub fn builtin(text: &str) -> Option<(Builtin, &str)> {
	let rest = text.trim().strip_prefix('/')?;
	let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
	BUILTINS.iter().find(|(_, n, ..)| n.eq_ignore_ascii_case(name)).map(|(b, ..)| (*b, args.trim()))
}

/// Completions for the composer `text`, or `None` when the text is not a
/// command name being typed: it must start with `/`, be one line, and hold
/// no space yet.
pub fn suggest(commands: &[SlashCommand], text: &str) -> Option<Vec<Suggestion>> {
	let typed = text.strip_prefix('/')?;
	if typed.contains(char::is_whitespace) {
		return None;
	}
	let needle = typed.to_lowercase();
	let desktop = BUILTINS.iter().map(|(_, name, description, hint)| Suggestion {
		insert:      format!("/{name} "),
		label:       format!("/{name}"),
		description: Some((*description).to_owned()),
		hint:        hint.map(str::to_owned),
		source:      Source::Desktop,
	});
	let pi = commands.iter().map(|command| Suggestion {
		insert:      format!("/{} ", command.name),
		label:       format!("/{}", command.name),
		description: command.description.clone().filter(|d| !d.trim().is_empty()),
		hint:        None,
		source:      Source::of(command),
	});
	// Name prefix first (shorter names first), then names containing the text.
	let mut ranked: Vec<(u8, Suggestion)> = desktop
		.chain(pi)
		.filter_map(|row| {
			let name = row.label[1..].to_lowercase();
			let rank = if name.starts_with(&needle) {
				0
			} else if name.contains(&needle) {
				1
			} else {
				return None;
			};
			Some((rank, row))
		})
		.collect();
	ranked.sort_by(|(ra, a), (rb, b)| (ra, a.label.len(), &a.label).cmp(&(rb, b.label.len(), &b.label)));
	// The same name from two sources: the first (desktop) one wins.
	let mut rows: Vec<Suggestion> = Vec::new();
	for (_, row) in ranked {
		if !rows.iter().any(|r| r.label == row.label) {
			rows.push(row);
		}
	}
	rows.truncate(LIMIT);
	Some(rows)
}

#[cfg(test)]
mod tests {
	use serde_json::json;

	use super::*;

	fn catalog() -> Vec<SlashCommand> {
		serde_json::from_value(json!([
			{"name":"fff-mode","description":"Show or set FFF mode","source":"extension","sourceInfo":{}},
			{"name":"fix-tests","description":"Fix failing tests","source":"prompt","sourceInfo":{}},
			{"name":"skill:commit","description":"Write a commit","source":"skill","sourceInfo":{}},
			{"name":"compact","description":"an extension override","source":"extension","sourceInfo":{}},
		]))
		.unwrap()
	}

	fn labels(rows: &[Suggestion]) -> Vec<&str> {
		rows.iter().map(|r| r.label.as_str()).collect()
	}

	#[::core::prelude::v1::test]
	fn only_a_leading_slash_on_one_word_completes() {
		let c = catalog();
		assert!(suggest(&c, "").is_none());
		assert!(suggest(&c, "hello /new").is_none());
		assert!(suggest(&c, "/name x").is_none(), "arguments");
		assert_eq!(suggest(&c, "/").unwrap().len(), BUILTINS.len() + c.len() - 1, "duplicate name listed once");
	}

	#[::core::prelude::v1::test]
	fn prefix_beats_substring_and_sources_are_labelled() {
		let c = catalog();
		assert_eq!(labels(&suggest(&c, "/f").unwrap()), ["/fff-mode", "/fix-tests"]);
		assert_eq!(labels(&suggest(&c, "/co").unwrap()), ["/copy", "/compact", "/skill:commit"]);
		let compact = suggest(&c, "/compact").unwrap();
		assert_eq!((compact[0].source, compact[0].hint.as_deref()), (Source::Desktop, Some("[说明]")));
		assert_eq!(suggest(&c, "/skill").unwrap()[0].source, Source::Skill);
		assert!(suggest(&c, "/zzz").unwrap().is_empty());
	}

	#[::core::prelude::v1::test]
	fn builtins_parse_with_arguments() {
		assert_eq!(builtin("/compact keep the API notes"), Some((Builtin::Compact, "keep the API notes")));
		assert_eq!(builtin("  /NEW  "), Some((Builtin::New, "")));
		assert_eq!(builtin("/fix-tests"), None);
		assert_eq!(builtin("compact"), None);
	}
}
