//! One session tab: conversation stream, composer, status bar, and the
//! dialog overlay for one `pi --mode rpc` process.

use std::{path::PathBuf, sync::Arc, time::Duration};

use gpui_kit::base::TestSupportExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{
	Enter, Escape, IndentInline, InputEvent, MoveDown, MoveUp, Paste, Textarea, TextareaState,
};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::progress::Progress;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::text::TextView;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Selectable as _, Sizable as _};
use gpui_kit::prelude::*;
use gpui_kit::*;
use pi_bridge::{Session, SessionOptions};
use pi_rpc::{
	AgentMessage, BashResult, Client, Model, Notification, SessionState, SessionStats, SlashCommand,
	StreamingBehavior, ThinkingLevel, UiResponse,
};
use pi_store::display::{Block, Role, View};
use serde_json::{Value, json};

use crate::attachments;
use crate::dialog::{DialogEvent, DialogView};
use crate::session::{Activity, Notice, PendingControl, Queue, QueueEdit, SessionModel};
use crate::slash::{self, Builtin};
use crate::status;
use crate::theme;
use crate::tool_card::{ToolCard, clip_lines, truncate};

#[derive(Clone, Copy)]
enum Delivery {
	Normal,
	Steer,
}

#[derive(Clone, Copy)]
enum CompletionKey {
	Up,
	Down,
	Tab,
	Enter,
	Escape,
}

enum Connection {
	Connecting,
	Connected(Arc<Client>),
	/// Startup failed or the process went away.
	Failed(String),
	/// pi has no model with usable credentials.
	NeedsLogin(String),
}

/// Requests the workspace handles.
pub enum ChatEvent {
	OpenAccounts,
}

impl EventEmitter<ChatEvent> for ChatView {}

/// What a tab shows in the tab bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabStatus {
	Connecting,
	Idle,
	Running,
	/// A dialog waits for the user.
	Attention,
	Disconnected,
}

/// The parts of a tab the workspace renders outside the tab itself.
#[derive(Debug, Clone, PartialEq)]
pub struct TabSummary {
	pub status:       TabStatus,
	pub name:         Option<String>,
	pub session_file: Option<PathBuf>,
	pub cwd:          PathBuf,
}

pub struct ChatView {
	cwd:          PathBuf,
	options:      SessionOptions,
	connection:   Connection,
	/// From `get_state`. For a new session the file appears on disk only
	/// after the first reply.
	session_file: Option<PathBuf>,
	model:        SessionModel,
	stats:        Option<SessionStats>,
	/// Choices for the status-bar menus, fetched once per connection.
	models:       Vec<Model>,
	thinking_levels: Vec<ThinkingLevel>,
	/// The right-hand panel (context, cost, every `get_state` field) is open.
	panel:        bool,
	list:         ListState,
	composer:     Entity<TextareaState>,
	dialog:       Option<Entity<DialogView>>,
	/// Slash-command completions for the composer text, with the selected
	/// row; `None` hides the popup. Esc dismisses it until the text changes.
	completions:  Option<(Vec<slash::Suggestion>, usize)>,
	completions_dismissed: Option<String>,
	/// Images waiting in the composer, sent with the next message.
	attachments:  Vec<attachments::Attachment>,
	/// Decoded transcript images by (row, block); cleared with the list.
	images:       std::cell::RefCell<std::collections::HashMap<(usize, usize), Option<Arc<Image>>>>,
	/// Thinking blocks and tool cards the user expanded, by (row, block).
	expanded:     std::collections::HashSet<(usize, usize)>,
	/// Set when the running `!cmd`'s `bash` call returns; `abort` repeats
	/// `abort_bash` until then.
	shell_done:   Arc<std::sync::atomic::AtomicBool>,
	/// Abort requests sent, for tests that check one key press aborts once.
	#[cfg(test)]
	pub(crate) aborts: usize,
	_tasks:       Vec<Task<()>>,
	_subs:        Vec<Subscription>,
}

impl ChatView {
	/// A tab for `options`: a new session, `options.resume`, or a fork of it
	/// (`options.fork`).
	pub fn new(options: SessionOptions, window: &mut Window, cx: &mut Context<Self>) -> Self {
		let cwd = options.cwd.clone().unwrap_or_else(|| PathBuf::from("."));
		let composer = cx.new(|cx| {
			TextareaState::new(window, cx)
				.placeholder("给 pi 发消息…（Enter 发送，Shift+Enter 换行；运行中 Enter 排队，⌘Enter 插队；!命令 直接运行 shell；⌘V 或拖入可附加图片）")
				.auto_grow(2, 10)
				.submit_on_enter(true)
		});
		composer.update(cx, |state, cx| state.focus(window, cx));
		let sub = cx.subscribe_in(&composer, window, |this, _, event, window, cx| match event {
			InputEvent::PressEnter { secondary: true, .. } => this.send(Delivery::Steer, window, cx),
			InputEvent::PressEnter { shift: false, .. } => this.send(Delivery::Normal, window, cx),
			InputEvent::Change => this.update_completions(cx),
			_ => {},
		});
		let list = ListState::new(0, ListAlignment::Top, px(600.));
		list.set_follow_mode(FollowMode::Tail);

		let mut view = Self {
			cwd,
			session_file: options.resume.as_ref().filter(|_| !options.fork).map(|f| std::fs::canonicalize(f).unwrap_or_else(|_| f.clone())),
			options,
			connection: Connection::Connecting,
			model: SessionModel::default(),
			stats: None,
			models: Vec::new(),
			thinking_levels: Vec::new(),
			panel: false,
			list,
			composer,
			dialog: None,
			attachments: Vec::new(),
			completions: None,
			completions_dismissed: None,
			images: Default::default(),
			expanded: Default::default(),
			shell_done: Default::default(),
			#[cfg(test)]
			aborts: 0,
			_tasks: Vec::new(),
			_subs: vec![sub],
		};
		view.connect(cx);
		view
	}

	fn connect(&mut self, cx: &mut Context<Self>) {
		self.connection = Connection::Connecting;
		let options = self.options.clone();
		// Spawn and load history off the UI thread. Records that arrive
		// meanwhile wait in the channel and apply after the history.
		let spawn = cx.background_spawn(crate::blocking::blocking(move || {
			let session = Session::spawn(&options)?;
			let history: Vec<AgentMessage> = if options.resume.is_some() {
				let data = session.client.request("get_messages", Value::Null, Some(Duration::from_secs(60)))?;
				serde_json::from_value(data.get("messages").cloned().unwrap_or_default()).map_err(pi_rpc::Error::Json)?
			} else {
				Vec::new()
			};
			Ok::<_, pi_bridge::BridgeError>((session, history))
		}));
		let task = cx.spawn(async move |this, cx| {
			let (session, history) = match spawn.await {
				Ok(started) => started,
				Err(error) => {
					_ = this.update(cx, |this, cx| {
						this.connection = if error.needs_login() {
							Connection::NeedsLogin(error.to_string())
						} else {
							Connection::Failed(error.to_string())
						};
						cx.notify();
					});
					return;
				},
			};
			let events = session.events.clone();
			let client = session.client.clone();
			_ = this.update(cx, |this, cx| {
				if !history.is_empty() {
					this.model.load_history(history);
					this.images.borrow_mut().clear();
					this.list.reset(this.model.row_count());
				}
				this.apply_state(session.state);
				this.connection = Connection::Connected(client);
				this.refresh_state(cx);
				this.load_choices(cx);
				this.load_commands(cx);
				cx.notify();
			});
			while let Ok(notification) = events.recv().await {
				if this.update(cx, |this, cx| this.on_notification(notification, cx)).is_err() {
					return;
				}
			}
			_ = this.update(cx, |this, cx| {
				this.connection = Connection::Failed("pi 进程已退出".into());
				this.model.activity = Activity::Idle;
				this.model.transcript.stop_streaming();
				this.model.dialogs.clear();
				this.dialog = None;
				cx.notify();
			});
		});
		self._tasks.push(task);
	}

	/// Restarts the process after it exited. A session that reached disk
	/// continues from its file (`--session`); otherwise a new one starts.
	fn reconnect(&mut self, cx: &mut Context<Self>) {
		if let Some(file) = self.session_file.clone().filter(|file| file.is_file()) {
			self.options.resume = Some(file);
		}
		self.options.fork = false;
		self.model = SessionModel::default();
		self.stats = None;
		self.dialog = None;
		self.images.borrow_mut().clear();
		self.list.reset(0);
		self.connect(cx);
		cx.notify();
	}

	/// Retries a tab that could not start, after credentials changed.
	pub fn reconnect_if_failed(&mut self, cx: &mut Context<Self>) {
		if matches!(self.connection, Connection::Failed(_) | Connection::NeedsLogin(_)) && self.model.row_count() == 0 {
			self.reconnect(cx);
		}
	}

	/// Prompts for a working directory and restarts this empty session in it.
	fn prompt_change_cwd(&mut self, cx: &mut Context<Self>) {
		let rx = cx.prompt_for_paths(PathPromptOptions {
			files:       false,
			directories: true,
			multiple:    false,
			prompt:      Some("选择工作目录".into()),
		});
		cx.spawn(async move |this, cx| {
			if let Ok(Ok(Some(mut paths))) = rx.await
				&& let Some(dir) = paths.pop()
			{
				_ = this.update(cx, |this, cx| this.set_cwd(dir, cx));
			}
		})
		.detach();
	}

	pub fn set_cwd(&mut self, cwd: PathBuf, cx: &mut Context<Self>) {
		let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);
		if self.cwd == cwd || self.model.row_count() > 0 {
			return;
		}
		self.shutdown(cx);
		self.cwd = cwd.clone();
		self.options.cwd = Some(cwd);
		self.options.resume = None;
		self.session_file = None;
		self.reconnect(cx);
	}

	/// Stops the process. Teardown waits for pi to exit, so the last client
	/// handle is dropped off the UI thread.
	pub fn shutdown(&mut self, cx: &mut Context<Self>) {
		let old = std::mem::replace(&mut self.connection, Connection::Failed("已关闭".into()));
		self._tasks.clear();
		if let Connection::Connected(client) = old {
			cx.background_spawn(crate::blocking::blocking(move || drop(client))).detach();
		}
	}

	pub fn summary(&self) -> TabSummary {
		let status = match &self.connection {
			Connection::Connecting => TabStatus::Connecting,
			Connection::Failed(_) | Connection::NeedsLogin(_) => TabStatus::Disconnected,
			Connection::Connected(_) if self.model.needs_attention() => TabStatus::Attention,
			Connection::Connected(_) if self.model.busy() => TabStatus::Running,
			Connection::Connected(_) => TabStatus::Idle,
		};
		let name = self.model.title.clone().or_else(|| self.model.state.as_ref().and_then(|s| s.session_name.clone()));
		TabSummary {
			status,
			name: name.filter(|n| !n.trim().is_empty()),
			session_file: self.session_file.clone(),
			cwd: self.cwd.clone(),
		}
	}

	pub fn focus_composer(&self, window: &mut Window, cx: &mut App) {
		self.composer.update(cx, |state, cx| state.focus(window, cx));
	}

	fn client(&self) -> Option<Arc<Client>> {
		match &self.connection {
			Connection::Connected(client) => Some(client.clone()),
			_ => None,
		}
	}

	#[cfg(test)]
	pub(crate) fn is_connected(&self) -> bool {
		matches!(self.connection, Connection::Connected(_))
	}

	#[cfg(test)]
	pub(crate) fn is_disconnected(&self) -> bool {
		matches!(self.connection, Connection::Failed(_) | Connection::NeedsLogin(_))
	}

	#[cfg(test)]
	pub(crate) fn needs_login(&self) -> bool {
		matches!(self.connection, Connection::NeedsLogin(_))
	}

	#[cfg(test)]
	pub(crate) fn pid(&self) -> Option<u32> {
		self.client()?.pid()
	}

	#[cfg(test)]
	pub(crate) fn queue_log(&self) -> &[crate::session::Queue] {
		&self.model.queue_log
	}

	#[cfg(test)]
	pub(crate) fn session_file(&self) -> Option<&std::path::Path> {
		self.session_file.as_deref()
	}

	#[cfg(test)]
	pub(crate) fn notices(&self) -> &[Notice] {
		&self.model.notices
	}

	#[cfg(test)]
	pub(crate) fn open_dialog_kind(&self, cx: &App) -> Option<&'static str> {
		Some(self.dialog.as_ref()?.read(cx).kind())
	}

	#[cfg(test)]
	pub(crate) fn open_dialog_title(&self, cx: &App) -> Option<String> {
		Some(self.dialog.as_ref()?.read(cx).title())
	}

	#[cfg(test)]
	pub(crate) fn is_idle(&self) -> bool {
		!self.model.busy()
	}

	#[cfg(test)]
	pub(crate) fn transcript(&self) -> &pi_store::Transcript {
		&self.model.transcript
	}

	#[cfg(test)]
	pub(crate) fn completion_labels(&self) -> Option<(Vec<String>, usize)> {
		self.completions.as_ref().map(|(rows, selected)| (rows.iter().map(|r| r.label.clone()).collect(), *selected))
	}

	#[cfg(test)]
	pub(crate) fn command_count(&self) -> usize {
		self.model.commands.len()
	}

	#[cfg(test)]
	pub(crate) fn attachment_labels(&self) -> Vec<String> {
		self.attachments.iter().map(attachments::Attachment::label).collect()
	}

	#[cfg(test)]
	pub(crate) fn composer_text(&self, cx: &App) -> String {
		self.composer.read(cx).value().to_string()
	}

	#[cfg(test)]
	pub(crate) fn composer(&self) -> Entity<TextareaState> {
		self.composer.clone()
	}

	/// Labels of the open `select` dialog, if one is shown.
	#[cfg(test)]
	pub(crate) fn open_dialog_options(&self, cx: &App) -> Option<Vec<String>> {
		self.dialog.as_ref()?.read(cx).select_options()
	}

	#[cfg(test)]
	pub(crate) fn queue(&self) -> &crate::session::Queue {
		&self.model.queue
	}

	#[cfg(test)]
	pub(crate) fn state(&self) -> Option<&SessionState> {
		self.model.state.as_ref()
	}

	#[cfg(test)]
	pub(crate) fn statuses(&self) -> &std::collections::BTreeMap<String, String> {
		&self.model.statuses
	}

	#[cfg(test)]
	pub(crate) fn choices(&self) -> (usize, usize) {
		(self.models.len(), self.thinking_levels.len())
	}

	#[cfg(test)]
	pub(crate) fn thinking_levels_for_test(&self) -> Vec<ThinkingLevel> {
		self.thinking_levels.clone()
	}

	#[cfg(test)]
	pub(crate) fn models_for_test(&self) -> Vec<(String, String)> {
		self.models.iter().map(|m| (m.provider.clone(), m.id.clone())).collect()
	}

	#[cfg(test)]
	pub(crate) fn has_stats(&self) -> bool {
		self.stats.is_some()
	}

	#[cfg(test)]
	pub(crate) fn control_pending(&self) -> bool {
		self.model.pending.is_some()
	}

	#[cfg(test)]
	pub(crate) fn tool_runs(&self) -> &pi_store::tools::ToolRuns {
		&self.model.tools
	}

	fn on_notification(&mut self, notification: Notification, cx: &mut Context<Self>) {
		let effects = self.model.apply(notification);
		self.apply_rows(effects.rows);
		if effects.refresh_state {
			self.refresh_state(cx);
		}
		if let Some(text) = self.model.editor_text.take() {
			self.set_composer(text, cx);
		}
		self.show_next_dialog(cx);
		cx.notify();
	}

	fn apply_rows(&mut self, rows: Option<(usize, usize)>) {
		let Some((first, count)) = rows else { return };
		let old = self.list.item_count();
		if count != old {
			self.list.splice(old.min(first)..old, count - old.min(first));
		} else {
			self.list.remeasure_items(first..count);
		}
	}

	fn set_composer(&mut self, text: String, cx: &mut Context<Self>) {
		let Some(window) = cx.windows().first().copied() else { return };
		let composer = self.composer.clone();
		_ = window.update(cx, |_, window, cx| {
			composer.update(cx, |state, cx| {
				let column = text.lines().last().map_or(0, |l| l.chars().count()) as u32;
				let row = text.lines().count().saturating_sub(1) as u32;
				state.set_value(text, window, cx);
				state.set_cursor_position(gpui_kit::component::input::Position::new(row, column), window, cx);
			});
		});
	}

	fn apply_state(&mut self, state: SessionState) {
		if let Some(file) = &state.session_file {
			let file = PathBuf::from(file);
			// Canonical once the file exists, to match the session list.
			self.session_file = Some(std::fs::canonicalize(&file).unwrap_or(file));
		}
		self.model.compacting = state.is_compacting;
		self.model.state = Some(state);
	}

	fn refresh_state(&mut self, cx: &mut Context<Self>) {
		let Some(client) = self.client() else {
			return;
		};
		let fetch = cx.background_spawn(crate::blocking::blocking(move || {
			(client.call::<SessionState>("get_state", Value::Null).ok(), client.call::<SessionStats>("get_session_stats", Value::Null).ok())
		}));
		cx.spawn(async move |this, cx| {
			let (state, stats) = fetch.await;
			_ = this.update(cx, |this, cx| {
				if let Some(state) = state {
					this.apply_state(state);
				}
				if let Some(stats) = stats {
					this.stats = Some(stats);
				}
				cx.notify();
			});
		})
		.detach();
	}

	fn show_next_dialog(&mut self, cx: &mut Context<Self>) {
		if self.dialog.is_some() {
			// pi may have answered the open dialog itself (timeout).
			let open = self.dialog.as_ref().map(|d| d.read(cx).id().to_owned()).unwrap_or_default();
			if self.model.dialogs.iter().any(|d| d.dialog_id() == Some(open.as_str())) {
				return;
			}
			self.dialog = None;
		}
		let Some(request) = self.model.dialogs.first().cloned() else {
			return;
		};
		let Some(window) = cx.windows().first().copied() else {
			return;
		};
		let this = cx.entity().downgrade();
		_ = window.update(cx, |_, window, cx| {
			let view = cx.new(|cx| DialogView::new(request, window, cx));
			let answer = this.clone();
			cx.subscribe(&view, move |_, event: &DialogEvent, cx| {
				let DialogEvent::Answered(response) = event;
				let response = response.clone();
				_ = answer.update(cx, |this, cx| this.answer_dialog(response, cx));
			})
			.detach();
			// pi resolves a timed-out dialog with its default and sends nothing;
			// close it here at the same moment.
			if let Some(deadline) = view.read(cx).deadline() {
				let id = view.read(cx).id().to_owned();
				let this = this.clone();
				// A thread sleeps on the wall clock, as pi's own timer does.
				let (tx, rx) = async_channel::bounded::<()>(1);
				let wait = deadline.saturating_duration_since(std::time::Instant::now());
				std::thread::spawn(move || {
					std::thread::sleep(wait);
					_ = tx.send_blocking(());
				});
				cx.spawn(async move |cx| {
					_ = rx.recv().await;
					_ = this.update(cx, |this, cx| {
						this.model.dialog_answered(&id);
						this.show_next_dialog(cx);
						cx.notify();
					});
				})
				.detach();
			}
			self.dialog = Some(view);
		});
	}

	fn answer_dialog(&mut self, response: UiResponse, cx: &mut Context<Self>) {
		if let Some(client) = self.client()
			&& let Err(error) = client.send(&response.to_wire())
		{
			self.model.notices.push(Notice::Error(format!("回复对话框失败：{error}")));
		}
		self.model.dialog_answered(response.id());
		self.dialog = None;
		self.show_next_dialog(cx);
		cx.notify();
	}

	fn send(&mut self, delivery: Delivery, window: &mut Window, cx: &mut Context<Self>) {
		let text = self.composer.read(cx).value().trim().to_owned();
		let Some(client) = self.client() else {
			return;
		};
		if text.is_empty() && self.attachments.is_empty() {
			return;
		}
		// Desktop commands and `!shell` lines run here, not as prompts.
		if self.attachments.is_empty() {
			if let Some((builtin, args)) = slash::builtin(&text) {
				let args = args.to_owned();
				self.clear_composer(window, cx);
				self.run_builtin(builtin, args, cx);
				return;
			}
			if let Some(command) = text.strip_prefix('!') {
				if self.model.busy() {
					self.model.notices.push(Notice::Info("运行中不能执行 shell 命令；等待结束或先停止".into()));
					cx.notify();
					return;
				}
				// `!!cmd` keeps the output out of the model's context.
				let (command, exclude) = match command.strip_prefix('!') {
					Some(rest) => (rest.trim().to_owned(), true),
					None => (command.trim().to_owned(), false),
				};
				if !command.is_empty() {
					self.clear_composer(window, cx);
					self.run_shell(command, exclude, cx);
				}
				return;
			}
		}
		let images = std::mem::take(&mut self.attachments);
		let wire: Vec<Value> = images.iter().map(|a| a.to_content().to_wire()).collect();
		// While the agent works, a message queues: as a follow-up for after
		// the run, or as a steer delivered after the current tool calls.
		let streaming_behavior = self.model.busy().then_some(match delivery {
			Delivery::Normal => StreamingBehavior::FollowUp,
			Delivery::Steer => StreamingBehavior::Steer,
		});
		self.clear_composer(window, cx);
		let was_busy = self.model.busy();
		self.model.prompt_sent();
		let mut params = json!({"message": text});
		if !wire.is_empty() {
			params["images"] = Value::Array(wire);
		}
		if let Some(behavior) = streaming_behavior {
			params["streamingBehavior"] = serde_json::to_value(behavior).unwrap_or_default();
		}
		// No deadline: pi answers an extension command only when its handler
		// returns, which can wait on the user. A process exit fails the call.
		let call = cx.background_spawn(crate::blocking::blocking(move || client.request("prompt", params, None)));
		cx.spawn(async move |this, cx| {
			let error = match call.await {
				// An extension command or input handler consumed the prompt; no
				// run follows.
				Ok(data) if data.get("disposition").and_then(Value::as_str) == Some("handled") && !was_busy => {
					_ = this.update(cx, |this, cx| {
						this.model.activity = Activity::Idle;
						this.refresh_state(cx);
						this.load_commands(cx);
						cx.notify();
					});
					return;
				},
				Ok(_) => return,
				Err(error) => error,
			};
			_ = this.update(cx, |this, cx| {
				this.model.notices.push(Notice::Error(format!("发送失败：{}", command_error(&error))));
				if !was_busy {
					this.model.activity = Activity::Idle;
				}
				// Keep the images so the user can resend.
				if this.attachments.is_empty() {
					this.attachments = images;
				}
				cx.notify();
			});
		})
		.detach();
		cx.notify();
	}

	fn clear_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
		self.composer.update(cx, |state, cx| state.clean(window, cx));
		self.completions = None;
	}

	/// `!cmd`: runs a shell command through pi (`bash`); its output joins the
	/// context of the next prompt unless `exclude` (`!!cmd`).
	fn run_shell(&mut self, command: String, exclude: bool, cx: &mut Context<Self>) {
		let Some(client) = self.client() else { return };
		let effects = self.model.shell_started(&command, exclude);
		self.apply_rows(effects.rows);
		let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
		self.shell_done = done.clone();
		let call = cx.background_spawn(crate::blocking::blocking(move || {
			let result = client
				.request("bash", json!({"command": command, "excludeFromContext": exclude}), None)
				.and_then(|data| serde_json::from_value::<BashResult>(data).map_err(pi_rpc::Error::Json));
			done.store(true, std::sync::atomic::Ordering::Release);
			result
		}));
		cx.spawn(async move |this, cx| {
			let result = call.await;
			_ = this.update(cx, |this, cx| {
				let ok = match result {
					Ok(result) => Some(result),
					Err(error) => {
						this.model.notices.push(Notice::Error(format!("shell 命令失败：{}", command_error(&error))));
						None
					},
				};
				let effects = this.model.shell_finished(ok);
				this.apply_rows(effects.rows);
				this.refresh_state(cx);
				cx.notify();
			});
		})
		.detach();
		cx.notify();
	}

	fn run_builtin(&mut self, builtin: Builtin, args: String, cx: &mut Context<Self>) {
		match builtin {
			Builtin::Compact => self.compact(Some(args).filter(|a| !a.is_empty()), cx),
			Builtin::New => self.session_command("new_session", Value::Null, "新会话", cx),
			Builtin::Clone => self.session_command("clone", Value::Null, "复制会话", cx),
			Builtin::Name if args.is_empty() => self.local_output("用法：/name <名称>", cx),
			Builtin::Name => self.simple_call("set_session_name", json!({"name": args}), "设置名称", cx),
			Builtin::Export => {
				let params = if args.is_empty() { Value::Null } else { json!({"outputPath": args}) };
				self.query("export_html", params, "导出", cx, |data, _| {
					format!("已导出：{}", data.get("path").and_then(Value::as_str).unwrap_or("?"))
				});
			},
			Builtin::Copy => self.query("get_last_assistant_text", Value::Null, "复制", cx, |data, cx| {
				match data.get("text").and_then(Value::as_str) {
					Some(text) => {
						cx.write_to_clipboard(ClipboardItem::new_string(text.to_owned()));
						"已复制最后一条回复".into()
					},
					None => "还没有回复可以复制".into(),
				}
			}),
			Builtin::Session => self.query("get_session_stats", Value::Null, "会话统计", cx, |data, _| {
				let stats: SessionStats = serde_json::from_value(data.clone()).unwrap_or_default();
				let file = data.get("sessionFile").and_then(Value::as_str).unwrap_or("（未保存）");
				format!(
					"文件 {file}\n消息 {}（用户 {} · 助手 {}）· 工具调用 {}\ntoken 输入 {} · 输出 {} · 缓存读/写 {}/{}\n费用 ${:.4}{}",
					stats.total_messages,
					stats.user_messages,
					stats.assistant_messages,
					stats.tool_calls,
					status::tokens(stats.tokens.input),
					status::tokens(stats.tokens.output),
					status::tokens(stats.tokens.cache_read),
					status::tokens(stats.tokens.cache_write),
					stats.cost,
					status::context_label(&stats).map(|c| format!("\n上下文 {c}")).unwrap_or_default(),
				)
			}),
		}
	}

	fn local_output(&mut self, text: &str, cx: &mut Context<Self>) {
		let row = self.model.transcript.push_command_output(text);
		self.apply_rows(Some((row, self.model.row_count())));
		cx.notify();
	}

	/// Runs a read-only command and shows its result as a local row.
	fn query(
		&mut self,
		command: &'static str,
		params: Value,
		label: &'static str,
		cx: &mut Context<Self>,
		format: impl FnOnce(&Value, &mut App) -> String + 'static,
	) {
		let Some(client) = self.client() else { return };
		let call = cx.background_spawn(crate::blocking::blocking(move || client.request(command, params, Some(Duration::from_secs(60)))));
		cx.spawn(async move |this, cx| {
			let result = call.await;
			_ = this.update(cx, |this, cx| {
				match result {
					Ok(data) => {
						let text = format(&data, cx);
						this.local_output(&text, cx);
					},
					Err(error) => this.model.notices.push(Notice::Error(format!("{label}失败：{}", command_error(&error)))),
				}
				cx.notify();
			});
		})
		.detach();
	}

	fn simple_call(&mut self, command: &'static str, params: Value, label: &'static str, cx: &mut Context<Self>) {
		let Some(client) = self.client() else { return };
		let call = cx.background_spawn(crate::blocking::blocking(move || client.request(command, params, Some(Duration::from_secs(60)))));
		cx.spawn(async move |this, cx| {
			let result = call.await;
			_ = this.update(cx, |this, cx| {
				if let Err(error) = result {
					this.model.notices.push(Notice::Error(format!("{label}失败：{}", command_error(&error))));
				}
				this.refresh_state(cx);
				cx.notify();
			});
		})
		.detach();
	}

	/// `new_session` / `clone`: pi switches this process to another session;
	/// the transcript is reloaded from it.
	fn session_command(&mut self, command: &'static str, params: Value, label: &'static str, cx: &mut Context<Self>) {
		let Some(client) = self.client() else { return };
		if self.model.busy() {
			self.model.notices.push(Notice::Info(format!("运行中不能{label}；先停止")));
			cx.notify();
			return;
		}
		let call = cx.background_spawn(crate::blocking::blocking(move || {
			let data = client.request(command, params, Some(Duration::from_secs(60)))?;
			let messages = client.request("get_messages", Value::Null, Some(Duration::from_secs(60)))?;
			Ok::<_, pi_rpc::Error>((data, messages))
		}));
		cx.spawn(async move |this, cx| {
			let result = call.await;
			_ = this.update(cx, |this, cx| {
				match result {
					Ok((data, _)) if data.get("cancelled").and_then(Value::as_bool) == Some(true) => {
						this.model.notices.push(Notice::Info(format!("{label}被扩展取消")));
					},
					Ok((_, messages)) => {
						let messages: Vec<AgentMessage> =
							serde_json::from_value(messages.get("messages").cloned().unwrap_or_default()).unwrap_or_default();
						this.model = SessionModel { commands: std::mem::take(&mut this.model.commands), ..Default::default() };
						this.session_file = None;
						this.model.load_history(messages);
						this.images.borrow_mut().clear();
						this.expanded.clear();
						this.list.reset(this.model.row_count());
						this.load_commands(cx);
					},
					Err(error) => this.model.notices.push(Notice::Error(format!("{label}失败：{}", command_error(&error)))),
				}
				this.refresh_state(cx);
				cx.notify();
			});
		})
		.detach();
	}

	/// Adds pasted or dropped images to the composer; reports what was
	/// skipped. Returns whether the paste carried anything besides text.
	fn attach(&mut self, pasted: attachments::Pasted, cx: &mut Context<Self>) -> bool {
		if pasted.is_empty() {
			return false;
		}
		if !pasted.rejected.is_empty() {
			self.model.notices.push(Notice::Info(format!("未附加：{}", pasted.rejected.join("、"))));
		}
		for image in pasted.images {
			// The same bitmap pasted twice attaches once.
			if !self.attachments.iter().any(|a| a.image.id == image.image.id) {
				self.attachments.push(image);
			}
		}
		cx.notify();
		true
	}

	fn remove_attachment(&mut self, index: usize, cx: &mut Context<Self>) {
		if index < self.attachments.len() {
			self.attachments.remove(index);
		}
		cx.notify();
	}

	fn render_attachments(&self, cx: &mut Context<Self>) -> impl IntoElement {
		let p = theme::palette(cx);
		div().flex().flex_wrap().gap_2().children(self.attachments.iter().enumerate().map(|(index, attachment)| {
			div()
				.id(("attachment", index))
				.relative()
				.flex()
				.flex_col()
				.items_center()
				.gap_0p5()
				.p_1()
				.rounded(px(6.))
				.border_1()
				.border_color(rgb(p.border))
				.bg(rgb(p.elevated))
				.child(img(attachment.image.clone()).size(px(56.)).object_fit(ObjectFit::Cover).rounded(px(4.)))
				.child(div().max_w(px(64.)).truncate().text_xs().text_color(rgb(p.muted_foreground)).child(attachment.label()))
				.child(
					div().absolute().top_0().right_0().child(
						Button::new(("attachment-remove", index))
							.ghost()
							.xsmall()
							.label("×")
							.tooltip("移除")
							.on_click(cx.listener(move |this, _, _, cx| this.remove_attachment(index, cx))),
					),
				)
				.test_support()
		}))
	}

	fn abort(&mut self, cx: &mut Context<Self>) {
		let Some(client) = self.client() else {
			return;
		};
		#[cfg(test)]
		{
			self.aborts += 1;
		}
		let shell = (self.model.activity == Activity::Shell).then(|| self.shell_done.clone());
		let call = cx.background_spawn(crate::blocking::blocking(move || {
			if let Some(done) = shell {
				// pi registers the command's abort handle only once the shell
				// starts, and `abort_bash` for a command not yet started is a
				// no-op; repeat it until the `bash` call returns, for at most 10s.
				let deadline = std::time::Instant::now() + Duration::from_secs(10);
				while !done.load(std::sync::atomic::Ordering::Acquire) {
					if std::time::Instant::now() >= deadline {
						return Err(pi_rpc::Error::Timeout { command: "abort_bash".into() });
					}
					client.request("abort_bash", Value::Null, Some(Duration::from_secs(10)))?;
					std::thread::sleep(Duration::from_millis(100));
				}
				return Ok(None);
			}
			// Esc in the TUI: take the queued messages back into the editor so
			// `abort` does not run them next.
			let queued = client.request("clear_queue", Value::Null, Some(Duration::from_secs(10)))?;
			client.request("abort", Value::Null, Some(Duration::from_secs(60)))?;
			Ok(Some(queued))
		}));
		cx.spawn(async move |this, cx| {
			let result = call.await;
			_ = this.update(cx, |this, cx| {
				// pi resolves dialogs tied to the aborted run without telling
				// the client; answering them later would go nowhere.
				if matches!(result, Ok(Some(_))) {
					this.model.dialogs.clear();
					this.dialog = None;
				}
				match result {
					Ok(Some(queued)) => {
						let restored: Vec<String> = ["steering", "followUp"]
							.iter()
							.flat_map(|key| queued.get(*key).and_then(Value::as_array).cloned().unwrap_or_default())
							.filter_map(|v| v.as_str().map(str::to_owned))
							.collect();
						if !restored.is_empty() {
							let current = this.composer.read(cx).value().to_string();
							let text = std::iter::once(current).filter(|c| !c.trim().is_empty()).chain(restored).collect::<Vec<_>>().join("\n");
							this.set_composer(text, cx);
						}
					},
					Ok(None) => {},
					Err(error) => this.model.notices.push(Notice::Error(format!("停止失败：{}", command_error(&error)))),
				}
				cx.notify();
			});
		})
		.detach();
	}

	fn update_completions(&mut self, cx: &mut Context<Self>) {
		let text = self.composer.read(cx).value().to_string();
		if self.completions_dismissed.as_deref() != Some(text.as_str()) {
			self.completions_dismissed = None;
		}
		let rows = if self.completions_dismissed.is_some() {
			None
		} else {
			slash::suggest(&self.model.commands, &text).filter(|rows| !rows.is_empty())
		};
		let selected = match (&self.completions, &rows) {
			// Keep the selection on the same row while typing narrows the list.
			(Some((old, index)), Some(new)) => {
				old.get(*index).and_then(|row| new.iter().position(|r| r.label == row.label)).unwrap_or(0)
			},
			_ => 0,
		};
		self.completions = rows.map(|rows| (rows, selected));
		cx.notify();
	}

	/// ↑/↓ move, Tab/Enter accept, Esc closes, while the popup is open;
	/// stops the key's action when it was used.
	fn completion_key(&mut self, key: CompletionKey, window: &mut Window, cx: &mut Context<Self>) {
		let Some((count, selected)) = self.completions.as_ref().map(|(rows, selected)| (rows.len(), *selected)) else {
			return;
		};
		let select = |this: &mut Self, index: usize| {
			if let Some((_, selected)) = &mut this.completions {
				*selected = index;
			}
		};
		match key {
			CompletionKey::Up => select(self, selected.checked_sub(1).unwrap_or(count - 1)),
			CompletionKey::Down => select(self, (selected + 1) % count),
			CompletionKey::Tab => self.accept_completion(selected, window, cx),
			CompletionKey::Enter => {
				// Enter on a fully typed command sends it; otherwise it completes.
				let insert = self.completions.as_ref().map(|(rows, _)| rows[selected].insert.clone()).unwrap_or_default();
				let text = self.composer.read(cx).value().to_string();
				if insert.trim_end() == text.trim_end() {
					self.completions = None;
					return;
				}
				self.accept_completion(selected, window, cx)
			},
			CompletionKey::Escape => {
				self.completions = None;
				self.completions_dismissed = Some(self.composer.read(cx).value().to_string());
			},
		}
		cx.stop_propagation();
		cx.notify();
	}

	fn accept_completion(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
		let Some(row) = self.completions.as_ref().and_then(|(rows, _)| rows.get(index)).cloned() else {
			return;
		};
		self.composer.update(cx, |state, cx| {
			state.set_value(row.insert.clone(), window, cx);
			let column = row.insert.chars().count() as u32;
			state.set_cursor_position(gpui_kit::component::input::Position::new(0, column), window, cx);
		});
		// `set_value` emits no change event.
		self.update_completions(cx);
	}

	fn render_completions(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
		let (rows, selected) = self.completions.as_ref()?;
		let p = theme::palette(cx);
		let selected = *selected;
		Some(
			div()
				.id("slash-popup")
				.absolute()
				.bottom_full()
				.left_0()
				.mb_1()
				.w(px(520.))
				.max_h(px(280.))
				.overflow_y_scroll()
				.rounded(px(8.))
				.border_1()
				.border_color(rgb(p.border))
				.bg(rgb(p.elevated))
				.shadow_lg()
				.py_1()
				.children(rows.iter().enumerate().map(|(index, row)| {
					div()
						.id(("slash-row", index))
						.flex()
						.items_baseline()
						.gap_2()
						.px_3()
						.py_1()
						.text_xs()
						.cursor_pointer()
						.when(index == selected, |r| r.bg(rgb(p.accent)))
						.hover(|r| r.bg(rgb(p.muted)))
						.child(div().flex_none().font_family("Menlo").text_color(rgb(p.foreground)).child(row.label.clone()))
						.when_some(row.hint.clone(), |r, hint| {
							r.child(div().flex_none().font_family("Menlo").text_color(rgb(p.muted_foreground)).child(hint))
						})
						.child(
							div()
								.flex_1()
								.min_w_0()
								.truncate()
								.text_color(rgb(p.muted_foreground))
								.child(row.description.clone().unwrap_or_default()),
						)
						.child(div().flex_none().text_color(rgb(p.muted_foreground)).child(row.source.label()))
						.on_click(cx.listener(move |this, _, window, cx| {
							this.accept_completion(index, window, cx);
							this.focus_composer(window, cx);
						}))
						.test_support()
				}))
				.test_support(),
		)
	}

	/// Esc in an empty composer stops the agent; with text it keeps the
	/// input's own handling. Text an IME is composing is already part of the
	/// value, so Esc that cancels a composition does not abort.
	fn on_escape(&mut self, cx: &mut Context<Self>) {
		if self.model.busy() && self.composer.read(cx).value().is_empty() {
			self.abort(cx);
		}
	}

	/// Model and thinking-level choices for the status-bar menus.
	fn load_choices(&mut self, cx: &mut Context<Self>) {
		let Some(client) = self.client() else {
			return;
		};
		let fetch = cx.background_spawn(crate::blocking::blocking(move || {
			let models = client
				.request("get_available_models", Value::Null, Some(Duration::from_secs(60)))
				.ok()
				.and_then(|data| serde_json::from_value::<Vec<Model>>(data.get("models")?.clone()).ok());
			let levels = client
				.request("get_available_thinking_levels", Value::Null, Some(Duration::from_secs(60)))
				.ok()
				.and_then(|data| serde_json::from_value::<Vec<ThinkingLevel>>(data.get("levels")?.clone()).ok());
			(models, levels)
		}));
		cx.spawn(async move |this, cx| {
			let (models, levels) = fetch.await;
			_ = this.update(cx, |this, cx| {
				this.models = models.unwrap_or_default();
				this.thinking_levels = levels.unwrap_or_default();
				cx.notify();
			});
		})
		.detach();
	}

	/// The slash-command catalog: extension commands, prompt templates, skills.
	fn load_commands(&mut self, cx: &mut Context<Self>) {
		let Some(client) = self.client() else {
			return;
		};
		let fetch = cx.background_spawn(crate::blocking::blocking(move || {
			client
				.request("get_commands", Value::Null, Some(Duration::from_secs(60)))
				.ok()
				.and_then(|data| serde_json::from_value::<Vec<SlashCommand>>(data.get("commands")?.clone()).ok())
		}));
		cx.spawn(async move |this, cx| {
			if let Some(commands) = fetch.await {
				_ = this.update(cx, |this, cx| {
					this.model.commands = commands;
					cx.notify();
				});
			}
		})
		.detach();
	}

	/// Runs a control command off the UI thread with its controls disabled,
	/// then refetches the state; `label` names it in the error notice.
	fn control(
		&mut self,
		pending: PendingControl,
		label: &'static str,
		call: impl FnOnce(&Client) -> Result<Value, pi_rpc::Error> + Send + 'static,
		cx: &mut Context<Self>,
	) {
		let Some(client) = self.client() else {
			return;
		};
		if self.model.pending.is_some() {
			return;
		}
		self.model.pending = Some(pending);
		cx.notify();
		let call = cx.background_spawn(crate::blocking::blocking(move || call(&client)));
		cx.spawn(async move |this, cx| {
			let result = call.await;
			_ = this.update(cx, |this, cx| {
				this.model.pending = None;
				match result {
					Err(pi_rpc::Error::Command { error, .. }) if pending == PendingControl::Compact => {
						this.model.notices.push(Notice::Info(format!("压缩未执行：{error}")));
					},
					Err(error) => this.model.notices.push(Notice::Error(format!("{label}失败：{}", command_error(&error)))),
					Ok(_) => {},
				}
				if pending == PendingControl::Model {
					// The thinking levels depend on the model.
					this.load_choices(cx);
				}
				this.refresh_state(cx);
				cx.notify();
			});
		})
		.detach();
	}

	fn set_model(&mut self, provider: String, model_id: String, cx: &mut Context<Self>) {
		self.control(
			PendingControl::Model,
			"切换模型",
			move |c| c.request("set_model", json!({"provider": provider, "modelId": model_id}), Some(Duration::from_secs(30))),
			cx,
		);
	}

	fn set_thinking(&mut self, level: ThinkingLevel, cx: &mut Context<Self>) {
		self.control(
			PendingControl::Thinking,
			"切换思考档位",
			move |c| c.request("set_thinking_level", json!({"level": level}), Some(Duration::from_secs(30))),
			cx,
		);
	}

	fn cycle_thinking(&mut self, cx: &mut Context<Self>) {
		self.control(
			PendingControl::Thinking,
			"切换思考档位",
			|c| c.request("cycle_thinking_level", Value::Null, Some(Duration::from_secs(30))),
			cx,
		);
	}

	/// Palette entries that depend on this tab: models, thinking levels, and
	/// slash commands, with the current model and level marked.
	pub fn palette_entries(&self) -> Vec<crate::palette::Entry> {
		use crate::palette::{Command, Entry, Group};
		if !matches!(self.connection, Connection::Connected(_)) {
			return Vec::new();
		}
		let state = self.model.state.as_ref();
		let current = state.and_then(|s| s.model.as_ref());
		let level = state.and_then(|s| s.thinking_level);
		let mut entries: Vec<Entry> = self
			.models
			.iter()
			.map(|m| {
				let name = m.name.trim();
				let has_distinct_name = !name.is_empty() && name != m.id;
				Entry::new(Group::Model, m.id.clone(), Command::SetModel { provider: m.provider.clone(), id: m.id.clone() })
					.subtitle(has_distinct_name.then(|| name.to_owned()))
					.provider(m.provider.clone())
					.detail(if has_distinct_name { format!("{} · {}", m.provider, name) } else { m.provider.clone() })
					.current(current.is_some_and(|c| c.provider == m.provider && c.id == m.id))
			})
			.collect();
		entries.extend(self.thinking_levels.iter().map(|&l| {
			Entry::new(Group::Thinking, format!("思考 {}", l.as_str()), Command::SetThinking(l)).current(level == Some(l))
		}));
		entries.extend(slash::BUILTINS.iter().map(|(_, name, description, _)| {
			Entry::new(Group::Slash, format!("/{name}"), Command::Slash((*name).to_owned())).detail(*description)
		}));
		entries.extend(self.model.commands.iter().map(|c| {
			Entry::new(Group::Slash, format!("/{}", c.name), Command::Slash(c.name.clone())).detail(c.description.clone().unwrap_or_default())
		}));
		entries
	}

	/// Runs a palette command that belongs to this tab.
	pub fn run_palette(&mut self, command: crate::palette::Command, window: &mut Window, cx: &mut Context<Self>) {
		use crate::palette::Command;
		match command {
			Command::Compact => self.compact(None, cx),
			Command::CycleThinking => self.cycle_thinking(cx),
			Command::TogglePanel => self.toggle_panel(cx),
			Command::SetModel { provider, id } => self.set_model(provider, id, cx),
			Command::SetThinking(level) => self.set_thinking(level, cx),
			Command::Slash(name) => {
				let text = format!("/{name} ");
				self.composer.update(cx, |state, cx| {
					state.set_value(text.clone(), window, cx);
					let column = text.chars().count() as u32;
					state.set_cursor_position(gpui_kit::component::input::Position::new(0, column), window, cx);
				});
				self.update_completions(cx);
			},
			_ => {},
		}
		self.focus_composer(window, cx);
	}

	fn compact(&mut self, instructions: Option<String>, cx: &mut Context<Self>) {
		let params = instructions.map_or(Value::Null, |text| json!({"customInstructions": text}));
		// Compaction calls the model; it can take minutes.
		self.control(PendingControl::Compact, "压缩", move |c| c.request("compact", params, Some(Duration::from_secs(600))), cx);
	}

	fn set_auto_compaction(&mut self, enabled: bool, cx: &mut Context<Self>) {
		self.control(
			PendingControl::AutoCompact,
			"自动压缩",
			move |c| c.request("set_auto_compaction", json!({"enabled": enabled}), Some(Duration::from_secs(30))),
			cx,
		);
	}

	pub fn toggle_panel(&mut self, cx: &mut Context<Self>) {
		self.panel = !self.panel;
		if self.panel {
			self.refresh_state(cx);
		}
		cx.notify();
	}

	/// Withdraws or promotes a queued message. pi's RPC can only clear the
	/// whole queue, so this clears it and queues the kept messages again in
	/// order (see [`crate::session::Queue::edit`]).
	fn edit_queue(&mut self, edit: QueueEdit, cx: &mut Context<Self>) {
		let shown = self.model.queue.clone();
		if shown.edit(&shown, edit).is_none() {
			return;
		}
		self.control(
			PendingControl::Queue,
			"修改队列",
			move |c| {
				let cleared = c.request("clear_queue", Value::Null, Some(Duration::from_secs(10)))?;
				let taken = |key: &str| -> Vec<String> {
					cleared.get(key).and_then(|v| serde_json::from_value(v.clone()).ok()).unwrap_or_default()
				};
				let current = Queue { steering: taken("steering"), follow_up: taken("followUp") };
				// pi may have delivered a message since the chip was drawn; the
				// edit applies to what it actually held.
				let kept = current.edit(&shown, edit).unwrap_or(current);
				// `prompt` with a streaming behavior queues while a run is active
				// and starts one if the run ended meanwhile; bare `steer` /
				// `follow_up` would leave the messages stranded in an idle queue.
				let queued = kept.steering.into_iter().map(|m| (m, "steer")).chain(kept.follow_up.into_iter().map(|m| (m, "followUp")));
				for (message, behavior) in queued {
					c.request("prompt", json!({"message": message, "streamingBehavior": behavior}), Some(Duration::from_secs(60)))?;
				}
				Ok(Value::Null)
			},
			cx,
		);
	}

	#[cfg(test)]
	pub(crate) fn dismiss_all_notices(&mut self) {
		self.model.notices.clear();
	}

	fn dismiss_notice(&mut self, index: usize, cx: &mut Context<Self>) {
		if index < self.model.notices.len() {
			self.model.notices.remove(index);
		}
		cx.notify();
	}

	fn render_row(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
		let p = theme::palette(cx);
		let Some(entry) = self.model.transcript.entries().get(index) else {
			return div().into_any_element();
		};
		let view = entry.view();
		let content = div().w_full().px_6();
		match &view.role {
			Role::Hidden => div().into_any_element(),
			Role::User => content
				.py_2()
				.flex()
				.justify_end()
				.child(
					div()
						.max_w(px(760.))
						.px_3()
						.py_2()
						.rounded(px(8.))
						.bg(rgb(p.accent))
						.text_sm()
						.flex()
						.flex_col()
						.gap_2()
						.children(view.blocks.iter().enumerate().map(|(bi, block)| match block {
							Block::Text(text) => div().child(text.clone()).into_any_element(),
							Block::Image { mime_type, data } => self.render_image(index, bi, mime_type, data, cx),
							_ => div().into_any_element(),
						})),
				)
				.into_any_element(),
			Role::Assistant => content
				.py_2()
				.flex()
				.flex_col()
				.gap_2()
				.children(view.blocks.iter().enumerate().map(|(bi, block)| self.render_block(index, bi, block, &view, cx)))
				.when(view.streaming && view.blocks.is_empty(), |col| {
					col.child(div().text_sm().text_color(rgb(p.muted_foreground)).child("思考中…"))
				})
				.when_some(view.error.clone(), |col, error| {
					col.child(div().text_sm().text_color(rgb(p.danger)).child(format!("错误：{error}")))
				})
				.into_any_element(),
			Role::Command => {
				let text = view.blocks.iter().filter_map(|b| if let Block::Output(t) = b { Some(t.as_str()) } else { None }).collect::<Vec<_>>().join("\n");
				content
					.py_1()
					.child(
						div()
							.id(("command-output", index))
							.rounded(px(6.))
							.border_l_2()
							.border_color(rgb(p.border))
							.bg(rgb(p.panel))
							.px_3()
							.py_1p5()
							.font_family("Menlo")
							.text_xs()
							.text_color(rgb(p.muted_foreground))
							.child(text)
							.test_support(),
					)
					.into_any_element()
			},
			Role::Summary { title } => {
				let open = self.expanded.contains(&(index, 0));
				let text = view.blocks.iter().filter_map(|b| if let Block::Text(t) = b { Some(t.clone()) } else { None }).collect::<String>();
				content
					.py_1()
					.child(
						div()
							.id(("summary", index))
							.rounded(px(6.))
							.border_1()
							.border_color(rgb(p.thinking_border))
							.bg(rgb(p.thinking_bg))
							.child(
								div()
									.id(("summary-toggle", index))
									.px_3()
									.py_1()
									.text_xs()
									.text_color(rgb(p.muted_foreground))
									.cursor_pointer()
									.child(format!("{} {title}", if open { "▾" } else { "▸" }))
									.on_click(cx.listener(move |this, _, _, cx| this.toggle_expanded(index, 0, cx))),
							)
							.when(open, |d| {
								d.child(
									div()
										.px_3()
										.pb_2()
										.text_sm()
										.child(TextView::markdown(ElementId::NamedInteger("summary-md".into(), index as u64), text)),
								)
							}),
					)
					.into_any_element()
			},
			Role::Bash { command, exit_code, cancelled, excluded } => {
				let output: String = view.blocks.iter().filter_map(|b| if let Block::Output(t) = b { Some(t.as_str()) } else { None }).collect();
				let running = self.model.shell_row == Some(index);
				let expanded = self.expanded.contains(&(index, 0));
				let (text, hidden) = clip_lines(&output, if expanded { usize::MAX } else { 14 });
				let failed = !running && (*cancelled || exit_code.is_some_and(|c| c != 0));
				let (status, color) = if running {
					("运行中".to_owned(), p.primary)
				} else if *cancelled {
					("已取消".to_owned(), p.warning)
				} else {
					(exit_code.map_or_else(|| "完成".to_owned(), |c| format!("exit {c}")), if failed { p.danger } else { p.success })
				};
				content
					.py_1()
					.child(
						div()
							.id(("shell", index))
							.rounded(px(8.))
							.border_1()
							.border_color(if failed { rgb(p.danger) } else { rgb(p.border) })
							.bg(rgb(p.elevated))
							.overflow_hidden()
							.child(
								div()
									.flex()
									.items_center()
									.gap_2()
									.px_3()
									.h(px(30.))
									.text_xs()
									.child(div().font_weight(FontWeight::SEMIBOLD).text_color(rgb(p.accent_foreground)).child(if *excluded { "!!" } else { "!" }))
									.child(div().flex_1().min_w_0().truncate().font_family("Menlo").child(format!("$ {command}")))
									.child(div().text_color(rgb(color)).child(status)),
							)
							.when(!text.trim().is_empty(), |card| {
								card.child(
									div()
										.px_3()
										.py_2()
										.border_t_1()
										.border_color(rgb(p.border))
										.bg(rgb(p.code))
										.font_family("Menlo")
										.text_xs()
										.child(text),
								)
							})
							.when(hidden > 0 || expanded, |card| {
								card.child(
									div()
										.id(("shell-more", index))
										.px_3()
										.py_1()
										.border_t_1()
										.border_color(rgb(p.border))
										.text_xs()
										.text_color(rgb(p.muted_foreground))
										.cursor_pointer()
										.child(if expanded { "收起".to_owned() } else { format!("展开其余 {hidden} 行") })
										.on_click(cx.listener(move |this, _, _, cx| this.toggle_expanded(index, 0, cx))),
								)
							})
							.test_support(),
					)
					.into_any_element()
			},
			// Shown inside the card of the call that produced it; a result with
			// no matching call falls back here.
			Role::ToolResult { tool_call_id, .. } if self.model.tool_rows.contains_key(tool_call_id) => div().into_any_element(),
			Role::ToolResult { tool_name, is_error, .. } => {
				let output: String = view
					.blocks
					.iter()
					.filter_map(|block| match block {
						Block::Output(text) | Block::Text(text) => Some(text.as_str()),
						_ => None,
					})
					.collect::<Vec<_>>()
					.join("\n");
				let (preview, more) = clip_lines(&output, 12);
				content
					.py_1()
					.child(
						div()
							.rounded(px(8.))
							.border_1()
							.border_color(if *is_error { rgb(p.danger) } else { rgb(p.border) })
							.bg(rgb(p.code))
							.px_3()
							.py_2()
							.font_family("Menlo")
							.text_xs()
							.child(div().text_color(rgb(p.muted_foreground)).child(tool_name.clone()))
							.child(preview)
							.when(more > 0, |d| d.child(div().text_color(rgb(p.muted_foreground)).child(format!("… 还有 {more} 行")))),
					)
					.into_any_element()
			},
		}
	}

	fn toggle_expanded(&mut self, row: usize, block: usize, cx: &mut Context<Self>) {
		if !self.expanded.remove(&(row, block)) {
			self.expanded.insert((row, block));
		}
		self.list.remeasure_items(row..row + 1);
		cx.notify();
	}

	/// A transcript image, decoded once per (row, block); a label when the
	/// message carried no data or an unknown format.
	fn render_image(&self, row: usize, bi: usize, mime_type: &str, data: &str, cx: &App) -> AnyElement {
		let p = theme::palette(cx);
		let image = self.images.borrow_mut().entry((row, bi)).or_insert_with(|| attachments::decode(mime_type, data)).clone();
		match image {
			Some(image) => div()
				.id(("transcript-image", row * 64 + bi))
				.child(img(image).max_w(px(320.)).max_h(px(240.)).object_fit(ObjectFit::Contain).rounded(px(6.)))
				.test_support()
				.into_any_element(),
			None => div().text_xs().text_color(rgb(p.muted_foreground)).child(format!("[图片 {mime_type}]")).into_any_element(),
		}
	}

	fn render_block(&self, row: usize, bi: usize, block: &Block, view: &View, cx: &mut Context<Self>) -> AnyElement {
		let p = theme::palette(cx);
		match block {
			Block::Text(text) => div()
				.w_full()
				.text_sm()
				.child(TextView::markdown(ElementId::NamedInteger("md".into(), (row * 64 + bi) as u64), text.clone()))
				.into_any_element(),
			Block::Thinking(text) => {
				let open = self.expanded.contains(&(row, bi));
				let live = view.streaming && bi + 1 == view.blocks.len();
				let summary = if live { "思考中…" } else { "思考过程" };
				div()
					.id(("thinking", row * 64 + bi))
					.w_full()
					.rounded(px(6.))
					.border_1()
					.border_color(rgb(p.thinking_border))
					.bg(rgb(p.thinking_bg))
					.child(
						div()
							.id(("thinking-toggle", row * 64 + bi))
							.px_3()
							.py_1()
							.text_xs()
							.text_color(rgb(p.muted_foreground))
							.cursor_pointer()
							.child(format!("{} {}", if open { "▾" } else { "▸" }, summary))
							.on_click(cx.listener(move |this, _, _, cx| this.toggle_expanded(row, bi, cx))),
					)
					.when(open, |d| d.child(div().px_3().pb_2().text_xs().text_color(rgb(p.muted_foreground)).child(text.clone())))
					.into_any_element()
			},
			Block::ToolCall { id, name, arguments } => {
				let expanded = self.expanded.contains(&(row, bi));
				ToolCard { id: row * 64 + bi, name, args: arguments, run: self.model.tools.get(id), expanded }
					.render(p, cx.listener(move |this, _, _, cx| this.toggle_expanded(row, bi, cx)))
			},
			Block::Image { mime_type, data } => self.render_image(row, bi, mime_type, data, cx),
			Block::Output(text) => div().font_family("Menlo").text_xs().child(text.clone()).into_any_element(),
		}
	}

	fn render_status(&self, cx: &mut Context<Self>) -> impl IntoElement {
		let p = theme::palette(cx);
		let (dot, label) = match (&self.connection, self.model.activity) {
			(Connection::Connecting, _) => (p.warning, "连接中".to_owned()),
			(Connection::Failed(_) | Connection::NeedsLogin(_), _) => (p.danger, "已断开".to_owned()),
			(Connection::Connected(_), Activity::Idle) => (p.success, "就绪".to_owned()),
			(Connection::Connected(_), Activity::Running) => match self.model.retry {
				Some((attempt, max)) => (p.warning, format!("重试 {attempt}/{max}")),
				None => (p.primary, "运行中".to_owned()),
			},
			(Connection::Connected(_), Activity::Shell) => (p.primary, "shell 运行中".to_owned()),
		};
		let state = self.model.state.as_ref();
		let connected = matches!(self.connection, Connection::Connected(_));
		let pending = self.model.pending;
		let this = cx.entity().downgrade();

		let current_model = state.and_then(|s| s.model.as_ref()).filter(|m| !m.is_unknown());
		let current = current_model.map(|m| (m.provider.clone(), m.id.clone()));
		let model_label = current_model.map_or_else(
			|| "模型".to_owned(),
			|m| format!("{} · {} ▾", truncate(m.display_name(), 30), m.provider),
		);
		let models = self.models.clone();
		let model_menu = {
			let this = this.clone();
			let current = current.clone();
			Button::new("status-model")
				.ghost()
				.xsmall()
				.label(model_label)
				.tooltip("切换模型（仅本会话，⌘M）")
				.loading(pending == Some(PendingControl::Model))
				.disabled(!connected || pending.is_some() || models.is_empty())
				.dropdown_menu_with_anchor(Anchor::BottomRight, move |menu, _, _| {
					let mut menu = menu.scrollable(true).max_w(px(520.)).max_h(px(420.));
					for model in &models {
						let key = (model.provider.clone(), model.id.clone());
						let this = this.clone();
						let checked = current.as_ref() == Some(&key);
						let name = model.display_name().to_owned();
						let id_sub = (name != model.id).then(|| model.id.clone());
						let provider = model.provider.clone();
						menu = menu.item(
							PopupMenuItem::element(move |_, cx| {
								let p = theme::palette(cx);
								let has_id = id_sub.is_some();
								div()
									.w(px(476.))
									.max_w(px(476.))
									.overflow_hidden()
									.flex()
									.items_center()
									.justify_between()
									.gap_2()
									.child(
										div()
											.flex_1()
											.min_w_0()
											.overflow_hidden()
											.flex()
											.items_baseline()
											.gap_1p5()
											.child(
												div()
													.when(has_id, |d| d.flex_none().max_w(px(230.)))
													.when(!has_id, |d| d.flex_1().min_w_0())
													.truncate()
													.font_weight(FontWeight::MEDIUM)
													.child(name.clone()),
											)
											.when_some(id_sub.clone(), |row, id| {
												row.child(div().flex_1().min_w_0().truncate().text_xs().text_color(rgb(p.muted_foreground)).child(id))
											}),
									)
									.child(
										div()
											.flex_none()
											.px_1p5()
											.py_0p5()
											.rounded(px(4.))
											.bg(rgb(p.muted))
											.text_xs()
											.text_color(rgb(p.muted_foreground))
											.child(provider.clone()),
									)
							})
							.checked(checked)
							.on_click(move |_, _, cx| {
								let (provider, id) = key.clone();
								_ = this.update(cx, |view, cx| view.set_model(provider, id, cx));
							}),
						);
					}
					menu
				})
		};

		let level = state.and_then(|s| s.thinking_level);
		let levels = self.thinking_levels.clone();
		let thinking_menu = {
			let this = this.clone();
			Button::new("status-thinking")
				.ghost()
				.xsmall()
				.label(format!("思考 {} ▾", level.map_or("—", |l| l.as_str())))
				.tooltip("切换思考档位（仅本会话，⌘T 循环）")
				.loading(pending == Some(PendingControl::Thinking))
				.disabled(!connected || pending.is_some() || levels.len() < 2)
				.dropdown_menu_with_anchor(Anchor::BottomRight, move |mut menu, _, _| {
					for &option in &levels {
						let this = this.clone();
						menu = menu.item(PopupMenuItem::new(option.as_str()).checked(level == Some(option)).on_click(
							move |_, _, cx| {
								_ = this.update(cx, |view, cx| view.set_thinking(option, cx));
							},
						));
					}
					menu
				})
		};

		let context_color = |percent: f64| match percent {
			p_ if p_ >= status::CONTEXT_DANGER => p.danger,
			p_ if p_ >= status::CONTEXT_WARN => p.warning,
			_ => p.primary,
		};
		let context = self.stats.as_ref().and_then(status::context).map(|(percent, used, window)| {
			let percent_value = percent.unwrap_or(0.);
			let label = match (percent, used) {
				(Some(percent), Some(used)) => format!("{percent:.0}% ({} / {})", status::tokens(used), status::tokens(window)),
				_ => format!("? / {}", status::tokens(window)),
			};
			div()
				.flex()
				.items_center()
				.gap_1()
				.child(Progress::new("status-context-bar").w(px(48.)).value(percent_value as f32).color(rgb(context_color(percent_value))))
				.child(
					div()
						.text_color(rgb(if percent_value >= status::CONTEXT_WARN { context_color(percent_value) } else { p.muted_foreground }))
						.child(label),
				)
		});
		let compacting = self.model.compacting;
		let compact = Button::new("status-compact")
			.ghost()
			.xsmall()
			.label(if compacting { "压缩中…" } else { "压缩" })
			.tooltip("压缩上下文（/compact）")
			.loading(compacting || pending == Some(PendingControl::Compact))
			.disabled(!connected || pending.is_some() || compacting || self.model.busy())
			.on_click(cx.listener(|this, _, _, cx| this.compact(None, cx)));

		let cost = self.stats.as_ref().map(|s| format!("${:.3}", s.cost));
		let statuses: Vec<String> = self.model.statuses.values().cloned().collect();
		div()
			.h(px(26.))
			.flex_none()
			.flex()
			.items_center()
			.gap_3()
			.px_3()
			.border_t_1()
			.border_color(rgb(p.border))
			.bg(rgb(p.panel))
			.text_xs()
			.text_color(rgb(p.muted_foreground))
			.child(div().flex().items_center().gap_1().child(div().size(px(7.)).rounded_full().bg(rgb(dot))).child(label))
			.child(div().min_w_0().truncate().child(self.cwd.display().to_string()))
			.child(div().flex_1())
			.when(!statuses.is_empty(), |row| {
				row.child(div().id("status-extensions").max_w(px(360.)).truncate().child(statuses.join(" · ")).test_support())
			})
			.child(model_menu)
			.child(thinking_menu)
			.children(context)
			.child(compact)
			.children(cost)
			.child(
				Button::new("status-panel")
					.ghost()
					.xsmall()
					.selected(self.panel)
					.label("详情")
					.tooltip("上下文、费用与会话状态（⌘J）")
					.on_click(cx.listener(|this, _, _, cx| this.toggle_panel(cx))),
			)
	}

	fn render_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
		let p = theme::palette(cx);
		let state = self.model.state.as_ref();
		let connected = matches!(self.connection, Connection::Connected(_));
		let pending = self.model.pending;
		let heading = |text: &'static str| {
			div().mt_3().mb_1().text_xs().font_weight(FontWeight::SEMIBOLD).text_color(rgb(p.muted_foreground)).child(text)
		};
		let kv = |key: String, value: String| {
			div()
				.flex()
				.gap_2()
				.text_xs()
				.child(div().w(px(92.)).flex_none().text_color(rgb(p.muted_foreground)).child(key))
				.child(div().flex_1().min_w_0().font_family("Menlo").child(value))
		};

		// Context and compaction.
		let usage = self.stats.as_ref().and_then(status::context);
		let percent = usage.and_then(|(percent, ..)| percent);
		let auto = state.is_some_and(|s| s.auto_compaction_enabled);
		let context = div()
			.flex()
			.flex_col()
			.gap_1()
			.child(heading("上下文"))
			.child(Progress::new("panel-context-bar").value(percent.unwrap_or(0.) as f32).color(rgb(match percent {
				Some(v) if v >= status::CONTEXT_DANGER => p.danger,
				Some(v) if v >= status::CONTEXT_WARN => p.warning,
				_ => p.primary,
			})))
			.child(kv(
				"已用".into(),
				match usage {
					Some((Some(percent), Some(used), window)) => {
						format!("{} / {}（{percent:.1}%）", status::tokens(used), status::tokens(window))
					},
					Some((_, _, window)) => format!("压缩后待更新 / {}", status::tokens(window)),
					None => "—".into(),
				},
			))
			.child(
				div().flex().items_center().justify_between().child(
					Switch::new("panel-auto-compact")
						.xsmall()
						.label("自动压缩")
						.checked(auto)
						.disabled(!connected || pending.is_some())
						.on_click(cx.listener(|this, checked: &bool, _, cx| this.set_auto_compaction(*checked, cx))),
				),
			);

		// Cost.
		let cost = div().flex().flex_col().gap_1().child(heading("费用")).children(self.stats.as_ref().map(|s| {
			div()
				.flex()
				.flex_col()
				.gap_1()
				.child(kv("累计".into(), format!("${:.4}", s.cost)))
				.child(kv("输入".into(), status::tokens(s.tokens.input)))
				.child(kv("输出".into(), status::tokens(s.tokens.output)))
				.child(kv("缓存读/写".into(), format!("{} / {}", status::tokens(s.tokens.cache_read), status::tokens(s.tokens.cache_write))))
				.child(kv("消息 / 工具".into(), format!("{} / {}", s.total_messages, s.tool_calls)))
		}));

		// Extension status and widgets.
		let extensions = div()
			.flex()
			.flex_col()
			.gap_1()
			.child(heading("扩展"))
			.when(self.model.statuses.is_empty(), |col| col.child(div().text_xs().text_color(rgb(p.muted_foreground)).child("暂无")))
			.children(self.model.statuses.iter().map(|(key, text)| kv(key.clone(), text.clone())));

		// Every `get_state` field.
		let fields = div().flex().flex_col().gap_0p5().child(heading("会话状态")).children(
			state.map(status::state_rows).unwrap_or_default().into_iter().map(|(key, label, value)| {
				div().id(SharedString::from(format!("state-{key}"))).child(kv(label.into(), value)).test_support()
			}),
		);

		div()
			.id("detail-panel")
			.w(px(300.))
			.flex_none()
			.h_full()
			.overflow_y_scroll()
			.border_l_1()
			.border_color(rgb(p.border))
			.bg(rgb(p.panel))
			.px_3()
			.pb_4()
			.child(context)
			.child(cost)
			.child(extensions)
			.child(fields)
	}

	fn render_widgets(&self, below: bool, cx: &App) -> impl IntoElement {
		let p = theme::palette(cx);
		div().flex().flex_col().gap_1().children(self.model.widgets.iter().filter(|(_, w)| w.below == below).map(|(key, widget)| {
			div()
				.id(SharedString::from(format!("widget-{key}")))
				.px_3()
				.py_1()
				.rounded(px(6.))
				.border_1()
				.border_color(rgb(p.border))
				.bg(rgb(p.panel))
				.font_family("Menlo")
				.text_xs()
				.text_color(rgb(p.muted_foreground))
				.child(widget.lines.join("\n"))
				.test_support()
		}))
	}

	fn render_composer(&self, cx: &mut Context<Self>) -> impl IntoElement {
		let p = theme::palette(cx);
		let busy = self.model.busy();
		let connected = matches!(self.connection, Connection::Connected(_));
		let editing = self.model.pending == Some(PendingControl::Queue);
		let queue = &self.model.queue;
		let chip = |kind: &'static str, color: u32, text: &String, index: usize, edit_index: usize, steering: bool| {
			div()
				.id(("queue-chip", index))
				.flex()
				.items_center()
				.gap_1()
				.pl_2()
				.pr_1()
				.h(px(26.))
				.rounded(px(6.))
				.border_1()
				.border_color(rgb(p.border))
				.bg(rgb(p.elevated))
				.text_xs()
				.child(div().font_weight(FontWeight::SEMIBOLD).text_color(rgb(color)).child(kind))
				.child(truncate(text, 60))
				.when(!steering, |chip| {
					chip.child(
						Button::new(("queue-promote", index))
							.ghost()
							.xsmall()
							.label("插队")
							.tooltip("转为 steer，在当前工具调用结束后送达")
							.disabled(editing)
							.on_click(cx.listener(move |this, _, _, cx| this.edit_queue(QueueEdit::Promote { index: edit_index }, cx))),
					)
				})
				.child(
					Button::new(("queue-remove", index))
						.ghost()
						.xsmall()
						.label("×")
						.tooltip("撤回")
						.disabled(editing)
						.on_click(cx.listener(move |this, _, _, cx| this.edit_queue(QueueEdit::Remove { steering, index: edit_index }, cx))),
				)
		};
		let steering = queue.steering.len();
		let chips: Vec<_> = queue
			.steering
			.iter()
			.enumerate()
			.map(|(i, text)| chip("插队", p.warning, text, i, i, true))
			.chain(queue.follow_up.iter().enumerate().map(|(i, text)| chip("排队", p.accent_foreground, text, steering + i, i, false)))
			.collect();
		div()
			.flex_none()
			.w_full()
			.px_6()
			.pb_3()
			.flex()
			.flex_col()
			.gap_2()
			.child(self.render_widgets(false, cx))
			.child(div().flex().flex_wrap().gap_1().children(chips))
			.when(!self.attachments.is_empty(), |col| col.child(self.render_attachments(cx)))
			.child(
				div()
					.flex()
					.items_end()
					.gap_2()
					.child(
						div()
							.flex_1()
							.relative()
							.children(self.render_completions(cx))
							.capture_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
								if event.keystroke.key == "escape" && this.completions.is_none() {
									this.on_escape(cx);
								}
							}))
							// The input binds these keys to actions, which dispatch
							// before key-down listeners; take them while the
							// completion popup is open.
							.capture_action(cx.listener(|this, _: &MoveUp, window, cx| this.completion_key(CompletionKey::Up, window, cx)))
							.capture_action(cx.listener(|this, _: &MoveDown, window, cx| this.completion_key(CompletionKey::Down, window, cx)))
							.capture_action(cx.listener(|this, _: &IndentInline, window, cx| this.completion_key(CompletionKey::Tab, window, cx)))
							.capture_action(cx.listener(|this, action: &Enter, window, cx| {
								if !action.secondary && !action.shift {
									this.completion_key(CompletionKey::Enter, window, cx)
								}
							}))
							.capture_action(cx.listener(|this, _: &Escape, window, cx| this.completion_key(CompletionKey::Escape, window, cx)))
							// Images on the clipboard attach; text pastes as usual.
							.capture_action(cx.listener(|this, _: &Paste, _, cx| {
								if let Some(item) = cx.read_from_clipboard()
									&& this.attach(attachments::from_clipboard(&item), cx)
								{
									cx.stop_propagation();
								}
							}))
							.child(div().id("composer").child(Textarea::new(&self.composer).w_full()).test_support()),
					)
					.when(busy, |row| {
						row.child(
							Button::new("abort")
								.danger()
								.label("停止")
								.tooltip("Esc（输入框为空时）；排队的消息退回输入框")
								.on_click(cx.listener(|this, _, _, cx| this.abort(cx))),
						)
					})
					.child(
						Button::new("send")
							.primary()
							.label(if busy { "排队" } else { "发送" })
							.disabled(!connected)
							.on_click(cx.listener(|this, _, window, cx| this.send(Delivery::Normal, window, cx))),
					),
			)
			.child(self.render_widgets(true, cx))
	}

	fn render_notices(&self, cx: &mut Context<Self>) -> impl IntoElement {
		let p = theme::palette(cx);
		div()
			.absolute()
			.top_3()
			.right_3()
			.w(px(360.))
			.flex()
			.flex_col()
			.gap_2()
			.children(self.model.notices.iter().enumerate().map(|(index, notice)| {
				let (color, text) = match notice {
					Notice::Info(text) => (p.accent_foreground, text.clone()),
					Notice::Error(text) => (p.danger, text.clone()),
				};
				div()
					.id(("notice", index))
					.px_3()
					.py_2()
					.rounded(px(8.))
					.border_1()
					.border_color(rgb(color))
					.bg(rgb(p.elevated))
					.text_xs()
					.shadow_md()
					.cursor_pointer()
					.child(text)
					.on_click(cx.listener(move |this, _, _, cx| this.dismiss_notice(index, cx)))
			}))
	}

	fn render_needs_login(&self, error: &str, cx: &mut Context<Self>) -> AnyElement {
		let p = theme::palette(cx);
		div()
			.id("needs-login")
			.flex_1()
			.flex()
			.flex_col()
			.items_center()
			.justify_center()
			.gap_3()
			.child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child("还没有可用的模型"))
			.child(
				div()
					.max_w(px(560.))
					.text_sm()
					.text_color(rgb(p.muted_foreground))
					.child("pi 找不到带凭据的模型。登录一个 provider（或设置 ANTHROPIC_API_KEY 等环境变量）后再连接。"),
			)
			.child(div().max_w(px(560.)).text_xs().font_family("Menlo").text_color(rgb(p.muted_foreground)).child(error.to_owned()))
			.child(
				div()
					.flex()
					.gap_2()
					.child(
						Button::new("needs-login-open")
							.primary()
							.label("登录 provider")
							.on_click(cx.listener(|_, _, _, cx| cx.emit(ChatEvent::OpenAccounts))),
					)
					.child(Button::new("reconnect").outline().label("重新连接").on_click(cx.listener(|this, _, _, cx| this.reconnect(cx)))),
			)
			.test_support()
			.into_any_element()
	}

	fn render_failed(&self, error: &str, cx: &mut Context<Self>) -> AnyElement {
		let p = theme::palette(cx);
		div()
			.flex_1()
			.flex()
			.flex_col()
			.items_center()
			.justify_center()
			.gap_3()
			.child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child("无法连接 pi"))
			.child(div().max_w(px(560.)).text_sm().text_color(rgb(p.muted_foreground)).child(error.to_owned()))
			.child(
				div()
					.font_family("Menlo")
					.text_xs()
					.px_3()
					.py_2()
					.rounded(px(6.))
					.bg(rgb(p.code))
					.child("curl -fsSL https://pi.dev/install.sh | sh"),
			)
			.child(Button::new("reconnect").primary().label("重新连接").on_click(cx.listener(|this, _, _, cx| this.reconnect(cx))))
			.into_any_element()
	}
}

/// The message of a failed command, without the `<command> failed:` prefix.
fn command_error(error: &pi_rpc::Error) -> String {
	match error {
		pi_rpc::Error::Command { error, .. } => error.clone(),
		other => other.to_string(),
	}
}

impl Render for ChatView {
	fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
		let p = theme::palette(cx);
		let body = match &self.connection {
			Connection::Failed(error) if self.model.row_count() == 0 => self.render_failed(&error.clone(), cx),
			Connection::NeedsLogin(error) => self.render_needs_login(&error.clone(), cx),
			_ if self.model.row_count() == 0 => div()
				.flex_1()
				.flex()
				.flex_col()
				.items_center()
				.justify_center()
				.gap_2()
				.child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child("开始一个新会话"))
				.child(
					div()
						.flex()
						.items_center()
						.gap_2()
						.text_sm()
						.text_color(rgb(p.muted_foreground))
						.child(format!("工作目录 {}", self.cwd.display()))
						.child(
							Button::new("change-cwd")
								.outline()
								.xsmall()
								.label("选择目录…")
								.on_click(cx.listener(|this, _, _, cx| this.prompt_change_cwd(cx))),
						),
				)
				.child(
					div()
						.text_xs()
						.text_color(rgb(p.muted_foreground))
						.child(if matches!(self.connection, Connection::Connecting) { "正在启动 pi…" } else { "输入消息后按 Enter 发送" }),
				)
				.into_any_element(),
			_ => {
				let this = cx.entity().downgrade();
				div()
					.flex_1()
					.min_h_0()
					.child(
						list(self.list.clone(), move |index, _window, cx| {
							this.upgrade()
								.map(|view| view.update(cx, |view, cx| view.render_row(index, cx)))
								.unwrap_or_else(|| div().into_any_element())
						})
						.size_full(),
					)
					.into_any_element()
			},
		};
		let disconnected_banner = match &self.connection {
			Connection::Failed(error) if self.model.row_count() > 0 => Some(error.clone()),
			_ => None,
		};

		div()
			.id("chat-root")
			.size_full()
			.relative()
			.flex()
			.flex_col()
			.bg(rgb(p.background))
			// Image files dropped anywhere on the tab attach to the composer.
			.drag_over::<ExternalPaths>(move |style, _, _, _| style.bg(rgb(p.accent)))
			.on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
				this.attach(attachments::from_paths(paths.paths()), cx);
			}))
			.text_color(cx.theme().foreground)
			.when_some(disconnected_banner, |root, error| {
				root.child(
					div()
						.flex()
						.items_center()
						.gap_3()
						.px_4()
						.py_2()
						.bg(rgb(p.danger))
						.text_color(rgb(0xFFFFFF))
						.text_sm()
						.child(format!("已断开：{error}"))
						.child(Button::new("reconnect-banner").small().label("重新连接").on_click(cx.listener(|this, _, _, cx| this.reconnect(cx)))),
				)
			})
			.child(
				div()
					.flex_1()
					.min_h_0()
					.flex()
					.child(div().flex_1().min_w_0().flex().flex_col().child(body).child(self.render_composer(cx)))
					.when(self.panel, |row| row.child(self.render_panel(cx))),
			)
			.child(self.render_status(cx))
			.child(self.render_notices(cx))
			.when_some(self.dialog.clone(), |root, dialog| {
				root.child(
					div().absolute().inset_0().flex().items_center().justify_center().bg(hsla(0., 0., 0., 0.35)).child(dialog),
				)
			})
	}
}

