//! Accounts panel: lists pi's providers, runs OAuth or API-key login, and
//! removes stored credentials.
//!
//! It drives its own pi process with the accounts bridge extension loaded
//! (see `pi-bridge/extension/accounts.ts`), so a login never waits behind a
//! session turn and works before any session can start (pi starts without
//! credentials). The process loads no other extensions, tools, or sessions
//! and makes no model calls.

use std::sync::Arc;
use std::time::Duration;

use gpui_kit::base::TestSupportExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _};
use gpui_kit::prelude::*;
use gpui_kit::*;
use pi_bridge::{ACCOUNTS_COMMAND, ACCOUNTS_WIDGET, Session, SessionOptions};
use pi_rpc::{Client, Notification, UiRequest, UiResponse};
use serde_json::json;

use crate::login::{self, AuthType, BridgeEvent, Credential, FlowAction, LoginFlow, Prompt, PromptKind, Provider};
use crate::theme;

pub enum AccountsEvent {
	/// Credentials changed (login or logout succeeded).
	Changed,
	Close,
}

impl EventEmitter<AccountsEvent> for AccountsView {}

enum Conn {
	Connecting,
	Connected(Arc<Client>),
	Failed(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Message {
	Info(String),
	Error(String),
}

pub struct AccountsView {
	options:     SessionOptions,
	conn:        Conn,
	providers:   Vec<Provider>,
	credentials: Vec<Credential>,
	search:      Entity<InputState>,
	/// Answer to the current login prompt (masked for secrets).
	answer:      Entity<InputState>,
	flow:        Option<LoginFlow>,
	/// Provider whose login methods are offered (it has both OAuth and an
	/// API key).
	choosing:    Option<String>,
	/// Credential waiting for the user to confirm its removal.
	confirm:     Option<String>,
	message:     Option<Message>,
	busy:        bool,
	_tasks:      Vec<Task<()>>,
	_subs:       Vec<Subscription>,
}

impl AccountsView {
	pub fn new(base: &SessionOptions, window: &mut Window, cx: &mut Context<Self>) -> Self {
		let mut env = base.env.clone();
		// The panel must not wait for catalog downloads.
		env.push(("PI_OFFLINE".into(), "1".into()));
		let dir = std::env::temp_dir().join("pi-desktop");
		let extension = pi_bridge::accounts_extension(&dir);
		let mut extra_args: Vec<String> =
			["--no-session", "--no-extensions", "--no-tools", "--no-mcp", "--no-skills", "--no-prompt-templates", "--no-themes", "--no-context-files", "--no-approve"]
				.map(String::from)
				.to_vec();
		let failed = match &extension {
			Ok(path) => {
				extra_args.extend(["-e".to_owned(), path.to_string_lossy().into_owned()]);
				None
			},
			Err(error) => Some(format!("写入账号扩展失败：{error}")),
		};
		let options = SessionOptions {
			pi: base.pi.clone(),
			cwd: Some(dir),
			extra_args,
			env,
			require_model: false,
			..Default::default()
		};
		let search = cx.new(|cx| InputState::new(window, cx).placeholder("搜索 provider"));
		search.update(cx, |state, cx| state.focus(window, cx));
		let answer = cx.new(|cx| InputState::new(window, cx));
		let subs = vec![
			cx.subscribe(&search, |_, _, event: &InputEvent, cx| {
				if matches!(event, InputEvent::Change) {
					cx.notify();
				}
			}),
			cx.subscribe(&answer, |this, _, event: &InputEvent, cx| {
				if matches!(event, InputEvent::PressEnter { .. }) {
					this.submit_answer(cx);
				}
			}),
		];
		let mut view = Self {
			options,
			conn: Conn::Connecting,
			providers: Vec::new(),
			credentials: Vec::new(),
			search,
			answer,
			flow: None,
			choosing: None,
			confirm: None,
			message: None,
			busy: false,
			_tasks: Vec::new(),
			_subs: subs,
		};
		match failed {
			Some(error) => view.conn = Conn::Failed(error),
			None => view.connect(cx),
		}
		view
	}

	fn client(&self) -> Option<Arc<Client>> {
		match &self.conn {
			Conn::Connected(client) => Some(client.clone()),
			_ => None,
		}
	}

	fn connect(&mut self, cx: &mut Context<Self>) {
		self.conn = Conn::Connecting;
		let options = self.options.clone();
		let spawn = cx.background_spawn(crate::blocking::blocking(move || Session::spawn(&options)));
		let task = cx.spawn(async move |this, cx| {
			let session = match spawn.await {
				Ok(session) => session,
				Err(error) => {
					_ = this.update(cx, |this, cx| {
						this.conn = Conn::Failed(error.to_string());
						cx.notify();
					});
					return;
				},
			};
			let events = session.events.clone();
			_ = this.update(cx, |this, cx| {
				this.conn = Conn::Connected(session.client.clone());
				this.run("list", cx);
				cx.notify();
			});
			while let Ok(notification) = events.recv().await {
				if this.update(cx, |this, cx| this.on_notification(notification, cx)).is_err() {
					return;
				}
			}
			_ = this.update(cx, |this, cx| {
				this.conn = Conn::Failed("pi 进程已退出".into());
				this.flow = None;
				cx.notify();
			});
		});
		// Dropping the old task drops the old client, which stops its process.
		self._tasks = vec![task];
	}

	/// Sends `/pi-desktop-accounts <args>`; results come back as widget events.
	fn run(&mut self, args: &str, cx: &mut Context<Self>) {
		let Some(client) = self.client() else { return };
		let message = format!("/{ACCOUNTS_COMMAND} {args}");
		cx.background_spawn(crate::blocking::blocking(move || {
			// `login` resolves only when the flow ends, so the reply can take
			// minutes; nothing waits on it.
			if let Err(error) = client.request("prompt", json!({"message": message}), Some(Duration::from_secs(900))) {
				eprintln!("pi-desktop accounts: {error}");
			}
		}))
		.detach();
	}

	fn on_notification(&mut self, notification: Notification, cx: &mut Context<Self>) {
		let Notification::Ui(request) = notification else { return };
		match request {
			UiRequest::SetWidget { widget_key, widget_lines: Some(lines), .. } if widget_key == ACCOUNTS_WIDGET => {
				for event in lines.iter().filter_map(|line| BridgeEvent::parse(line)) {
					self.on_event(event, cx);
				}
			},
			request @ (UiRequest::Input { .. } | UiRequest::Select { .. }) => match (Prompt::from_request(&request), &mut self.flow) {
				(Some(prompt), Some(flow)) => {
					let secret = prompt.kind == PromptKind::Secret;
					let placeholder = prompt.placeholder.clone().unwrap_or_default();
					if let Some(window) = cx.windows().first().copied() {
						let answer = self.answer.clone();
						_ = window.update(cx, |_, window, cx| {
							answer.update(cx, |state, cx| {
								state.set_value("", window, cx);
								state.set_placeholder(placeholder, window, cx);
								state.set_masked(secret, window, cx);
								state.focus(window, cx);
							});
						});
					}
					flow.prompt = Some(prompt);
				},
				// Not a login prompt: decline so pi does not wait.
				_ => self.reply(UiResponse::Cancelled { id: request.dialog_id().unwrap_or_default().to_owned() }),
			},
			request @ (UiRequest::Confirm { .. } | UiRequest::Editor { .. }) => {
				self.reply(UiResponse::Cancelled { id: request.dialog_id().unwrap_or_default().to_owned() });
			},
			_ => {},
		}
		cx.notify();
	}

	fn on_event(&mut self, event: BridgeEvent, cx: &mut Context<Self>) {
		match event {
			BridgeEvent::Providers { providers, credentials } => {
				self.providers = providers;
				self.credentials = credentials;
				self.busy = false;
			},
			BridgeEvent::LoginStarted { .. } => {},
			BridgeEvent::Auth(event) => {
				if let Some(flow) = &mut self.flow
					&& let FlowAction::OpenBrowser(url) = flow.apply(&event)
				{
					cx.open_url(&url);
				}
			},
			BridgeEvent::PromptClosed => {
				if let Some(flow) = &mut self.flow {
					flow.prompt = None;
				}
			},
			BridgeEvent::LoginDone { provider_id } => {
				self.flow = None;
				self.message = Some(Message::Info(format!("已登录 {}", self.provider_name(&provider_id))));
				cx.emit(AccountsEvent::Changed);
				self.run("list", cx);
			},
			BridgeEvent::LoginFailed { provider_id, cancelled, message } => {
				self.flow = None;
				self.message = Some(if cancelled {
					Message::Info("已取消登录".into())
				} else {
					Message::Error(format!("登录 {} 失败：{message}", self.provider_name(&provider_id)))
				});
			},
			BridgeEvent::LoggedOut { provider_id } => {
				self.message = Some(Message::Info(format!("已移除 {} 的凭据", self.provider_name(&provider_id))));
				cx.emit(AccountsEvent::Changed);
				self.run("list", cx);
			},
			BridgeEvent::Error { message } => {
				self.busy = false;
				self.message = Some(Message::Error(message));
			},
		}
	}

	fn reply(&self, response: UiResponse) {
		if let Some(client) = self.client() {
			_ = client.send(&response.to_wire());
		}
	}

	pub fn login(&mut self, provider_id: String, method: AuthType, cx: &mut Context<Self>) {
		if self.flow.is_some() || self.client().is_none() {
			return;
		}
		self.message = None;
		self.choosing = None;
		self.confirm = None;
		self.flow = Some(LoginFlow::new(provider_id.clone()));
		self.run(&format!("login {provider_id} {}", method.as_str()), cx);
		cx.notify();
	}

	/// Clicking 登录: one method starts it, two ask which.
	fn start_login(&mut self, provider: &Provider, cx: &mut Context<Self>) {
		let api_key = provider.api_key.as_ref().is_some_and(|k| k.interactive);
		match (provider.oauth.is_some(), api_key) {
			(true, true) => {
				self.choosing = if self.choosing.as_deref() == Some(provider.id.as_str()) { None } else { Some(provider.id.clone()) };
				cx.notify();
			},
			(true, false) => self.login(provider.id.clone(), AuthType::Oauth, cx),
			(false, true) => self.login(provider.id.clone(), AuthType::ApiKey, cx),
			(false, false) => {},
		}
	}

	pub fn cancel_login(&mut self, cx: &mut Context<Self>) {
		let Some(flow) = &mut self.flow else { return };
		// Answering the open prompt with "cancelled" ends a flow that waits
		// on it; the extension's abort ends one that waits on the browser.
		if let Some(prompt) = flow.prompt.take() {
			self.reply(UiResponse::Cancelled { id: prompt.id });
		}
		self.run("cancel", cx);
		cx.notify();
	}

	fn submit_answer(&mut self, cx: &mut Context<Self>) {
		let value = self.answer.read(cx).value().to_string();
		let Some(response) = self.flow.as_ref().and_then(|f| f.prompt.as_ref()).and_then(|p| p.answer(&value)) else {
			return;
		};
		self.reply(response);
		if let Some(flow) = &mut self.flow {
			flow.prompt = None;
		}
		cx.notify();
	}

	fn pick(&mut self, option: String, cx: &mut Context<Self>) {
		let Some(prompt) = self.flow.as_mut().and_then(|f| f.prompt.take()) else { return };
		self.reply(UiResponse::Value { id: prompt.id, value: option });
		cx.notify();
	}

	fn logout(&mut self, provider_id: String, cx: &mut Context<Self>) {
		self.busy = true;
		self.confirm = None;
		self.run(&format!("logout {provider_id}"), cx);
		cx.notify();
	}

	fn provider_name(&self, id: &str) -> String {
		self.providers.iter().find(|p| p.id == id).map_or_else(|| id.to_owned(), |p| p.name.clone())
	}

	fn stored(&self, id: &str) -> Option<&Credential> {
		self.credentials.iter().find(|c| c.provider_id == id)
	}

	#[cfg(test)]
	pub(crate) fn providers(&self) -> &[Provider] {
		&self.providers
	}

	#[cfg(test)]
	pub(crate) fn credentials(&self) -> &[Credential] {
		&self.credentials
	}

	#[cfg(test)]
	pub(crate) fn flow(&self) -> Option<&LoginFlow> {
		self.flow.as_ref()
	}

	#[cfg(test)]
	pub(crate) fn message(&self) -> Option<&Message> {
		self.message.as_ref()
	}

	#[cfg(test)]
	pub(crate) fn answer_state(&self) -> Entity<InputState> {
		self.answer.clone()
	}

	fn render_flow(&self, flow: &LoginFlow, cx: &mut Context<Self>) -> impl IntoElement {
		let p = theme::palette(cx);
		let name = self.provider_name(&flow.provider_id);
		let open = flow.url.clone();
		let copy = flow.user_code.clone().or_else(|| flow.url.clone());
		let copy_label = if flow.user_code.is_some() { "复制验证码" } else { "复制链接" };
		let prompt = flow.prompt.clone();
		div()
			.id("login-flow")
			.flex()
			.flex_col()
			.gap_2()
			.p_3()
			.rounded(px(8.))
			.border_1()
			.border_color(rgb(p.primary))
			.bg(rgb(p.accent))
			.text_sm()
			.child(div().font_weight(FontWeight::SEMIBOLD).child(format!("正在登录 {name}")))
			.child(
				div()
					.text_xs()
					.text_color(rgb(p.muted_foreground))
					.whitespace_normal()
					.child(flow.instructions.clone().or_else(|| flow.progress.clone()).unwrap_or_else(|| "等待 pi…".into())),
			)
			.when_some(flow.user_code.clone(), |d, code| {
				d.child(div().id("login-user-code").font_family("Menlo").text_lg().font_weight(FontWeight::SEMIBOLD).child(code).test_support())
			})
			.when(flow.url.is_some(), |d| {
				d.child(
					div()
						.flex()
						.gap_2()
						.child(Button::new("login-open").small().primary().label("在浏览器中打开").on_click(cx.listener(
							move |_, _, _, cx| {
								if let Some(url) = &open {
									cx.open_url(url);
								}
							},
						)))
						.child(Button::new("login-copy").small().outline().label(copy_label).on_click(cx.listener(
							move |this, _, _, cx| {
								if let Some(text) = &copy {
									cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
									this.message = Some(Message::Info("已复制".into()));
									cx.notify();
								}
							},
						))),
				)
			})
			.when_some(prompt, |d, prompt| match &prompt.kind {
				PromptKind::Select(options) => d
					.child(div().text_xs().text_color(rgb(p.muted_foreground)).child(prompt.message.clone()))
					.child(div().flex().flex_wrap().gap_1().children(options.iter().enumerate().map(|(i, option)| {
						let value = option.clone();
						Button::new(("login-option", i))
							.small()
							.outline()
							.label(option.clone())
							.on_click(cx.listener(move |this, _, _, cx| this.pick(value.clone(), cx)))
					}))),
				kind => d
					.child(div().text_xs().text_color(rgb(p.muted_foreground)).whitespace_normal().child(match kind {
						PromptKind::ManualCode => format!("浏览器无法回到本机时：{}", prompt.message),
						_ => prompt.message.clone(),
					}))
					.child(
						div()
							.flex()
							.gap_2()
							.child(div().id("login-answer").flex_1().child(Input::new(&self.answer).small()).test_support())
							.child(Button::new("login-submit").small().label("提交").on_click(cx.listener(|this, _, _, cx| this.submit_answer(cx)))),
					),
			})
			.child(
				div().flex().justify_end().child(
					Button::new("login-cancel")
						.small()
						.ghost()
						.label("取消登录")
						.on_click(cx.listener(|this, _, _, cx| this.cancel_login(cx))),
				),
			)
			.test_support()
	}
}

impl Render for AccountsView {
	fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
		let p = theme::palette(cx);
		let query = self.search.read(cx).value().to_string();
		let connected = matches!(self.conn, Conn::Connected(_));
		let logging_in = self.flow.is_some();
		let rows: Vec<AnyElement> = login::filter(&self.providers, &query)
			.into_iter()
			.enumerate()
			.flat_map(|(index, provider)| {
				let stored = self.stored(&provider.id).cloned();
				let (login_target, logout_id, keep_id) = (provider.clone(), provider.id.clone(), provider.id.clone());
				let confirming = self.confirm.as_deref() == Some(provider.id.as_str());
				let row = div()
					.id(("provider", index))
					.flex()
					.items_center()
					.gap_2()
					.px_2()
					.h(px(30.))
					.rounded(px(6.))
					.text_sm()
					.hover(|s| s.bg(rgb(p.muted)))
					.child(div().flex_1().min_w_0().truncate().child(provider.name.clone()))
					.child(div().flex_none().text_xs().font_family("Menlo").text_color(rgb(p.muted_foreground)).child(provider.id.clone()))
					.when(provider.oauth.as_ref().is_some_and(|o| o.subscription), |r| {
						r.child(div().flex_none().text_xs().text_color(rgb(p.muted_foreground)).child("订阅"))
					})
					.when(provider.configured, |r| {
						let source = match &stored {
							Some(c) => format!("● {}", c.kind.label()),
							None => format!("● {}", provider.source.clone().unwrap_or_else(|| "已配置".into())),
						};
						r.child(div().flex_none().text_xs().text_color(rgb(p.success)).child(source))
					})
					.when(stored.is_some() && !confirming, |r| {
						r.child(
							Button::new(("provider-logout", index))
								.xsmall()
								.ghost()
								.label("退出登录")
								.disabled(!connected || self.busy || logging_in)
								.on_click(cx.listener(move |this, _, _, cx| {
									this.confirm = Some(keep_id.clone());
									cx.notify();
								})),
						)
					})
					.when(confirming, |r| {
						r.child(div().text_xs().text_color(rgb(p.danger)).child("确认移除？"))
							.child(
								Button::new(("provider-confirm", index))
									.xsmall()
									.danger()
									.label("移除")
									.on_click(cx.listener(move |this, _, _, cx| this.logout(logout_id.clone(), cx))),
							)
							.child(Button::new(("provider-keep", index)).xsmall().ghost().label("保留").on_click(cx.listener(|this, _, _, cx| {
								this.confirm = None;
								cx.notify();
							})))
					})
					.when(provider.can_log_in(), |r| {
						r.child(
							Button::new(("provider-login", index))
								.xsmall()
								.outline()
								.label(if provider.configured { "再登录" } else { "登录" })
								.disabled(!connected || logging_in)
								.on_click(cx.listener(move |this, _, _, cx| this.start_login(&login_target, cx))),
						)
					})
					.test_support()
					.into_any_element();
				let methods = (self.choosing.as_deref() == Some(provider.id.as_str())).then(|| {
					let (oauth_id, key_id) = (provider.id.clone(), provider.id.clone());
					let oauth_label = provider.oauth.as_ref().and_then(|o| o.label.clone().or_else(|| o.name.clone())).unwrap_or_else(|| "OAuth 登录".into());
					let key_label = provider.api_key.as_ref().and_then(|k| k.name.clone()).unwrap_or_else(|| "API key".into());
					div()
						.id(("provider-methods", index))
						.flex()
						.gap_2()
						.ml_4()
						.mb_1()
						.child(Button::new(("method-oauth", index)).xsmall().primary().label(oauth_label).on_click(cx.listener(
							move |this, _, _, cx| this.login(oauth_id.clone(), AuthType::Oauth, cx),
						)))
						.child(Button::new(("method-api-key", index)).xsmall().outline().label(key_label).on_click(cx.listener(
							move |this, _, _, cx| this.login(key_id.clone(), AuthType::ApiKey, cx),
						)))
						.test_support()
						.into_any_element()
				});
				std::iter::once(row).chain(methods)
			})
			.collect();

		let status: Option<AnyElement> = match &self.conn {
			Conn::Connecting => Some(div().text_sm().text_color(rgb(p.muted_foreground)).child("正在启动 pi…").into_any_element()),
			Conn::Failed(error) => Some(
				div()
					.flex()
					.items_center()
					.gap_2()
					.text_sm()
					.text_color(rgb(p.danger))
					.child(div().flex_1().child(format!("无法启动 pi：{error}")))
					.child(Button::new("accounts-retry").small().label("重试").on_click(cx.listener(|this, _, _, cx| this.connect(cx))))
					.into_any_element(),
			),
			Conn::Connected(_) => None,
		};

		div()
			.id("accounts")
			.w(px(640.))
			.h(px(600.))
			.flex()
			.flex_col()
			.gap_3()
			.p_4()
			.rounded(px(10.))
			.border_1()
			.border_color(rgb(p.border))
			.bg(rgb(p.elevated))
			.text_color(cx.theme().foreground)
			.shadow_lg()
			.child(
				div()
					.flex()
					.items_center()
					.child(div().flex_1().text_base().font_weight(FontWeight::SEMIBOLD).child("账号与登录"))
					.child(Button::new("accounts-close").ghost().small().label("关闭").on_click(cx.listener(|_, _, _, cx| cx.emit(AccountsEvent::Close)))),
			)
			.child(
				div()
					.text_xs()
					.text_color(rgb(p.muted_foreground))
					.child("凭据保存在 pi 的 auth.json（~/.pi/agent），所有会话共用；新开的会话使用新凭据。退出登录只移除 auth.json 里的凭据，环境变量与 models.json 不受影响。"),
			)
			.children(status)
			.when_some(self.flow.clone(), |d, flow| d.child(self.render_flow(&flow, cx)))
			.when_some(self.message.clone(), |d, message| {
				let (color, text) = match message {
					Message::Info(text) => (p.accent_foreground, text),
					Message::Error(text) => (p.danger, text),
				};
				d.child(div().id("accounts-message").text_xs().text_color(rgb(color)).child(text).test_support())
			})
			.child(
				div()
					.flex()
					.gap_2()
					.child(div().id("accounts-search").flex_1().child(Input::new(&self.search).small()).test_support())
					.child(
						Button::new("accounts-refresh")
							.small()
							.ghost()
							.label("刷新")
							.disabled(!connected)
							.on_click(cx.listener(|this, _, _, cx| this.run("list", cx))),
					),
			)
			.child(div().id("provider-list").flex_1().min_h_0().overflow_y_scroll().children(rows))
	}
}
