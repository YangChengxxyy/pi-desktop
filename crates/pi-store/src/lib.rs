//! UI-independent conversation state rebuilt from pi RPC session events.
//!
//! [`Transcript`] consumes `message_start` / `message_update` / `message_end`.
//! pi's wire `message_update` is delta-only: `*_start` opens an empty block
//! at `contentIndex`, deltas append, `*_end` carries the block's final
//! content. `message_end` is authoritative and replaces whatever the stream
//! produced. pi sends no message ids, so updates and ends apply to the newest
//! message still streaming.

pub mod display;
pub mod sessions;
pub mod tools;

use std::collections::HashMap;

use pi_rpc::{
	AgentMessage, AssistantContent, AssistantMessage, AssistantMessageEvent, BashResult, Content, CustomMessage, Notification,
	TextContent, ThinkingContent, ToolCall,
};

/// `customType` of the local rows that hold desktop command output.
pub const COMMAND_OUTPUT: &str = "pi-desktop/command-output";

/// One message in display order.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
	/// A local id, stable for the life of the transcript.
	pub id:        String,
	pub message:   AgentMessage,
	/// Between `message_start` and `message_end`.
	pub streaming: bool,
	/// Raw argument JSON streamed so far for tool calls that have not ended,
	/// keyed by content index. Complete arguments arrive in `toolcall_end`.
	pub tool_args: HashMap<usize, String>,
}

/// What an applied event changed, for incremental rendering.
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
	Started { index: usize },
	Updated { index: usize },
	/// `streamed` is the message as rebuilt from the stream, just before
	/// `message_end` replaced it (`None` when the message had not started).
	Ended { index: usize, streamed: Option<AgentMessage> },
}

impl Change {
	pub fn index(&self) -> usize {
		match self {
			Self::Started { index } | Self::Updated { index } | Self::Ended { index, .. } => *index,
		}
	}
}

#[derive(Debug, Default)]
pub struct Transcript {
	entries:    Vec<Entry>,
	next_local: u64,
}

impl Transcript {
	pub fn new() -> Self {
		Self::default()
	}

	pub fn entries(&self) -> &[Entry] {
		&self.entries
	}

	/// Applies one record; records other than message lifecycle return
	/// `None`.
	pub fn apply(&mut self, notification: &Notification) -> Option<Change> {
		match notification {
			Notification::MessageStart(message) => Some(self.start(message)),
			Notification::MessageUpdate(event) => self.update(event),
			Notification::MessageEnd(message) => Some(self.end(message)),
			_ => None,
		}
	}

	fn start(&mut self, message: &AgentMessage) -> Change {
		let id = self.local_id();
		self.entries.push(Entry { id, message: message.clone(), streaming: true, tool_args: HashMap::new() });
		Change::Started { index: self.entries.len() - 1 }
	}

	fn update(&mut self, event: &AssistantMessageEvent) -> Option<Change> {
		let index = self.streaming()?;
		let entry = &mut self.entries[index];
		let AgentMessage::Assistant(message) = &mut entry.message else {
			return None;
		};
		apply_delta(message, &mut entry.tool_args, event);
		Some(Change::Updated { index })
	}

	fn end(&mut self, message: &AgentMessage) -> Change {
		match self.streaming() {
			Some(index) => {
				let entry = &mut self.entries[index];
				let streamed = std::mem::replace(&mut entry.message, message.clone());
				entry.streaming = false;
				entry.tool_args.clear();
				Change::Ended { index, streamed: Some(streamed) }
			},
			None => {
				let id = self.local_id();
				self.entries.push(Entry { id, message: message.clone(), streaming: false, tool_args: HashMap::new() });
				Change::Ended { index: self.entries.len() - 1, streamed: None }
			},
		}
	}

	/// The newest entry still streaming.
	fn streaming(&self) -> Option<usize> {
		self.entries.iter().rposition(|entry| entry.streaming)
	}

	/// Marks every streaming entry finished (the process went away).
	pub fn stop_streaming(&mut self) {
		for entry in &mut self.entries {
			entry.streaming = false;
			entry.tool_args.clear();
		}
	}

	/// Appends a local row of command output; returns its index.
	pub fn push_command_output(&mut self, text: &str) -> usize {
		self.push_local(AgentMessage::Custom(CustomMessage {
			custom_type: COMMAND_OUTPUT.to_owned(),
			content:     Content::Text(text.to_owned()),
			display:     true,
		}))
	}

	/// Appends a finished row the client made itself; returns its index.
	pub fn push_local(&mut self, message: AgentMessage) -> usize {
		let id = self.local_id();
		self.entries.push(Entry { id, message, streaming: false, tool_args: HashMap::new() });
		self.entries.len() - 1
	}

	/// Appends shell output to the `bashExecution` row at `index`; returns
	/// whether the row is one.
	pub fn append_bash_output(&mut self, index: usize, delta: &str) -> bool {
		match self.entries.get_mut(index).map(|entry| &mut entry.message) {
			Some(AgentMessage::BashExecution(bash)) => {
				bash.output.push_str(delta);
				true
			},
			_ => false,
		}
	}

	/// Stores pi's final result on the `bashExecution` row at `index`.
	pub fn finish_bash(&mut self, index: usize, result: &BashResult) {
		if let Some(AgentMessage::BashExecution(bash)) = self.entries.get_mut(index).map(|entry| &mut entry.message) {
			// The response can be truncated; the streamed output is complete.
			if bash.output.is_empty() {
				bash.output = result.output.clone();
			}
			bash.exit_code = result.exit_code;
			bash.cancelled = result.cancelled;
			bash.truncated = result.truncated;
		}
	}

	fn local_id(&mut self) -> String {
		self.next_local += 1;
		format!("local-{}", self.next_local)
	}
}

fn apply_delta(message: &mut AssistantMessage, tool_args: &mut HashMap<usize, String>, event: &AssistantMessageEvent) {
	use AssistantMessageEvent as E;
	match event {
		E::TextStart { content_index } => put(message, *content_index, AssistantContent::Text(TextContent::default())),
		E::ThinkingStart { content_index } => {
			put(message, *content_index, AssistantContent::Thinking(ThinkingContent::default()));
		},
		E::ToolcallStart { content_index, id, tool_name } => {
			let call = ToolCall { id: id.clone(), name: tool_name.clone(), arguments: Default::default() };
			put(message, *content_index, AssistantContent::ToolCall(call));
		},
		E::TextDelta { content_index, delta } => match message.content.get_mut(*content_index) {
			Some(AssistantContent::Text(block)) => block.text.push_str(delta),
			_ => put(message, *content_index, AssistantContent::Text(TextContent { text: delta.clone() })),
		},
		E::ThinkingDelta { content_index, delta } => match message.content.get_mut(*content_index) {
			Some(AssistantContent::Thinking(block)) => block.thinking.push_str(delta),
			_ => put(
				message,
				*content_index,
				AssistantContent::Thinking(ThinkingContent { thinking: delta.clone(), redacted: false }),
			),
		},
		E::TextEnd { content_index, content } => {
			put(message, *content_index, AssistantContent::Text(TextContent { text: content.clone() }));
		},
		E::ThinkingEnd { content_index, content } => {
			put(message, *content_index, AssistantContent::Thinking(ThinkingContent { thinking: content.clone(), redacted: false }));
		},
		E::ToolcallDelta { content_index, delta } => tool_args.entry(*content_index).or_default().push_str(delta),
		E::ToolcallEnd { content_index, tool_call } => {
			put(message, *content_index, AssistantContent::ToolCall(tool_call.clone()));
			tool_args.remove(content_index);
		},
		E::Other => {},
	}
}

/// Stores `block` at `index`, padding with empty text blocks if the stream
/// skipped ahead.
fn put(message: &mut AssistantMessage, index: usize, block: AssistantContent) {
	if index >= message.content.len() {
		message.content.resize_with(index + 1, || AssistantContent::Text(TextContent::default()));
	}
	message.content[index] = block;
}

#[cfg(test)]
mod tests {
	use serde_json::{Value, json};

	use super::*;

	fn record(value: Value) -> Notification {
		Notification::from_value(value)
	}

	fn update(event: Value) -> Notification {
		record(json!({"type": "message_update", "usage": {}, "assistantMessageEvent": event}))
	}

	fn blocks(message: &AgentMessage) -> Vec<String> {
		let AgentMessage::Assistant(message) = message else { panic!("not assistant") };
		message
			.content
			.iter()
			.map(|block| match block {
				AssistantContent::Text(t) => format!("text:{}", t.text),
				AssistantContent::Thinking(t) => format!("thinking:{}", t.thinking),
				AssistantContent::ToolCall(c) => format!("tool:{}:{}", c.name, Value::Object(c.arguments.clone())),
				AssistantContent::Other => "other".into(),
			})
			.collect()
	}

	/// The record sequence pi 1.5 sent for one turn (thinking, two parallel
	/// tool calls), trimmed.
	#[test]
	fn delta_stream_rebuilds_the_message_and_end_replaces_it() {
		let mut t = Transcript::new();
		t.apply(&record(json!({"type":"message_start","message":{"role":"user","content":[{"type":"text","text":"go"}]}})));
		t.apply(&record(json!({"type":"message_end","message":{"role":"user","content":[{"type":"text","text":"go"}]}})));
		t.apply(&record(json!({"type":"message_start","message":{"role":"assistant","content":[],"stopReason":"pending"}})));
		for event in [
			json!({"type":"thinking_start","contentIndex":0}),
			json!({"type":"thinking_delta","contentIndex":0,"delta":"plan "}),
			json!({"type":"thinking_delta","contentIndex":0,"delta":"it"}),
			json!({"type":"toolcall_start","contentIndex":1,"id":"bash:0","toolName":"bash"}),
			json!({"type":"toolcall_delta","contentIndex":1,"delta":"{\"command\":"}),
			json!({"type":"toolcall_start","contentIndex":2,"id":"read:1","toolName":"read"}),
		] {
			t.apply(&update(event));
		}
		let entry = &t.entries()[1];
		assert!(entry.streaming);
		assert_eq!(entry.tool_args.get(&1).map(String::as_str), Some("{\"command\":"));
		assert_eq!(blocks(&entry.message), ["thinking:plan it", "tool:bash:{}", "tool:read:{}"]);

		t.apply(&update(json!({"type":"thinking_end","contentIndex":0,"content":"plan it."})));
		t.apply(&update(json!({"type":"toolcall_end","contentIndex":1,"toolCall":{"type":"toolCall","id":"bash:0","name":"bash","arguments":{"command":"ls"}}})));
		assert!(t.entries()[1].tool_args.is_empty());
		assert_eq!(blocks(&t.entries()[1].message)[..2], ["thinking:plan it.", r#"tool:bash:{"command":"ls"}"#]);

		let change = t.apply(&record(json!({"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"final"}],"stopReason":"stop"}})));
		let Some(Change::Ended { index: 1, streamed: Some(streamed) }) = change else { panic!("{change:?}") };
		assert_eq!(blocks(&streamed).len(), 3);
		assert!(!t.entries()[1].streaming);
		assert_eq!(blocks(&t.entries()[1].message), ["text:final"]);
	}

	#[test]
	fn end_without_start_appends_and_text_skips_ahead() {
		let mut t = Transcript::new();
		let change = t.apply(&record(json!({"type":"message_end","message":{"role":"toolResult","toolCallId":"c","toolName":"read","content":[],"isError":false}})));
		assert_eq!(change, Some(Change::Ended { index: 0, streamed: None }));

		t.apply(&record(json!({"type":"message_start","message":{"role":"assistant","content":[]}})));
		t.apply(&update(json!({"type":"text_delta","contentIndex":2,"delta":"x"})));
		assert_eq!(blocks(&t.entries()[1].message), ["text:", "text:", "text:x"]);
	}

	#[test]
	fn local_command_rows_do_not_capture_streaming_updates() {
		let mut t = Transcript::new();
		t.apply(&record(json!({"type":"message_start","message":{"role":"assistant","content":[]}})));
		let row = t.push_command_output("$ ls");
		t.apply(&update(json!({"type":"text_start","contentIndex":0})));
		t.apply(&update(json!({"type":"text_delta","contentIndex":0,"delta":"hi"})));
		assert_eq!(row, 1);
		assert_eq!(blocks(&t.entries()[0].message), ["text:hi"]);
	}
}
