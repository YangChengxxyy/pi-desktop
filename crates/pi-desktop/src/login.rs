//! Provider list and login state for the accounts panel, folded from the
//! accounts bridge extension's events (`pi-bridge/extension/accounts.ts`).
//! Free of GPUI so it can be tested directly.
//!
//! The extension reports through `setWidget("pi-desktop/accounts", [json])`
//! and asks for login input through `input`/`select` dialogs whose title is
//! a JSON description of pi's `AuthPrompt`.

use pi_rpc::{UiRequest, UiResponse};
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct OauthMethod {
	#[serde(default)]
	pub name:         Option<String>,
	#[serde(default)]
	pub label:        Option<String>,
	#[serde(default)]
	pub subscription: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ApiKeyMethod {
	#[serde(default)]
	pub name:        Option<String>,
	/// The provider can prompt for a key; ambient-only providers (env vars,
	/// cloud profiles) cannot.
	#[serde(default)]
	pub interactive: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Provider {
	pub id:         String,
	pub name:       String,
	#[serde(default)]
	pub oauth:      Option<OauthMethod>,
	#[serde(rename = "apiKey", default)]
	pub api_key:    Option<ApiKeyMethod>,
	/// Some credential source works: stored, environment, or config.
	#[serde(default)]
	pub configured: bool,
	/// Where the working credential comes from, as pi labels it.
	#[serde(default)]
	pub source:     Option<String>,
}

impl Provider {
	pub fn can_log_in(&self) -> bool {
		self.oauth.is_some() || self.api_key.as_ref().is_some_and(|k| k.interactive)
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum AuthType {
	#[serde(rename = "oauth")]
	Oauth,
	#[serde(rename = "api_key")]
	ApiKey,
}

impl AuthType {
	pub fn as_str(self) -> &'static str {
		match self {
			Self::Oauth => "oauth",
			Self::ApiKey => "api_key",
		}
	}

	pub fn label(self) -> &'static str {
		match self {
			Self::Oauth => "OAuth",
			Self::ApiKey => "API key",
		}
	}
}

/// A credential saved in `auth.json` (what logout removes).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Credential {
	pub provider_id: String,
	#[serde(rename = "type")]
	pub kind:        AuthType,
}

/// One event from the extension.
#[derive(Debug, Clone, PartialEq)]
pub enum BridgeEvent {
	Providers { providers: Vec<Provider>, credentials: Vec<Credential> },
	LoginStarted { provider_id: String },
	/// pi's `AuthEvent`: `auth_url`, `device_code`, `info`, `progress`.
	Auth(AuthEvent),
	/// pi answered the open prompt itself (the browser callback won).
	PromptClosed,
	LoginDone { provider_id: String },
	LoginFailed { provider_id: String, cancelled: bool, message: String },
	LoggedOut { provider_id: String },
	Error { message: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthEvent {
	Url { url: String, instructions: Option<String> },
	DeviceCode { user_code: String, verification_uri: String },
	Info { message: String, links: Vec<String> },
	Progress { message: String },
}

impl BridgeEvent {
	/// Parses one widget line; `None` for anything else.
	pub fn parse(line: &str) -> Option<Self> {
		let value: Value = serde_json::from_str(line).ok()?;
		let text = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
		let provider_id = || text("providerId").unwrap_or_default();
		Some(match value.get("event")?.as_str()? {
			"providers" => Self::Providers {
				providers:   serde_json::from_value(value.get("providers")?.clone()).ok()?,
				credentials: value.get("credentials").and_then(|c| serde_json::from_value(c.clone()).ok()).unwrap_or_default(),
			},
			"login_started" => Self::LoginStarted { provider_id: provider_id() },
			"auth" => Self::Auth(match value.get("type")?.as_str()? {
				"auth_url" => AuthEvent::Url { url: text("url")?, instructions: text("instructions") },
				"device_code" => AuthEvent::DeviceCode { user_code: text("userCode")?, verification_uri: text("verificationUri")? },
				"info" => AuthEvent::Info {
					message: text("message").unwrap_or_default(),
					links:   value
						.get("links")
						.and_then(Value::as_array)
						.into_iter()
						.flatten()
						.filter_map(|l| l.get("url").and_then(Value::as_str).map(str::to_owned))
						.collect(),
				},
				_ => AuthEvent::Progress { message: text("message").unwrap_or_default() },
			}),
			"prompt_closed" => Self::PromptClosed,
			"login_done" => Self::LoginDone { provider_id: provider_id() },
			"login_failed" => Self::LoginFailed {
				provider_id: provider_id(),
				cancelled:   value.get("cancelled").and_then(Value::as_bool).unwrap_or(false),
				message:     text("message").unwrap_or_default(),
			},
			"logged_out" => Self::LoggedOut { provider_id: provider_id() },
			"error" => Self::Error { message: text("message").unwrap_or_default() },
			_ => return None,
		})
	}
}

/// Providers matching every whitespace-separated term of `query` (in the id
/// or the name, case-insensitively); configured ones first, then by name.
pub fn filter<'a>(providers: &'a [Provider], query: &str) -> Vec<&'a Provider> {
	let terms: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
	let mut rows: Vec<&Provider> = providers
		.iter()
		.filter(|p| {
			let haystack = format!("{} {}", p.id, p.name).to_lowercase();
			terms.iter().all(|term| haystack.contains(term))
		})
		.collect();
	rows.sort_by(|a, b| (!a.configured, a.name.to_lowercase()).cmp(&(!b.configured, b.name.to_lowercase())));
	rows
}

/// What a login prompt asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptKind {
	Text,
	/// An API key or another secret: the input is masked.
	Secret,
	/// The authorization code or redirect URL, when the browser cannot reach
	/// pi's local callback.
	ManualCode,
	Select(Vec<String>),
}

/// A login prompt waiting for the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompt {
	pub id:          String,
	pub kind:        PromptKind,
	pub message:     String,
	pub placeholder: Option<String>,
}

impl Prompt {
	/// The extension's dialog, decoded; `None` for dialogs it did not send.
	pub fn from_request(request: &UiRequest) -> Option<Self> {
		let (id, title, options) = match request {
			UiRequest::Input { id, title, .. } => (id, title, None),
			UiRequest::Select { id, title, options, .. } => (id, title, Some(options.clone())),
			_ => return None,
		};
		let spec: Value = serde_json::from_str(title).ok()?;
		let text = |key: &str| spec.get(key).and_then(Value::as_str).map(str::to_owned);
		let kind = match (options, text("type")?.as_str()) {
			(Some(options), _) => PromptKind::Select(options),
			(None, "secret") => PromptKind::Secret,
			(None, "manual_code") => PromptKind::ManualCode,
			(None, _) => PromptKind::Text,
		};
		Some(Self { id: id.clone(), kind, message: text("message").unwrap_or_default(), placeholder: text("placeholder") })
	}

	/// The answer to send; `None` for blank text, which pi would reject.
	pub fn answer(&self, value: &str) -> Option<UiResponse> {
		let value = value.trim();
		(!value.is_empty()).then(|| UiResponse::Value { id: self.id.clone(), value: value.to_owned() })
	}
}

/// One login in progress.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LoginFlow {
	pub provider_id: String,
	/// The authorize URL (`auth_url`) or device verification URI.
	pub url:         Option<String>,
	pub user_code:   Option<String>,
	pub instructions: Option<String>,
	/// Latest progress or info text.
	pub progress:    Option<String>,
	pub prompt:      Option<Prompt>,
}

/// What the view must do for an event that arrived during a login.
#[derive(Debug, Clone, PartialEq)]
pub enum FlowAction {
	None,
	OpenBrowser(String),
}

impl LoginFlow {
	pub fn new(provider_id: impl Into<String>) -> Self {
		Self { provider_id: provider_id.into(), ..Default::default() }
	}

	pub fn apply(&mut self, event: &AuthEvent) -> FlowAction {
		match event {
			AuthEvent::Url { url, instructions } => {
				self.url = Some(url.clone());
				self.instructions = instructions.clone();
				FlowAction::OpenBrowser(url.clone())
			},
			AuthEvent::DeviceCode { user_code, verification_uri } => {
				self.url = Some(verification_uri.clone());
				self.user_code = Some(user_code.clone());
				FlowAction::OpenBrowser(verification_uri.clone())
			},
			AuthEvent::Info { message, links } => {
				self.progress = Some(message.clone()).filter(|m| !m.is_empty());
				if self.url.is_none() {
					self.url = links.first().cloned();
				}
				FlowAction::None
			},
			AuthEvent::Progress { message } => {
				self.progress = Some(message.clone()).filter(|m| !m.is_empty());
				FlowAction::None
			},
		}
	}
}

#[cfg(test)]
mod tests {
	use serde_json::json;

	use super::*;

	#[::core::prelude::v1::test]
	fn parses_bridge_events() {
		let line = json!({"event":"providers","providers":[
			{"id":"anthropic","name":"Anthropic","oauth":{"name":"Claude Pro/Max","label":null,"subscription":true},
				"apiKey":{"name":"Anthropic API key","interactive":true},"configured":false,"source":null},
			{"id":"amazon-bedrock","name":"Amazon Bedrock","oauth":null,"apiKey":{"name":null,"interactive":false},"configured":true,"source":"environment"}
		],"credentials":[{"providerId":"kiro","type":"oauth"}]})
		.to_string();
		let Some(BridgeEvent::Providers { providers, credentials }) = BridgeEvent::parse(&line) else { panic!() };
		assert!(providers[0].can_log_in());
		assert!(!providers[1].can_log_in(), "ambient-only");
		assert_eq!(credentials, [Credential { provider_id: "kiro".into(), kind: AuthType::Oauth }]);

		let auth = BridgeEvent::parse(&json!({"event":"auth","type":"auth_url","url":"https://a/b","instructions":"Open it"}).to_string());
		assert_eq!(auth, Some(BridgeEvent::Auth(AuthEvent::Url { url: "https://a/b".into(), instructions: Some("Open it".into()) })));
		let failed = BridgeEvent::parse(&json!({"event":"login_failed","providerId":"x","cancelled":true,"message":"Login cancelled"}).to_string());
		assert!(matches!(failed, Some(BridgeEvent::LoginFailed { cancelled: true, .. })));
		assert_eq!(BridgeEvent::parse("{\"other\":1}"), None);
		assert_eq!(BridgeEvent::parse("not json"), None);
	}

	#[::core::prelude::v1::test]
	fn filter_matches_terms_and_puts_configured_first() {
		let p = |id: &str, name: &str, configured: bool| Provider {
			id: id.into(),
			name: name.into(),
			oauth: None,
			api_key: None,
			configured,
			source: None,
		};
		let list = [p("openai-codex", "ChatGPT Plus/Pro", false), p("anthropic", "Anthropic", true), p("openrouter", "OpenRouter", true)];
		let ids = |rows: Vec<&Provider>| rows.iter().map(|p| p.id.clone()).collect::<Vec<_>>();
		assert_eq!(ids(filter(&list, "")), ["anthropic", "openrouter", "openai-codex"]);
		assert_eq!(ids(filter(&list, "OPEN")), ["openrouter", "openai-codex"]);
		assert_eq!(ids(filter(&list, "chatgpt pro")), ["openai-codex"]);
	}

	#[::core::prelude::v1::test]
	fn prompts_decode_from_dialog_titles() {
		let input = UiRequest::Input {
			id:          "d1".into(),
			title:       json!({"type":"secret","message":"Enter API key","placeholder":"sk-..."}).to_string(),
			placeholder: Some("sk-...".into()),
			timeout:     None,
		};
		let prompt = Prompt::from_request(&input).unwrap();
		assert_eq!((prompt.kind.clone(), prompt.message.as_str()), (PromptKind::Secret, "Enter API key"));
		assert_eq!(prompt.answer("  "), None);
		assert_eq!(prompt.answer(" sk-1 "), Some(UiResponse::Value { id: "d1".into(), value: "sk-1".into() }));

		let select = UiRequest::Select {
			id:      "d2".into(),
			title:   json!({"type":"select","message":"Region"}).to_string(),
			options: vec!["us".into(), "eu".into()],
			timeout: None,
		};
		assert_eq!(Prompt::from_request(&select).unwrap().kind, PromptKind::Select(vec!["us".into(), "eu".into()]));
		let foreign = UiRequest::Input { id: "x".into(), title: "Name?".into(), placeholder: None, timeout: None };
		assert_eq!(Prompt::from_request(&foreign), None, "not the extension's dialog");
	}

	#[::core::prelude::v1::test]
	fn flow_folds_url_device_code_and_progress() {
		let mut flow = LoginFlow::new("openai-codex");
		let action = flow.apply(&AuthEvent::Url { url: "https://auth/x".into(), instructions: Some("A browser window should open.".into()) });
		assert_eq!(action, FlowAction::OpenBrowser("https://auth/x".into()));
		flow.apply(&AuthEvent::Progress { message: "Waiting...".into() });
		assert_eq!(flow.progress.as_deref(), Some("Waiting..."));

		let mut device = LoginFlow::new("github-copilot");
		device.apply(&AuthEvent::DeviceCode { user_code: "AB-12".into(), verification_uri: "https://github.com/login/device".into() });
		assert_eq!((device.user_code.as_deref(), device.url.as_deref()), (Some("AB-12"), Some("https://github.com/login/device")));
	}
}
