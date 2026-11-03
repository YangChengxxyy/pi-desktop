//! Tool execution state keyed by tool-call id, folded from
//! `tool_execution_*` events and `toolResult` messages, so a view can render
//! each call as one card: arguments, live output, final result, duration.
//!
//! Also parses edit diffs (`details.diff`) into display lines.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use pi_rpc::{AgentMessage, Notification, ToolExecution, UserContent};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
	Running,
	Done,
	Failed,
}

#[derive(Debug, Clone)]
pub struct ToolRun {
	pub name:    String,
	pub status:  Status,
	/// Text output: streamed partial output while running, final content after.
	pub output:  String,
	/// The result's `details` object (tool specific: `diff`, `truncation`…).
	pub details: Value,
	pub started: Instant,
	/// `durationMs` reported by pi, or measured here.
	pub elapsed: Option<Duration>,
}

impl ToolRun {
	fn new(name: &str) -> Self {
		Self {
			name:    name.to_owned(),
			status:  Status::Running,
			output:  String::new(),
			details: Value::Null,
			started: Instant::now(),
			elapsed: None,
		}
	}

	/// The edit diff, for any tool whose result carries one (pi's `edit`,
	/// and extension edit tools that follow its `details.diff` convention).
	pub fn diff(&self) -> Option<Diff> {
		self.details.get("diff").and_then(Value::as_str).filter(|d| !d.is_empty()).map(Diff::parse)
	}

	fn finish(&mut self, output: String, details: Option<Value>, failed: bool, duration_ms: Option<f64>) {
		self.output = output;
		self.details = details.unwrap_or(Value::Null);
		self.status = if failed { Status::Failed } else { Status::Done };
		self.elapsed = Some(
			duration_ms
				.filter(|ms| ms.is_finite() && *ms >= 0.)
				.map(|ms| Duration::from_secs_f64(ms / 1000.))
				.unwrap_or_else(|| self.started.elapsed()),
		);
	}
}

#[derive(Debug, Default)]
pub struct ToolRuns {
	runs: HashMap<String, ToolRun>,
}

impl ToolRuns {
	pub fn get(&self, tool_call_id: &str) -> Option<&ToolRun> {
		self.runs.get(tool_call_id)
	}

	/// The id of the tool call an event is about.
	pub fn event_id(notification: &Notification) -> Option<&str> {
		match notification {
			Notification::ToolExecutionStart(e) | Notification::ToolExecutionUpdate(e) | Notification::ToolExecutionEnd(e) => {
				Some(&e.tool_call_id)
			},
			_ => None,
		}
	}

	/// Folds one record; returns whether a run changed.
	pub fn apply(&mut self, notification: &Notification) -> bool {
		match notification {
			Notification::ToolExecutionStart(start) => {
				self.runs.insert(start.tool_call_id.clone(), ToolRun::new(&start.tool_name));
				true
			},
			Notification::ToolExecutionUpdate(update) => {
				let Some(run) = self.runs.get_mut(&update.tool_call_id) else {
					return false;
				};
				if let Some(partial) = &update.partial_result {
					run.output = content_text(partial.get("content"));
				}
				true
			},
			Notification::ToolExecutionEnd(end) => {
				let run = self.runs.entry(end.tool_call_id.clone()).or_insert_with(|| ToolRun::new(&end.tool_name));
				finish_from_event(run, end);
				true
			},
			// History loaded after the fact has results without execution events.
			Notification::MessageEnd(AgentMessage::ToolResult(result)) => {
				if result.tool_call_id.is_empty() || self.runs.get(&result.tool_call_id).is_some_and(|run| run.status != Status::Running) {
					return false;
				}
				let output = result
					.content
					.iter()
					.filter_map(|part| match part {
						UserContent::Text(text) => Some(text.text.as_str()),
						_ => None,
					})
					.collect::<Vec<_>>()
					.join("\n");
				let run = self.runs.entry(result.tool_call_id.clone()).or_insert_with(|| ToolRun::new(&result.tool_name));
				run.finish(output, result.details.clone(), result.is_error, result.duration_ms);
				true
			},
			_ => false,
		}
	}
}

fn finish_from_event(run: &mut ToolRun, end: &ToolExecution) {
	let result = end.result.clone().unwrap_or(Value::Null);
	run.finish(content_text(result.get("content")), result.get("details").cloned(), end.is_error, end.duration_ms);
}

fn content_text(content: Option<&Value>) -> String {
	content
		.and_then(Value::as_array)
		.into_iter()
		.flatten()
		.filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
		.filter_map(|part| part.get("text").and_then(Value::as_str))
		.collect::<Vec<_>>()
		.join("\n")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
	Context,
	Added,
	Removed,
	/// A `...` gap between hunks.
	Gap,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
	pub kind:   LineKind,
	pub number: Option<u32>,
	pub text:   String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Diff {
	pub lines:   Vec<DiffLine>,
	pub added:   usize,
	pub removed: usize,
}

impl Diff {
	/// Parses an edit diff, one row per line, `<sign><body>` with sign `+`,
	/// `-`, or a space. Two body shapes exist:
	/// - pi's `edit`: `<padded line number> <text>`, and ` <pad> ...` gaps;
	/// - hashline edit extensions: `<anchor>│<text>` (no line number).
	///
	/// Rows that fit neither (hunk separators) become gaps.
	pub fn parse(text: &str) -> Self {
		let mut diff = Diff::default();
		for row in text.lines() {
			let mut chars = row.chars();
			let kind = match chars.next() {
				Some('+') => LineKind::Added,
				Some('-') => LineKind::Removed,
				Some(' ') => LineKind::Context,
				_ => {
					diff.lines.push(DiffLine { kind: LineKind::Gap, number: None, text: row.to_owned() });
					continue;
				},
			};
			let body = chars.as_str();
			let line = if let Some((_, text)) = body.split_once('│').filter(|(anchor, _)| !anchor.contains(' ')) {
				DiffLine { kind, number: None, text: text.to_owned() }
			} else {
				let trimmed = body.trim_start();
				let digits = trimmed.find(|c: char| !c.is_ascii_digit()).unwrap_or(trimmed.len());
				match trimmed[..digits].parse::<u32>() {
					Ok(number) => {
						let text = &trimmed[digits..];
						DiffLine { kind, number: Some(number), text: text.strip_prefix(' ').unwrap_or(text).to_owned() }
					},
					Err(_) => {
						diff.lines.push(DiffLine { kind: LineKind::Gap, number: None, text: trimmed.to_owned() });
						continue;
					},
				}
			};
			match kind {
				LineKind::Added => diff.added += 1,
				LineKind::Removed => diff.removed += 1,
				_ => {},
			}
			diff.lines.push(line);
		}
		diff
	}
}

#[cfg(test)]
mod tests {
	use serde_json::json;

	use super::*;

	fn record(value: Value) -> Notification {
		Notification::from_value(value)
	}

	// Records below are trimmed copies of a real pi 1.5.0 run
	// (`printf x; sleep 1; printf y`).
	#[test]
	fn bash_streams_partial_output_then_finishes_with_duration() {
		let mut runs = ToolRuns::default();
		runs.apply(&record(json!({"type":"tool_execution_start","toolCallId":"bash:0","toolName":"bash","args":{"command":"printf x"}})));
		assert_eq!(runs.get("bash:0").unwrap().status, Status::Running);
		runs.apply(&record(json!({"type":"tool_execution_update","toolCallId":"bash:0","toolName":"bash","args":{},"partialResult":{"content":[]}})));
		runs.apply(&record(json!({"type":"tool_execution_update","toolCallId":"bash:0","toolName":"bash","args":{},"partialResult":{"content":[{"type":"text","text":"x"}],"details":{}}})));
		assert_eq!(runs.get("bash:0").unwrap().output, "x");
		runs.apply(&record(json!({"type":"tool_execution_end","toolCallId":"bash:0","toolName":"bash","result":{"content":[{"type":"text","text":"xy"}]},"isError":false,"durationMs":1023})));
		let run = runs.get("bash:0").unwrap();
		assert_eq!((run.status.clone(), run.output.as_str()), (Status::Done, "xy"));
		assert_eq!(run.elapsed.unwrap().as_millis(), 1023);
	}

	#[test]
	fn failed_end_marks_run_failed_and_history_fills_runs() {
		let mut runs = ToolRuns::default();
		runs.apply(&record(json!({"type":"tool_execution_end","toolCallId":"c","toolName":"read","result":{"content":[{"type":"text","text":"ENOENT"}]},"isError":true})));
		assert_eq!(runs.get("c").unwrap().status, Status::Failed);

		runs.apply(&record(json!({"type":"message_end","message":{"role":"toolResult","toolCallId":"c9","toolName":"edit",
			"content":[{"type":"text","text":"ok"}],"details":{"diff":"+1 a"},"isError":false,"durationMs":5}})));
		let run = runs.get("c9").unwrap();
		assert_eq!((run.name.as_str(), run.status.clone()), ("edit", Status::Done));
		assert_eq!(run.diff().map(|d| d.added), Some(1));
	}

	#[test]
	fn parses_pi_edit_diff_with_padded_numbers_and_gaps() {
		let diff = Diff::parse("  9 fn main() {\n-10     let a = 1;\n+10     let b = 1;\n    ...\n 40 tail");
		assert_eq!((diff.added, diff.removed), (1, 1));
		assert_eq!(diff.lines[0], DiffLine { kind: LineKind::Context, number: Some(9), text: "fn main() {".into() });
		assert_eq!(diff.lines[1], DiffLine { kind: LineKind::Removed, number: Some(10), text: "    let a = 1;".into() });
		assert_eq!(diff.lines[3], DiffLine { kind: LineKind::Gap, number: None, text: "...".into() });
		assert_eq!(diff.lines[4].number, Some(40));
	}

	#[test]
	fn parses_hashline_diff() {
		let diff = Diff::parse("-udyun│hello\n+kgxia│world │ x");
		assert_eq!((diff.added, diff.removed), (1, 1));
		assert_eq!(diff.lines[1], DiffLine { kind: LineKind::Added, number: None, text: "world │ x".into() });
	}
}
