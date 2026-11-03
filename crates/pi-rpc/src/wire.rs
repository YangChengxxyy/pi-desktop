//! Wire types for `pi --mode rpc` (pi ≥ 1.5): messages, session events,
//! extension UI requests, and command payloads.
//!
//! Decoding is lenient: unknown message roles, content blocks, events, and
//! UI methods decode to `Other`/`Unknown` variants instead of failing, so a
//! newer pi does not break the client.

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

// ---------------------------------------------------------------- content

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TextContent {
	#[serde(default)]
	pub text: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageContent {
	#[serde(default)]
	pub data:      String,
	#[serde(default)]
	pub mime_type: String,
}

impl ImageContent {
	/// The `{"type":"image",...}` object `prompt` takes.
	pub fn to_wire(&self) -> Value {
		serde_json::json!({"type": "image", "data": self.data, "mimeType": self.mime_type})
	}
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ThinkingContent {
	#[serde(default)]
	pub thinking: String,
	#[serde(default)]
	pub redacted: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
	#[serde(default)]
	pub id:        String,
	#[serde(default)]
	pub name:      String,
	#[serde(default)]
	pub arguments: Map<String, Value>,
}

/// User and tool-result content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum UserContent {
	Text(TextContent),
	Image(ImageContent),
	#[serde(other)]
	Other,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum AssistantContent {
	Text(TextContent),
	Thinking(ThinkingContent),
	ToolCall(ToolCall),
	#[serde(other)]
	Other,
}

/// `string | (TextContent | ImageContent)[]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Content {
	Text(String),
	Parts(Vec<UserContent>),
}

impl Default for Content {
	fn default() -> Self {
		Self::Text(String::new())
	}
}

impl Content {
	pub fn parts(&self) -> Vec<UserContent> {
		match self {
			Self::Text(text) => vec![UserContent::Text(TextContent { text: text.clone() })],
			Self::Parts(parts) => parts.clone(),
		}
	}

	/// Text blocks joined with newlines.
	pub fn text(&self) -> String {
		match self {
			Self::Text(text) => text.clone(),
			Self::Parts(parts) => parts
				.iter()
				.filter_map(|part| match part {
					UserContent::Text(t) => Some(t.text.as_str()),
					_ => None,
				})
				.collect::<Vec<_>>()
				.join("\n"),
		}
	}
}

// ---------------------------------------------------------------- messages

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
	#[serde(default)]
	pub input:        i64,
	#[serde(default)]
	pub output:       i64,
	#[serde(default)]
	pub cache_read:   i64,
	#[serde(default)]
	pub cache_write:  i64,
	#[serde(default)]
	pub total_tokens: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserMessage {
	#[serde(default)]
	pub content:   Content,
	#[serde(default)]
	pub timestamp: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantMessage {
	#[serde(default)]
	pub content:       Vec<AssistantContent>,
	#[serde(default)]
	pub provider:      Option<String>,
	#[serde(default)]
	pub model:         Option<String>,
	#[serde(default)]
	pub usage:         Option<Usage>,
	#[serde(default)]
	pub stop_reason:   Option<String>,
	#[serde(default)]
	pub error_message: Option<String>,
	#[serde(default)]
	pub timestamp:     Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultMessage {
	#[serde(default)]
	pub tool_call_id: String,
	#[serde(default)]
	pub tool_name:    String,
	#[serde(default)]
	pub content:      Vec<UserContent>,
	#[serde(default)]
	pub details:      Option<Value>,
	#[serde(default)]
	pub is_error:     bool,
	#[serde(default)]
	pub duration_ms:  Option<f64>,
}

/// A direct shell command (`bash` RPC command, `!cmd` in the TUI).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BashExecutionMessage {
	#[serde(default)]
	pub command:              String,
	#[serde(default)]
	pub output:               String,
	#[serde(default)]
	pub exit_code:            Option<i64>,
	#[serde(default)]
	pub cancelled:            bool,
	#[serde(default)]
	pub truncated:            bool,
	#[serde(default)]
	pub exclude_from_context: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomMessage {
	#[serde(default)]
	pub custom_type: String,
	#[serde(default)]
	pub content:     Content,
	#[serde(default)]
	pub display:     bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryMessage {
	#[serde(default)]
	pub summary:       String,
	#[serde(default)]
	pub tokens_before: Option<i64>,
}

/// `AgentMessage`, keyed by `role`.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentMessage {
	User(UserMessage),
	Assistant(AssistantMessage),
	ToolResult(ToolResultMessage),
	BashExecution(BashExecutionMessage),
	Custom(CustomMessage),
	CompactionSummary(SummaryMessage),
	BranchSummary(SummaryMessage),
	/// `system` and roles added by extensions.
	Other { role: String, raw: Value },
}

impl AgentMessage {
	pub fn from_value(value: Value) -> Self {
		let role = value.get("role").and_then(Value::as_str).unwrap_or_default().to_owned();
		fn parse<T: serde::de::DeserializeOwned>(value: &Value) -> Option<T> {
			serde_json::from_value(value.clone()).ok()
		}
		let parsed = match role.as_str() {
			"user" => parse(&value).map(Self::User),
			"assistant" => parse(&value).map(Self::Assistant),
			"toolResult" => parse(&value).map(Self::ToolResult),
			"bashExecution" => parse(&value).map(Self::BashExecution),
			"custom" => parse(&value).map(Self::Custom),
			"compactionSummary" => parse(&value).map(Self::CompactionSummary),
			"branchSummary" => parse(&value).map(Self::BranchSummary),
			_ => None,
		};
		parsed.unwrap_or(Self::Other { role, raw: value })
	}
}

impl<'de> Deserialize<'de> for AgentMessage {
	fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
		Ok(Self::from_value(Value::deserialize(deserializer)?))
	}
}

// ---------------------------------------------------------------- events

/// `message_update.assistantMessageEvent` (delta-only on the wire).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum AssistantMessageEvent {
	TextStart { content_index: usize },
	TextDelta { content_index: usize, delta: String },
	TextEnd { content_index: usize, content: String },
	ThinkingStart { content_index: usize },
	ThinkingDelta { content_index: usize, delta: String },
	ThinkingEnd { content_index: usize, content: String },
	ToolcallStart {
		content_index: usize,
		#[serde(default)]
		id:            String,
		#[serde(default)]
		tool_name:     String,
	},
	ToolcallDelta { content_index: usize, delta: String },
	ToolcallEnd { content_index: usize, tool_call: ToolCall },
	#[serde(other)]
	Other,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolExecution {
	pub tool_call_id:   String,
	#[serde(default)]
	pub tool_name:      String,
	#[serde(default)]
	pub args:           Option<Value>,
	#[serde(default)]
	pub partial_result: Option<Value>,
	#[serde(default)]
	pub result:         Option<Value>,
	#[serde(default)]
	pub is_error:       bool,
	#[serde(default)]
	pub duration_ms:    Option<f64>,
}

/// Extension UI requests (`extension_ui_request`), keyed by `method`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "method", rename_all_fields = "camelCase")]
pub enum UiRequest {
	#[serde(rename = "select")]
	Select {
		id:      String,
		#[serde(default)]
		title:   String,
		#[serde(default)]
		options: Vec<String>,
		#[serde(default)]
		timeout: Option<u64>,
	},
	#[serde(rename = "confirm")]
	Confirm {
		id:      String,
		#[serde(default)]
		title:   String,
		#[serde(default)]
		message: String,
		#[serde(default)]
		timeout: Option<u64>,
	},
	#[serde(rename = "input")]
	Input {
		id:          String,
		#[serde(default)]
		title:       String,
		#[serde(default)]
		placeholder: Option<String>,
		#[serde(default)]
		timeout:     Option<u64>,
	},
	#[serde(rename = "editor")]
	Editor {
		id:      String,
		#[serde(default)]
		title:   String,
		#[serde(default)]
		prefill: Option<String>,
	},
	#[serde(rename = "notify")]
	Notify {
		#[serde(default)]
		message:     String,
		#[serde(default)]
		notify_type: Option<String>,
	},
	#[serde(rename = "setStatus")]
	SetStatus {
		status_key:  String,
		#[serde(default)]
		status_text: Option<String>,
	},
	#[serde(rename = "setWidget")]
	SetWidget {
		widget_key:       String,
		#[serde(default)]
		widget_lines:     Option<Vec<String>>,
		#[serde(default)]
		widget_placement: Option<String>,
	},
	#[serde(rename = "setTitle")]
	SetTitle {
		#[serde(default)]
		title: String,
	},
	#[serde(rename = "set_editor_text")]
	SetEditorText {
		#[serde(default)]
		text: String,
	},
	#[serde(other)]
	Other,
}

impl UiRequest {
	/// The id of a dialog that waits for an answer.
	pub fn dialog_id(&self) -> Option<&str> {
		match self {
			Self::Select { id, .. } | Self::Confirm { id, .. } | Self::Input { id, .. } | Self::Editor { id, .. } => Some(id),
			_ => None,
		}
	}

	/// Milliseconds after which pi answers the dialog itself.
	pub fn timeout_ms(&self) -> Option<u64> {
		match self {
			Self::Select { timeout, .. } | Self::Confirm { timeout, .. } | Self::Input { timeout, .. } => *timeout,
			_ => None,
		}
	}
}

/// The answer to a dialog (`extension_ui_response`).
#[derive(Debug, Clone, PartialEq)]
pub enum UiResponse {
	Value { id: String, value: String },
	Confirmed { id: String, confirmed: bool },
	Cancelled { id: String },
}

impl UiResponse {
	pub fn id(&self) -> &str {
		match self {
			Self::Value { id, .. } | Self::Confirmed { id, .. } | Self::Cancelled { id } => id,
		}
	}

	pub fn to_wire(&self) -> Value {
		use serde_json::json;
		match self {
			Self::Value { id, value } => json!({"type": "extension_ui_response", "id": id, "value": value}),
			Self::Confirmed { id, confirmed } => {
				json!({"type": "extension_ui_response", "id": id, "confirmed": confirmed})
			},
			Self::Cancelled { id } => json!({"type": "extension_ui_response", "id": id, "cancelled": true}),
		}
	}
}

/// An unsolicited stdout record: a session event, an extension UI request,
/// or an RPC-only event.
#[derive(Debug, Clone, PartialEq)]
pub enum Notification {
	AgentStart,
	AgentEnd { will_retry: bool },
	AgentSettled { aborted: bool },
	TurnStart,
	TurnEnd,
	MessageStart(AgentMessage),
	MessageUpdate(AssistantMessageEvent),
	MessageEnd(AgentMessage),
	ToolExecutionStart(ToolExecution),
	ToolExecutionUpdate(ToolExecution),
	ToolExecutionEnd(ToolExecution),
	QueueUpdate { steering: Vec<String>, follow_up: Vec<String> },
	SessionInfoChanged { name: Option<String> },
	ThinkingLevelChanged { level: Option<ThinkingLevel> },
	CompactionStart { reason: Option<String> },
	CompactionEnd { reason: Option<String>, aborted: bool, error_message: Option<String> },
	AutoRetryStart { attempt: i64, max_attempts: i64, error_message: Option<String> },
	AutoRetryEnd { success: bool, final_error: Option<String> },
	/// Output chunk of a direct `bash` command; `id` is the command's id.
	BashExecutionUpdate { id: Option<String>, delta: String },
	ExtensionError { extension_path: String, error: String },
	Ui(UiRequest),
	/// A record type this client does not handle (`entry_appended`, …).
	Unknown(Value),
}

impl Notification {
	pub fn from_value(value: Value) -> Self {
		let kind = value.get("type").and_then(Value::as_str).unwrap_or_default();
		let str_field = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
		let bool_field = |key: &str| value.get(key).and_then(Value::as_bool).unwrap_or(false);
		let strings = |key: &str| -> Vec<String> {
			value.get(key).and_then(|v| serde_json::from_value(v.clone()).ok()).unwrap_or_default()
		};
		let message = || AgentMessage::from_value(value.get("message").cloned().unwrap_or(Value::Null));
		let tool = || serde_json::from_value::<ToolExecution>(value.clone()).ok();
		let parsed = match kind {
			"agent_start" => Some(Self::AgentStart),
			"agent_end" => Some(Self::AgentEnd { will_retry: bool_field("willRetry") }),
			"agent_settled" => Some(Self::AgentSettled { aborted: bool_field("aborted") }),
			"turn_start" => Some(Self::TurnStart),
			"turn_end" => Some(Self::TurnEnd),
			"message_start" => Some(Self::MessageStart(message())),
			"message_end" => Some(Self::MessageEnd(message())),
			"message_update" => value
				.get("assistantMessageEvent")
				.and_then(|e| serde_json::from_value(e.clone()).ok())
				.map(Self::MessageUpdate),
			"tool_execution_start" => tool().map(Self::ToolExecutionStart),
			"tool_execution_update" => tool().map(Self::ToolExecutionUpdate),
			"tool_execution_end" => tool().map(Self::ToolExecutionEnd),
			"queue_update" => Some(Self::QueueUpdate { steering: strings("steering"), follow_up: strings("followUp") }),
			"session_info_changed" => Some(Self::SessionInfoChanged { name: str_field("name") }),
			"thinking_level_changed" => Some(Self::ThinkingLevelChanged {
				level: value.get("level").and_then(|v| serde_json::from_value(v.clone()).ok()),
			}),
			"compaction_start" => Some(Self::CompactionStart { reason: str_field("reason") }),
			"compaction_end" => Some(Self::CompactionEnd {
				reason:        str_field("reason"),
				aborted:       bool_field("aborted"),
				error_message: str_field("errorMessage"),
			}),
			"auto_retry_start" => Some(Self::AutoRetryStart {
				attempt:       value.get("attempt").and_then(Value::as_i64).unwrap_or_default(),
				max_attempts:  value.get("maxAttempts").and_then(Value::as_i64).unwrap_or_default(),
				error_message: str_field("errorMessage"),
			}),
			"auto_retry_end" => {
				Some(Self::AutoRetryEnd { success: bool_field("success"), final_error: str_field("finalError") })
			},
			"bash_execution_update" => {
				Some(Self::BashExecutionUpdate { id: str_field("id"), delta: str_field("delta").unwrap_or_default() })
			},
			"extension_error" => Some(Self::ExtensionError {
				extension_path: str_field("extensionPath").unwrap_or_default(),
				error:          str_field("error").unwrap_or_default(),
			}),
			"extension_ui_request" => serde_json::from_value(value.clone()).ok().map(Self::Ui),
			_ => None,
		};
		parsed.unwrap_or(Self::Unknown(value))
	}
}

// ---------------------------------------------------------------- state

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
	Off,
	Minimal,
	Low,
	Medium,
	High,
	Xhigh,
	Max,
}

impl ThinkingLevel {
	pub fn as_str(self) -> &'static str {
		match self {
			Self::Off => "off",
			Self::Minimal => "minimal",
			Self::Low => "low",
			Self::Medium => "medium",
			Self::High => "high",
			Self::Xhigh => "xhigh",
			Self::Max => "max",
		}
	}
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
	#[serde(default)]
	pub id:             String,
	#[serde(default)]
	pub name:           String,
	#[serde(default)]
	pub api:            String,
	#[serde(default)]
	pub provider:       String,
	#[serde(default)]
	pub reasoning:      bool,
	#[serde(default)]
	pub input:          Vec<String>,
	#[serde(default)]
	pub context_window: i64,
	#[serde(default)]
	pub max_tokens:     i64,
}

impl Model {
	/// The placeholder pi reports when no model has credentials.
	pub fn is_unknown(&self) -> bool {
		self.provider == "unknown" && self.id == "unknown"
	}

	pub fn display_name(&self) -> &str {
		if self.name.trim().is_empty() { &self.id } else { &self.name }
	}
}

/// `get_state`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionState {
	#[serde(default)]
	pub model:                   Option<Model>,
	#[serde(default)]
	pub thinking_level:          Option<ThinkingLevel>,
	#[serde(default)]
	pub is_streaming:            bool,
	#[serde(default)]
	pub is_compacting:           bool,
	#[serde(default)]
	pub steering_mode:           Option<String>,
	#[serde(default)]
	pub follow_up_mode:          Option<String>,
	#[serde(default)]
	pub session_file:            Option<String>,
	#[serde(default)]
	pub session_id:              String,
	#[serde(default)]
	pub session_name:            Option<String>,
	#[serde(default)]
	pub auto_compaction_enabled: bool,
	#[serde(default)]
	pub message_count:           i64,
	#[serde(default)]
	pub pending_message_count:   i64,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenTotals {
	#[serde(default)]
	pub input:       i64,
	#[serde(default)]
	pub output:      i64,
	#[serde(default)]
	pub cache_read:  i64,
	#[serde(default)]
	pub cache_write: i64,
	#[serde(default)]
	pub total:       i64,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextUsage {
	/// `null` right after a compaction, until the next response reports usage.
	#[serde(default)]
	pub tokens:         Option<i64>,
	#[serde(default)]
	pub context_window: i64,
	#[serde(default)]
	pub percent:        Option<f64>,
}

/// `get_session_stats`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStats {
	#[serde(default)]
	pub user_messages:      i64,
	#[serde(default)]
	pub assistant_messages: i64,
	#[serde(default)]
	pub tool_calls:         i64,
	#[serde(default)]
	pub total_messages:     i64,
	#[serde(default)]
	pub tokens:             TokenTotals,
	#[serde(default)]
	pub cost:               f64,
	#[serde(default)]
	pub context_usage:      Option<ContextUsage>,
}

/// One entry of `get_commands`: extension commands, prompt templates, skills.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SlashCommand {
	pub name:        String,
	#[serde(default)]
	pub description: Option<String>,
	#[serde(default)]
	pub source:      String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum StreamingBehavior {
	#[serde(rename = "steer")]
	Steer,
	#[serde(rename = "followUp")]
	FollowUp,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BashResult {
	#[serde(default)]
	pub output:           String,
	#[serde(default)]
	pub exit_code:        Option<i64>,
	#[serde(default)]
	pub cancelled:        bool,
	#[serde(default)]
	pub truncated:        bool,
	#[serde(default)]
	pub full_output_path: Option<String>,
}

#[cfg(test)]
mod tests {
	use serde_json::json;

	use super::*;

	#[test]
	fn messages_decode_by_role_and_keep_unknown_roles() {
		let user = AgentMessage::from_value(json!({"role":"user","content":[{"type":"text","text":"hi"},{"type":"image","data":"AA","mimeType":"image/png"}]}));
		let AgentMessage::User(user) = user else { panic!("user") };
		assert_eq!(user.content.text(), "hi");
		assert_eq!(user.content.parts().len(), 2);

		let assistant = AgentMessage::from_value(json!({"role":"assistant","content":[
			{"type":"thinking","thinking":"t"},
			{"type":"toolCall","id":"c1","name":"bash","arguments":{"command":"ls"}},
			{"type":"weird"}
		],"stopReason":"toolUse"}));
		let AgentMessage::Assistant(a) = assistant else { panic!("assistant") };
		assert!(matches!(&a.content[1], AssistantContent::ToolCall(c) if c.name == "bash" && c.arguments["command"] == "ls"));
		assert_eq!(a.content[2], AssistantContent::Other);

		let system = AgentMessage::from_value(json!({"role":"system","content":""}));
		assert!(matches!(system, AgentMessage::Other { ref role, .. } if role == "system"));
	}

	#[test]
	fn events_decode_from_real_pi_records() {
		let update = Notification::from_value(json!({"type":"message_update","usage":{},"assistantMessageEvent":{"type":"toolcall_start","contentIndex":1,"id":"bash:0","toolName":"bash"}}));
		assert_eq!(
			update,
			Notification::MessageUpdate(AssistantMessageEvent::ToolcallStart { content_index: 1, id: "bash:0".into(), tool_name: "bash".into() })
		);
		let end = Notification::from_value(json!({"type":"tool_execution_end","toolCallId":"bash:0","toolName":"bash","result":{"content":[]},"isError":false,"durationMs":1023}));
		assert!(matches!(end, Notification::ToolExecutionEnd(t) if t.duration_ms == Some(1023.)));
		let ui = Notification::from_value(json!({"type":"extension_ui_request","id":"u","method":"setStatus","statusKey":"usage"}));
		assert_eq!(ui, Notification::Ui(UiRequest::SetStatus { status_key: "usage".into(), status_text: None }));
		let other = Notification::from_value(json!({"type":"extension_ui_request","id":"u","method":"later"}));
		assert_eq!(other, Notification::Ui(UiRequest::Other));
		assert!(matches!(Notification::from_value(json!({"type":"entry_appended"})), Notification::Unknown(_)));
	}

	#[test]
	fn ui_responses_serialize_like_the_protocol() {
		assert_eq!(
			UiResponse::Cancelled { id: "1".into() }.to_wire(),
			json!({"type":"extension_ui_response","id":"1","cancelled":true})
		);
		assert_eq!(
			UiResponse::Confirmed { id: "2".into(), confirmed: false }.to_wire(),
			json!({"type":"extension_ui_response","id":"2","confirmed":false})
		);
	}
}
