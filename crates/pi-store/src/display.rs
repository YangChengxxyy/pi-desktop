//! Display projection: turns wire messages into the blocks a view renders,
//! so UI code never matches on wire unions.

use pi_rpc::{AgentMessage, AssistantContent, Content, ImageContent, UserContent};
use serde_json::{Map, Value};

use crate::Entry;

#[derive(Debug, Clone, PartialEq)]
pub enum Role {
	User,
	Assistant,
	/// A tool's result, attached under the call that produced it.
	ToolResult { tool_call_id: String, tool_name: String, is_error: bool },
	/// A direct shell command (`!cmd` from the composer) and its output.
	Bash { command: String, exit_code: Option<i64>, cancelled: bool, excluded: bool },
	/// Desktop-local command output, or a displayed extension message.
	Command,
	/// Context summaries: compaction and branch summaries.
	Summary { title: &'static str },
	/// Messages the conversation view does not render (system prompts,
	/// hidden extension messages, unknown roles).
	Hidden,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Block {
	/// Markdown source.
	Text(String),
	Thinking(String),
	/// `arguments` is the parsed object once `toolcall_end` arrived, else the
	/// raw JSON fragment streamed so far.
	ToolCall { id: String, name: String, arguments: ToolArgs },
	/// `data` is the base64 payload.
	Image { mime_type: String, data: String },
	/// Plain text output (tool results, shell output).
	Output(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum ToolArgs {
	Complete(Map<String, Value>),
	Partial(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct View {
	pub role:      Role,
	pub blocks:    Vec<Block>,
	pub streaming: bool,
	/// Provider error text for a failed assistant turn.
	pub error:     Option<String>,
}

impl Entry {
	pub fn view(&self) -> View {
		let mut view = View { role: Role::Hidden, blocks: Vec::new(), streaming: self.streaming, error: None };
		match &self.message {
			AgentMessage::User(user) => {
				view.role = Role::User;
				view.blocks.extend(user.content.parts().iter().filter_map(user_block));
			},
			AgentMessage::Custom(custom) if custom.custom_type == crate::COMMAND_OUTPUT || custom.display => {
				view.role = Role::Command;
				let text = custom.content.text();
				if !text.is_empty() {
					view.blocks.push(Block::Output(text));
				}
			},
			AgentMessage::Assistant(message) => {
				view.role = Role::Assistant;
				// An aborted turn reports "aborted" as its error; that is not a failure.
				view.error = message.error_message.clone().filter(|_| message.stop_reason.as_deref() != Some("aborted"));
				for (index, block) in message.content.iter().enumerate() {
					let block = match block {
						AssistantContent::Text(t) if t.text.is_empty() && !self.streaming => continue,
						AssistantContent::Text(t) => Block::Text(t.text.clone()),
						AssistantContent::Thinking(t) if t.thinking.is_empty() && !self.streaming => continue,
						AssistantContent::Thinking(t) => Block::Thinking(t.thinking.clone()),
						AssistantContent::ToolCall(call) => Block::ToolCall {
							id:        call.id.clone(),
							name:      call.name.clone(),
							arguments: match self.tool_args.get(&index) {
								Some(partial) => ToolArgs::Partial(partial.clone()),
								None => ToolArgs::Complete(call.arguments.clone()),
							},
						},
						AssistantContent::Other => continue,
					};
					view.blocks.push(block);
				}
			},
			AgentMessage::ToolResult(result) => {
				view.role = Role::ToolResult {
					tool_call_id: result.tool_call_id.clone(),
					tool_name:    result.tool_name.clone(),
					is_error:     result.is_error,
				};
				view.blocks.extend(result.content.iter().filter_map(|part| match user_block(part)? {
					Block::Text(text) => Some(Block::Output(text)),
					other => Some(other),
				}));
			},
			AgentMessage::BashExecution(bash) => {
				view.role = Role::Bash {
					command:   bash.command.clone(),
					exit_code: bash.exit_code,
					cancelled: bash.cancelled,
					excluded:  bash.exclude_from_context,
				};
				view.blocks.push(Block::Output(bash.output.clone()));
			},
			AgentMessage::CompactionSummary(summary) => {
				view.role = Role::Summary { title: "上下文已压缩" };
				view.blocks.push(Block::Text(summary.summary.clone()));
			},
			AgentMessage::BranchSummary(summary) => {
				view.role = Role::Summary { title: "分支摘要" };
				view.blocks.push(Block::Text(summary.summary.clone()));
			},
			AgentMessage::Custom(_) | AgentMessage::Other { .. } => {},
		}
		view
	}
}

fn user_block(part: &UserContent) -> Option<Block> {
	match part {
		UserContent::Text(t) => Some(Block::Text(t.text.clone())),
		UserContent::Image(image) => Some(image_block(image)),
		UserContent::Other => None,
	}
}

fn image_block(image: &ImageContent) -> Block {
	Block::Image { mime_type: image.mime_type.clone(), data: image.data.clone() }
}

/// Plain text of a user message, for titles and tests.
pub fn user_text(message: &AgentMessage) -> Option<String> {
	match message {
		AgentMessage::User(user) => Some(match &user.content {
			Content::Text(text) => text.clone(),
			content => content.text(),
		}),
		_ => None,
	}
}

#[cfg(test)]
mod tests {
	use pi_rpc::Notification;
	use serde_json::json;

	use super::*;
	use crate::Transcript;

	fn apply(t: &mut Transcript, value: Value) {
		t.apply(&Notification::from_value(value));
	}

	fn update(t: &mut Transcript, event: Value) {
		apply(t, json!({"type":"message_update","usage":{},"assistantMessageEvent": event}));
	}

	#[test]
	fn streaming_tool_call_shows_partial_then_complete_arguments() {
		let mut t = Transcript::new();
		apply(&mut t, json!({"type":"message_start","message":{"role":"assistant","content":[]}}));
		update(&mut t, json!({"type":"toolcall_start","contentIndex":0,"id":"c","toolName":"read"}));
		update(&mut t, json!({"type":"toolcall_delta","contentIndex":0,"delta":"{\"pa"}));
		let view = t.entries()[0].view();
		assert!(view.streaming);
		assert!(matches!(&view.blocks[..], [Block::ToolCall { arguments: ToolArgs::Partial(p), .. }] if p == "{\"pa"));

		update(&mut t, json!({"type":"toolcall_end","contentIndex":0,"toolCall":{"type":"toolCall","id":"c","name":"read","arguments":{"path":"x"}}}));
		let view = t.entries()[0].view();
		assert!(matches!(&view.blocks[..], [Block::ToolCall { arguments: ToolArgs::Complete(a), .. }] if a["path"] == "x"));
	}

	#[test]
	fn roles_and_hidden_messages() {
		let mut t = Transcript::new();
		for message in [
			json!({"role":"user","content":"hi"}),
			json!({"role":"system","content":"","sections":{}}),
			json!({"role":"toolResult","toolCallId":"c","toolName":"read","content":[{"type":"text","text":"file"}],"isError":true}),
			json!({"role":"bashExecution","command":"ls","output":"a\n","exitCode":0,"cancelled":false,"truncated":false}),
			json!({"role":"custom","customType":"x","content":"shown","display":true}),
			json!({"role":"custom","customType":"x","content":"hidden","display":false}),
			json!({"role":"compactionSummary","summary":"S","tokensBefore":10}),
			json!({"role":"assistant","content":[{"type":"text","text":""}],"stopReason":"aborted","errorMessage":"Request was aborted"}),
		] {
			apply(&mut t, json!({"type":"message_end","message":message}));
		}
		let views: Vec<_> = t.entries().iter().map(Entry::view).collect();
		assert_eq!((views[0].role.clone(), views[0].blocks.clone()), (Role::User, vec![Block::Text("hi".into())]));
		assert_eq!(views[1].role, Role::Hidden);
		assert_eq!(views[2].role, Role::ToolResult { tool_call_id: "c".into(), tool_name: "read".into(), is_error: true });
		assert_eq!(views[2].blocks, [Block::Output("file".into())]);
		assert_eq!(views[3].role, Role::Bash { command: "ls".into(), exit_code: Some(0), cancelled: false, excluded: false });
		assert_eq!(views[4].role, Role::Command);
		assert_eq!(views[5].role, Role::Hidden);
		assert_eq!(views[6].role, Role::Summary { title: "上下文已压缩" });
		assert_eq!((views[7].blocks.len(), views[7].error.clone()), (0, None), "an abort is not an error");
	}
}
