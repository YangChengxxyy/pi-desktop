//! Per-session state the view renders, folded from pi RPC records. Kept
//! free of GPUI so it can be tested directly.

use std::collections::{BTreeMap, HashMap};

use pi_rpc::{AgentMessage, AssistantContent, BashExecutionMessage, Notification, SessionState, SlashCommand, UiRequest};
use pi_store::Transcript;
use pi_store::tools::ToolRuns;

/// Whether the agent is working on something the user sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Activity {
	#[default]
	Idle,
	/// A prompt was sent or a run started, and `agent_settled` has not
	/// arrived.
	Running,
	/// A direct shell command (`!cmd`) runs.
	Shell,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Notice {
	Info(String),
	Error(String),
}

/// A block of text lines an extension shows next to the composer
/// (`setWidget`).
#[derive(Debug, Clone, PartialEq)]
pub struct Widget {
	pub lines: Vec<String>,
	pub below: bool,
}

#[derive(Default)]
pub struct SessionModel {
	pub transcript: Transcript,
	pub tools:      ToolRuns,
	/// Row of the assistant message that issued each tool call, so a tool
	/// event can remeasure that row and its result renders inside the card.
	pub tool_rows:  HashMap<String, usize>,
	pub activity:   Activity,
	pub state:      Option<SessionState>,
	/// Dialogs waiting for an answer, oldest first.
	pub dialogs:    Vec<UiRequest>,
	pub notices:    Vec<Notice>,
	/// `setStatus` texts by extension key, in key order.
	pub statuses:   BTreeMap<String, String>,
	/// `setWidget` blocks by extension key.
	pub widgets:    BTreeMap<String, Widget>,
	/// `setTitle` from an extension; replaces the tab title.
	pub title:      Option<String>,
	pub queue:      Queue,
	/// Control request in flight, if any: its controls stay disabled until
	/// the call returns.
	pub pending:    Option<PendingControl>,
	/// A compaction (manual or automatic) is running.
	pub compacting: bool,
	/// Automatic retry in progress: (attempt, max attempts).
	pub retry:      Option<(i64, i64)>,
	/// Extension commands, prompt templates, and skills from `get_commands`.
	pub commands:   Vec<SlashCommand>,
	/// Row of the `!cmd` shell command that is running.
	pub shell_row:  Option<usize>,
	/// `set_editor_text` asks for this composer text.
	pub editor_text: Option<String>,
	/// Every queue state seen, for tests: a steer is delivered within moments,
	/// so the current queue alone cannot show that it was queued.
	#[cfg(test)]
	pub queue_log:  Vec<Queue>,
}

/// A control command in flight; its controls stay disabled until the call
/// returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingControl {
	Model,
	Thinking,
	Compact,
	AutoCompact,
	Queue,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Queue {
	pub steering:  Vec<String>,
	pub follow_up: Vec<String>,
}

/// A change to the steer / follow-up queue, addressed by position in the
/// queue the user saw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueEdit {
	/// Withdraw the message at this index of the steering (`true`) or
	/// follow-up queue.
	Remove { steering: bool, index: usize },
	/// Move a follow-up to the end of the steering queue.
	Promote { index: usize },
}

impl Queue {
	#[cfg(test)]
	pub fn is_empty(&self) -> bool {
		self.steering.is_empty() && self.follow_up.is_empty()
	}

	/// This queue after `edit`, which addresses a message by its position in
	/// `shown` (the queue on screen). pi delivers from the front, so the
	/// message is matched by text, counting copies of that text from the back:
	/// the edit still applies when earlier messages were delivered meanwhile.
	/// `None` when the message is no longer queued.
	pub fn edit(&self, shown: &Queue, edit: QueueEdit) -> Option<Queue> {
		let (from_steering, index) = match edit {
			QueueEdit::Remove { steering, index } => (steering, index),
			QueueEdit::Promote { index } => (false, index),
		};
		let source = |q: &Queue| if from_steering { q.steering.clone() } else { q.follow_up.clone() };
		let text = source(shown).get(index)?.clone();
		let from_back = source(shown)[index + 1..].iter().filter(|m| **m == text).count();
		let mut list = source(self);
		let position = list.iter().enumerate().rev().filter(|(_, m)| **m == text).nth(from_back).map(|(i, _)| i)?;
		let moved = list.remove(position);
		let mut next = self.clone();
		if from_steering {
			next.steering = list;
		} else {
			next.follow_up = list;
		}
		if let QueueEdit::Promote { .. } = edit {
			next.steering.push(moved);
		}
		Some(next)
	}
}

/// What the view must do after a record.
#[derive(Debug, Default, PartialEq)]
pub struct Effects {
	/// Transcript rows changed: (first changed index, new row count).
	pub rows:          Option<(usize, usize)>,
	/// Session state should be refetched (turn ended, model changed…).
	pub refresh_state: bool,
	/// The slash-command catalog may have changed (`/reload`, new session).
	pub refresh_commands: bool,
}

impl SessionModel {
	pub fn busy(&self) -> bool {
		self.activity != Activity::Idle
	}

	/// Marks a prompt as sent, before pi accepts it.
	pub fn prompt_sent(&mut self) {
		self.activity = Activity::Running;
	}

	pub fn apply(&mut self, notification: Notification) -> Effects {
		let mut effects = Effects::default();
		match &notification {
			Notification::AgentStart => self.activity = Activity::Running,
			Notification::AgentSettled { .. } => {
				if self.activity == Activity::Running {
					self.activity = Activity::Idle;
				}
				self.retry = None;
				effects.refresh_state = true;
			},
			Notification::TurnEnd | Notification::AgentEnd { .. } => effects.refresh_state = true,
			Notification::ThinkingLevelChanged { level } => {
				if let Some(state) = &mut self.state {
					state.thinking_level = *level;
				}
			},
			Notification::SessionInfoChanged { name } => {
				if let Some(state) = &mut self.state {
					state.session_name = name.clone();
				}
			},
			Notification::CompactionStart { .. } => self.compacting = true,
			Notification::CompactionEnd { reason, aborted, error_message } => {
				self.compacting = false;
				effects.refresh_state = true;
				// A manual compaction reports its failure through the command reply.
				if let Some(error) = error_message
					&& !aborted
					&& reason.as_deref() != Some("manual")
				{
					self.notices.push(Notice::Error(format!("自动压缩失败：{error}")));
				}
			},
			Notification::AutoRetryStart { attempt, max_attempts, error_message } => {
				self.retry = Some((*attempt, *max_attempts));
				if let Some(error) = error_message {
					self.notices.push(Notice::Info(format!("重试 {attempt}/{max_attempts}：{error}")));
				}
			},
			Notification::AutoRetryEnd { success, final_error } => {
				self.retry = None;
				if !success && let Some(error) = final_error {
					self.notices.push(Notice::Error(format!("重试失败：{error}")));
				}
			},
			Notification::QueueUpdate { steering, follow_up } => {
				self.queue = Queue { steering: steering.clone(), follow_up: follow_up.clone() };
				#[cfg(test)]
				self.queue_log.push(self.queue.clone());
			},
			Notification::BashExecutionUpdate { delta, .. } => {
				if let Some(row) = self.shell_row
					&& self.transcript.append_bash_output(row, delta)
				{
					effects.rows = Some((row, self.transcript.entries().len()));
				}
			},
			Notification::ExtensionError { extension_path, error } => {
				self.notices.push(Notice::Error(format!("扩展 {extension_path}：{error}")));
			},
			Notification::Ui(request) => self.apply_ui(request.clone()),
			_ => {},
		}
		let rows = self.transcript.entries().len();
		if self.tools.apply(&notification)
			&& let Some(id) = ToolRuns::event_id(&notification)
			&& let Some(&row) = self.tool_rows.get(id)
		{
			effects.rows = Some((row, rows));
		}
		if let Some(change) = self.transcript.apply(&notification) {
			let index = change.index();
			for id in tool_call_ids(&self.transcript.entries()[index].message) {
				self.tool_rows.insert(id, index);
			}
			let first = effects.rows.map_or(index, |(row, _)| row.min(index));
			effects.rows = Some((first.min(rows), self.transcript.entries().len()));
		}
		effects
	}

	fn apply_ui(&mut self, request: UiRequest) {
		match request {
			UiRequest::Notify { message, notify_type } => self.notices.push(match notify_type.as_deref() {
				Some("error") => Notice::Error(message),
				_ => Notice::Info(message),
			}),
			UiRequest::SetStatus { status_key, status_text } => match status_text.filter(|t| !t.trim().is_empty()) {
				Some(text) => _ = self.statuses.insert(status_key, text),
				None => _ = self.statuses.remove(&status_key),
			},
			UiRequest::SetWidget { widget_key, widget_lines, widget_placement } => match widget_lines {
				Some(lines) if !lines.is_empty() => {
					let below = widget_placement.as_deref() == Some("belowEditor");
					self.widgets.insert(widget_key, Widget { lines, below });
				},
				_ => _ = self.widgets.remove(&widget_key),
			},
			UiRequest::SetTitle { title } => self.title = Some(title).filter(|t| !t.trim().is_empty()),
			UiRequest::SetEditorText { text } => self.editor_text = Some(text),
			request @ (UiRequest::Select { .. } | UiRequest::Confirm { .. } | UiRequest::Input { .. } | UiRequest::Editor { .. }) => {
				self.dialogs.push(request);
			},
			UiRequest::Other => {},
		}
	}

	/// Removes an answered (or timed-out) dialog.
	pub fn dialog_answered(&mut self, id: &str) {
		self.dialogs.retain(|dialog| dialog.dialog_id() != Some(id));
	}

	/// Number of rows the conversation shows (hidden messages included; the
	/// row renderer collapses them).
	pub fn row_count(&self) -> usize {
		self.transcript.entries().len()
	}

	/// Replaces the conversation with a resumed session's history. Each
	/// message goes through `message_end`, so tool results attach to their
	/// cards exactly as they do live.
	pub fn load_history(&mut self, messages: Vec<AgentMessage>) {
		self.transcript = Transcript::default();
		self.tools = Default::default();
		self.tool_rows.clear();
		self.shell_row = None;
		for message in messages {
			self.apply(Notification::MessageEnd(message));
		}
	}

	/// Starts a `!cmd` row; output streams in through `bash_execution_update`.
	pub fn shell_started(&mut self, command: &str, exclude_from_context: bool) -> Effects {
		let row = self.transcript.push_local(AgentMessage::BashExecution(BashExecutionMessage {
			command: command.to_owned(),
			exclude_from_context,
			..Default::default()
		}));
		self.shell_row = Some(row);
		self.activity = Activity::Shell;
		Effects { rows: Some((row, self.transcript.entries().len())), ..Default::default() }
	}

	/// Finishes the `!cmd` row with pi's result (`None` when the call failed).
	pub fn shell_finished(&mut self, result: Option<pi_rpc::BashResult>) -> Effects {
		if self.activity == Activity::Shell {
			self.activity = Activity::Idle;
		}
		let Some(row) = self.shell_row.take() else { return Effects::default() };
		if let Some(result) = result {
			self.transcript.finish_bash(row, &result);
		} else {
			self.transcript.finish_bash(row, &pi_rpc::BashResult { cancelled: true, ..Default::default() });
		}
		Effects { rows: Some((row, self.transcript.entries().len())), refresh_state: true, ..Default::default() }
	}

	/// Whether a dialog waits for the user (the tab shows a warning dot).
	pub fn needs_attention(&self) -> bool {
		!self.dialogs.is_empty()
	}
}

fn tool_call_ids(message: &AgentMessage) -> Vec<String> {
	let AgentMessage::Assistant(message) = message else {
		return Vec::new();
	};
	message
		.content
		.iter()
		.filter_map(|block| match block {
			AssistantContent::ToolCall(call) if !call.id.is_empty() => Some(call.id.clone()),
			_ => None,
		})
		.collect()
}

#[cfg(test)]
mod tests {
	use serde_json::{Value, json};

	use super::*;

	fn notify(model: &mut SessionModel, value: Value) -> Effects {
		model.apply(Notification::from_value(value))
	}

	#[::core::prelude::v1::test]
	fn run_lifecycle_tracks_activity() {
		let mut m = SessionModel::default();
		m.prompt_sent();
		assert!(m.busy());
		notify(&mut m, json!({"type":"agent_start"}));
		notify(&mut m, json!({"type":"agent_end","messages":[],"willRetry":false}));
		assert!(m.busy(), "agent_end is not the end of the run");
		let effects = notify(&mut m, json!({"type":"agent_settled","aborted":false}));
		assert!(!m.busy());
		assert!(effects.refresh_state);
	}

	#[::core::prelude::v1::test]
	fn dialogs_queue_and_ui_records_fold() {
		let mut m = SessionModel::default();
		notify(&mut m, json!({"type":"extension_ui_request","id":"a","method":"confirm","title":"Run?","message":"rm"}));
		notify(&mut m, json!({"type":"extension_ui_request","id":"b","method":"select","title":"Pick","options":["x","y"]}));
		assert_eq!(m.dialogs.len(), 2);
		m.dialog_answered("a");
		assert_eq!(m.dialogs.iter().filter_map(UiRequest::dialog_id).collect::<Vec<_>>(), ["b"]);

		notify(&mut m, json!({"type":"extension_ui_request","id":"s","method":"setStatus","statusKey":"usage","statusText":"5h 42%"}));
		notify(&mut m, json!({"type":"extension_ui_request","id":"s","method":"setStatus","statusKey":"goal"}));
		assert_eq!(m.statuses.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect::<Vec<_>>(), [("usage", "5h 42%")]);
		notify(&mut m, json!({"type":"extension_ui_request","id":"s","method":"setStatus","statusKey":"usage"}));
		assert!(m.statuses.is_empty(), "an absent text clears the entry");

		notify(&mut m, json!({"type":"extension_ui_request","id":"w","method":"setWidget","widgetKey":"k","widgetLines":["a","b"],"widgetPlacement":"belowEditor"}));
		assert_eq!(m.widgets["k"], Widget { lines: vec!["a".into(), "b".into()], below: true });
		notify(&mut m, json!({"type":"extension_ui_request","id":"w","method":"setWidget","widgetKey":"k"}));
		assert!(m.widgets.is_empty());

		notify(&mut m, json!({"type":"extension_ui_request","id":"n","method":"notify","message":"blocked","notifyType":"error"}));
		assert_eq!(m.notices, [Notice::Error("blocked".into())]);
		notify(&mut m, json!({"type":"extension_ui_request","id":"t","method":"set_editor_text","text":"draft"}));
		assert_eq!(m.editor_text.as_deref(), Some("draft"));
	}

	#[::core::prelude::v1::test]
	fn tool_output_refreshes_the_row_that_issued_the_call() {
		let mut m = SessionModel::default();
		notify(&mut m, json!({"type":"message_end","message":{"role":"user","content":"go"}}));
		notify(&mut m, json!({"type":"message_end","message":{"role":"assistant",
			"content":[{"type":"toolCall","id":"c1","name":"bash","arguments":{"command":"ls"}}]}}));
		notify(&mut m, json!({"type":"message_end","message":{"role":"user","content":"next"}}));
		notify(&mut m, json!({"type":"tool_execution_start","toolCallId":"c1","toolName":"bash","args":{}}));
		let effects = notify(&mut m, json!({"type":"tool_execution_update","toolCallId":"c1","toolName":"bash",
			"args":{},"partialResult":{"content":[{"type":"text","text":"a.rs"}]}}));
		assert_eq!(effects.rows, Some((1, 3)));
		assert_eq!(m.tools.get("c1").unwrap().output, "a.rs");
	}

	#[::core::prelude::v1::test]
	fn history_attaches_tool_results_to_their_call() {
		let mut m = SessionModel::default();
		let messages = [
			json!({"role":"user","content":[{"type":"text","text":"ls"}],"timestamp":1}),
			json!({"role":"assistant","content":[{"type":"toolCall","id":"c1","name":"bash","arguments":{"command":"ls"}}],"timestamp":2}),
			json!({"role":"toolResult","toolCallId":"c1","toolName":"bash","content":[{"type":"text","text":"a.rs"}],"isError":false,"timestamp":3}),
		]
		.map(AgentMessage::from_value);
		m.load_history(messages.to_vec());
		assert_eq!(m.row_count(), 3);
		assert_eq!(m.tool_rows.get("c1"), Some(&1));
		assert_eq!(m.tools.get("c1").unwrap().output, "a.rs");
		assert!(!m.busy());
		m.load_history(messages[..1].to_vec());
		assert_eq!(m.row_count(), 1, "reloading replaces rather than appends");
		assert!(m.tools.get("c1").is_none());
	}

	#[::core::prelude::v1::test]
	fn shell_command_streams_into_its_row() {
		let mut m = SessionModel::default();
		let effects = m.shell_started("printf ab", false);
		assert_eq!(effects.rows, Some((0, 1)));
		assert!(m.busy());
		notify(&mut m, json!({"type":"bash_execution_update","id":"d1","delta":"a"}));
		let effects = notify(&mut m, json!({"type":"bash_execution_update","id":"d1","delta":"b"}));
		assert_eq!(effects.rows, Some((0, 1)));
		let AgentMessage::BashExecution(bash) = &m.transcript.entries()[0].message else { panic!() };
		assert_eq!(bash.output, "ab");
		m.shell_finished(Some(pi_rpc::BashResult { output: "ab".into(), exit_code: Some(0), ..Default::default() }));
		assert!(!m.busy());
		let AgentMessage::BashExecution(bash) = &m.transcript.entries()[0].message else { panic!() };
		assert_eq!(bash.exit_code, Some(0));
	}

	#[::core::prelude::v1::test]
	fn queue_edits_follow_messages_that_pi_delivered_meanwhile() {
		let q = |steering: &[&str], follow_up: &[&str]| Queue {
			steering:  steering.iter().map(|s| (*s).to_owned()).collect(),
			follow_up: follow_up.iter().map(|s| (*s).to_owned()).collect(),
		};
		let shown = q(&["s1"], &["a", "b", "a"]);
		assert_eq!(shown.edit(&shown, QueueEdit::Remove { steering: false, index: 2 }), Some(q(&["s1"], &["a", "b"])));
		assert_eq!(shown.edit(&shown, QueueEdit::Remove { steering: true, index: 0 }), Some(q(&[], &["a", "b", "a"])));
		assert_eq!(shown.edit(&shown, QueueEdit::Promote { index: 1 }), Some(q(&["s1", "b"], &["a", "a"])));
		// pi delivered the steer and the first follow-up before the click landed.
		let now = q(&[], &["b", "a"]);
		assert_eq!(now.edit(&shown, QueueEdit::Promote { index: 1 }), Some(q(&["b"], &["a"])));
		assert_eq!(now.edit(&shown, QueueEdit::Remove { steering: false, index: 2 }), Some(q(&[], &["b"])), "the last \"a\" is still queued");
		assert_eq!(now.edit(&shown, QueueEdit::Remove { steering: false, index: 0 }), None, "the first \"a\" was delivered");
		assert_eq!(now.edit(&shown, QueueEdit::Remove { steering: true, index: 0 }), None);
		assert_eq!(shown.edit(&shown, QueueEdit::Remove { steering: false, index: 9 }), None);
	}

	#[::core::prelude::v1::test]
	fn compaction_retry_and_state_events() {
		let mut m = SessionModel::default();
		m.state = Some(serde_json::from_value(json!({"sessionId":"s","thinkingLevel":"high"})).unwrap());
		notify(&mut m, json!({"type":"compaction_start","reason":"manual"}));
		assert!(m.compacting);
		let effects = notify(&mut m, json!({"type":"compaction_end","reason":"manual","aborted":false,"willRetry":false,"errorMessage":"Nothing to compact"}));
		assert!(!m.compacting);
		assert!(effects.refresh_state);
		assert!(m.notices.is_empty(), "manual failures are reported by the command reply");
		notify(&mut m, json!({"type":"compaction_end","reason":"threshold","aborted":false,"errorMessage":"boom"}));
		assert_eq!(m.notices, [Notice::Error("自动压缩失败：boom".into())]);

		notify(&mut m, json!({"type":"auto_retry_start","attempt":1,"maxAttempts":3,"delayMs":2000}));
		assert_eq!(m.retry, Some((1, 3)));
		notify(&mut m, json!({"type":"auto_retry_end","success":true,"attempt":2}));
		assert_eq!(m.retry, None);

		notify(&mut m, json!({"type":"thinking_level_changed","level":"low"}));
		assert_eq!(m.state.as_ref().unwrap().thinking_level, Some(pi_rpc::ThinkingLevel::Low));
		notify(&mut m, json!({"type":"session_info_changed","name":"新标题"}));
		assert_eq!(m.state.as_ref().unwrap().session_name.as_deref(), Some("新标题"));
		notify(&mut m, json!({"type":"queue_update","steering":["s"],"followUp":["f"]}));
		assert_eq!(m.queue, Queue { steering: vec!["s".into()], follow_up: vec!["f".into()] });
	}
}
