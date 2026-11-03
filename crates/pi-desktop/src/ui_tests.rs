//! Headless UI tests: the real views against a real `pi --mode rpc`.
//! Tests that spawn pi are ignored by default; those that call a model say
//! so. Run them one at a time:
//!
//! cargo test -p pi-desktop -- --ignored --test-threads 1 --nocapture
//!
//! Each test works in its own temp directory. Model-free tests that touch
//! credentials use a scratch `PI_CODING_AGENT_DIR`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gpui_kit::test::{TestInput, TestWindowExt as _};
use gpui_kit::{
	AnyWindowHandle, AppContext as _, Bounds, ElementInputHandler, Entity, Point, TestAppContext, WindowBounds, WindowOptions,
	px, size,
};
use pi_bridge::SessionOptions;
use pi_store::display::{Block, Role, View};

use crate::chat::ChatView;
use crate::session::Notice;
use crate::workspace::Workspace;

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/dialogs.ts");

fn setup(cx: &mut TestAppContext) {
	// Sessions run on real threads and pipes, outside the test scheduler.
	cx.executor().allow_parking();
	cx.update(gpui_kit::init);
	cx.update(crate::theme::apply);
}

fn temp_dir(name: &str) -> PathBuf {
	let dir = std::env::temp_dir().join(format!("pi-desktop-{name}-{}", std::process::id()));
	_ = std::fs::remove_dir_all(&dir);
	std::fs::create_dir_all(&dir).unwrap();
	// Resolve /var → /private/var so paths match what pi reports.
	std::fs::canonicalize(&dir).unwrap()
}

fn window_options(width: f32, height: f32) -> WindowOptions {
	WindowOptions {
		window_bounds: Some(WindowBounds::Windowed(Bounds { origin: Point::default(), size: size(px(width), px(height)) })),
		..Default::default()
	}
}

/// Opens the real `ChatView` on a fresh temp dir and waits for pi.
fn open_chat(cx: &mut TestAppContext, name: &str, args: &[&str]) -> (AnyWindowHandle, Entity<ChatView>, PathBuf) {
	setup(cx);
	let dir = temp_dir(name);
	let mut extra_args: Vec<String> = ["--no-session", "--no-approve"].map(String::from).to_vec();
	extra_args.extend(args.iter().map(|a| (*a).to_owned()));
	let options = SessionOptions { cwd: Some(dir.clone()), extra_args, ..Default::default() };
	let (handle, view) = cx.update(|cx| {
		gpui_kit::open_window(window_options(1000., 800.), cx, |window, cx| cx.new(|cx| ChatView::new(options, window, cx))).expect("open window")
	});
	wait_until(cx, Duration::from_secs(90), "pi connects", |cx| view.read_with(cx, |v, _| v.is_connected()));
	(handle, view, dir)
}

fn open_workspace(cx: &mut TestAppContext, sessions: &Path, cwd: &Path, args: &[&str]) -> (AnyWindowHandle, Entity<Workspace>) {
	setup(cx);
	let base = SessionOptions {
		session_dir: Some(sessions.to_owned()),
		extra_args: args.iter().map(|a| (*a).to_owned()).collect(),
		..Default::default()
	};
	let cwd = cwd.to_owned();
	cx.update(|cx| {
		gpui_kit::open_window(window_options(1200., 800.), cx, |window, cx| cx.new(|cx| Workspace::new(base, cwd, window, cx)))
			.expect("open window")
	})
}

fn type_and(cx: &mut TestAppContext, handle: AnyWindowHandle, text: &str, key: &str) {
	cx.update_window(handle, |_, window, cx| {
		window.render_frame(cx);
		window.input(text, cx);
		window.press(key, cx);
	})
	.unwrap();
}

/// Polls on wall-clock time: the IO is real, so GPUI's simulated clock does
/// not apply.
fn wait_until(cx: &mut TestAppContext, timeout: Duration, what: &str, mut done: impl FnMut(&mut TestAppContext) -> bool) {
	let deadline = Instant::now() + timeout;
	while !done(cx) {
		assert!(Instant::now() < deadline, "timed out waiting for: {what}");
		cx.run_until_parked();
		std::thread::sleep(Duration::from_millis(20));
	}
}

fn process_alive(pid: u32) -> bool {
	std::process::Command::new("kill").args(["-0", &pid.to_string()]).stderr(std::process::Stdio::null()).status().is_ok_and(|s| s.success())
}

fn info_notice(view: &Entity<ChatView>, cx: &mut TestAppContext, prefix: &str) -> Option<String> {
	view.read_with(cx, |v, _| {
		v.notices().iter().find_map(|n| match n {
			Notice::Info(t) if t.starts_with(prefix) => Some(t.clone()),
			_ => None,
		})
	})
}

impl ChatView {
	fn rows(&self) -> Vec<View> {
		self.transcript().entries().iter().map(pi_store::Entry::view).collect()
	}

	fn user_texts(&self) -> Vec<String> {
		self.rows()
			.into_iter()
			.filter(|row| row.role == Role::User)
			.flat_map(|row| row.blocks)
			.filter_map(|block| match block {
				Block::Text(text) => Some(text),
				_ => None,
			})
			.collect()
	}

	/// Text of each finished assistant message that has text.
	fn assistant_texts(&self) -> Vec<String> {
		self.rows()
			.into_iter()
			.filter(|row| row.role == Role::Assistant && !row.streaming)
			.map(|row| {
				row.blocks
					.into_iter()
					.filter_map(|block| match block {
						Block::Text(text) => Some(text),
						_ => None,
					})
					.collect::<String>()
			})
			.filter(|text| !text.is_empty())
			.collect()
	}

	fn assistant_text(&self) -> Option<String> {
		let text = self.assistant_texts().concat();
		(!text.is_empty()).then_some(text)
	}

	fn tool_running(&self, name: &str) -> bool {
		self.transcript().entries().iter().any(|e| {
			e.view().blocks.iter().any(|b| {
				matches!(b, Block::ToolCall { id, name: n, .. } if n == name
					&& self.tool_runs().get(id).is_some_and(|run| run.status == pi_store::tools::Status::Running))
			})
		})
	}
}

// ---------------------------------------------------------------- model-free

/// The fixture extension opens confirm, input, editor and select dialogs
/// (pi's RPC dialog methods); each answer reaches the extension.
#[gpui_kit::test]
#[ignore = "spawns pi"]
async fn confirm_input_editor_and_select_round_trip(cx: &mut TestAppContext) {
	let (handle, view, dir) = open_chat(cx, "dialogs", &["--no-extensions", "--no-tools", "-e", FIXTURE]);
	type_and(cx, handle, "/dialogs", "enter");
	let wait_kind = |cx: &mut TestAppContext, kind: &'static str| {
		wait_until(cx, Duration::from_secs(30), kind, |cx| view.read_with(cx, |v, cx| v.open_dialog_kind(cx) == Some(kind)))
	};
	wait_kind(cx, "confirm");
	assert_eq!(view.read_with(cx, |v, cx| v.open_dialog_title(cx)).as_deref(), Some("确认标题"));
	cx.update_window(handle, |_, window, cx| window.click("dialog-yes", cx)).unwrap();
	wait_kind(cx, "input");
	cx.update_window(handle, |_, window, cx| {
		window.render_frame(cx);
		window.input("Alice", cx);
		window.click("dialog-submit", cx);
	})
	.unwrap();
	wait_kind(cx, "editor");
	cx.update_window(handle, |_, window, cx| {
		window.render_frame(cx);
		window.input("改", cx);
		window.click("dialog-submit", cx);
	})
	.unwrap();
	wait_kind(cx, "select");
	assert_eq!(view.read_with(cx, |v, cx| v.open_dialog_options(cx)).unwrap(), ["甲", "乙", "丙"]);
	cx.update_window(handle, |_, window, cx| {
		window.render_frame(cx);
		window.click(("select-option", 1usize), cx);
	})
	.unwrap();

	wait_until(cx, Duration::from_secs(30), "result notice", |cx| info_notice(&view, cx, "dialogs ").is_some());
	let notice = info_notice(&view, cx, "dialogs ").unwrap();
	assert!(notice.contains("confirmed=true") && notice.contains("name=Alice") && notice.contains("pick=乙"), "{notice}");
	let text = notice.split("text=").nth(1).unwrap_or_default();
	assert!(text.contains("预填") && text.contains("改"), "editor keeps the prefill and adds input: {notice}");
	assert!(view.read_with(cx, |v, cx| v.open_dialog_kind(cx).is_none()));
	assert!(view.read_with(cx, |v, _| v.is_idle()), "a handled extension command leaves the tab idle");
	_ = std::fs::remove_dir_all(&dir);
}

/// A dialog with a timeout closes when pi answers it itself; a dialog
/// cancelled with 取消 reports `undefined`.
#[gpui_kit::test]
#[ignore = "spawns pi"]
async fn timed_out_dialog_closes_and_cancel_answers_undefined(cx: &mut TestAppContext) {
	let (handle, view, dir) = open_chat(cx, "timeout", &["--no-extensions", "--no-tools", "-e", FIXTURE]);
	type_and(cx, handle, "/timeout", "enter");
	wait_until(cx, Duration::from_secs(30), "select opens", |cx| view.read_with(cx, |v, cx| v.open_dialog_kind(cx) == Some("select")));
	wait_until(cx, Duration::from_secs(10), "pi times it out", |cx| info_notice(&view, cx, "timeout ").is_some());
	wait_until(cx, Duration::from_secs(5), "dialog closes", |cx| view.read_with(cx, |v, cx| v.open_dialog_kind(cx).is_none()));
	assert_eq!(info_notice(&view, cx, "timeout ").as_deref(), Some("timeout pick=undefined"));

	type_and(cx, handle, "/timeout", "enter");
	wait_until(cx, Duration::from_secs(30), "select opens again", |cx| view.read_with(cx, |v, cx| v.open_dialog_kind(cx) == Some("select")));
	view.update(cx, |v, _| v.dismiss_all_notices());
	cx.update_window(handle, |_, window, cx| {
		window.render_frame(cx);
		window.click("dialog-cancel", cx);
	})
	.unwrap();
	wait_until(cx, Duration::from_secs(10), "cancel reported", |cx| info_notice(&view, cx, "timeout ").is_some());
	assert!(view.read_with(cx, |v, cx| v.open_dialog_kind(cx).is_none()));
	_ = std::fs::remove_dir_all(&dir);
}

/// `setStatus` lands in the status bar and the detail panel; `setWidget`
/// renders above the composer.
#[gpui_kit::test]
#[ignore = "spawns pi"]
async fn extension_status_and_widget_render(cx: &mut TestAppContext) {
	let (handle, view, dir) = open_chat(cx, "status-ext", &["--no-extensions", "--no-tools", "-e", FIXTURE]);
	type_and(cx, handle, "/status", "enter");
	wait_until(cx, Duration::from_secs(30), "status set", |cx| view.read_with(cx, |v, _| v.statuses().get("fixture").map(String::as_str) == Some("fixture ok")));
	cx.update_window(handle, |_, window, cx| {
		window.render_frame(cx);
		assert!(window.try_find("status-extensions").is_some(), "status bar shows it");
		assert!(window.try_find("widget-fixture").is_some(), "widget renders");
	})
	.unwrap();
	_ = std::fs::remove_dir_all(&dir);
}

/// `/` completes pi's catalog (extension commands, prompt templates, skills)
/// plus the desktop's own commands; Tab completes, Esc hides without
/// aborting; Enter on an extension command runs it without a model call.
#[gpui_kit::test]
#[ignore = "spawns pi"]
async fn slash_commands_complete_and_run(cx: &mut TestAppContext) {
	let (handle, view, dir) = open_chat(cx, "slash", &["--no-extensions", "--no-tools", "-e", FIXTURE]);
	wait_until(cx, Duration::from_secs(30), "command catalog", |cx| view.read_with(cx, |v, _| v.command_count() >= 3));
	let popup = |cx: &mut TestAppContext| view.read_with(cx, |v, _| v.completion_labels());
	let key = |cx: &mut TestAppContext, key: &str| {
		cx.update_window(handle, |_, window, cx| {
			window.press(key, cx);
			window.render_frame(cx);
		})
		.unwrap();
	};
	cx.update_window(handle, |_, window, cx| {
		window.render_frame(cx);
		window.click("composer", cx);
		window.input("/", cx);
	})
	.unwrap();
	cx.run_until_parked();
	cx.update_window(handle, |_, window, cx| {
		window.render_frame(cx);
		assert!(window.try_find("slash-popup").is_some(), "popup not shown");
	})
	.unwrap();
	let (all, _) = popup(cx).expect("popup");
	for expected in ["/compact", "/new", "/dialogs", "/status"] {
		assert!(all.iter().any(|l| l == expected), "{expected} missing from {all:?}");
	}

	cx.update_window(handle, |_, window, cx| window.input("sta", cx)).unwrap();
	let (rows, selected) = popup(cx).expect("narrowed");
	assert_eq!((rows[0].as_str(), selected), ("/status", 0), "{rows:?}");
	key(cx, "tab");
	assert_eq!(view.read_with(cx, |v, cx| v.composer_text(cx)), "/status ");
	assert!(popup(cx).is_none(), "a space ends the command word");

	cx.update_window(handle, |_, window, cx| window.press("backspace", cx)).unwrap();
	assert!(popup(cx).is_some(), "editing reopens it");
	key(cx, "escape");
	assert!(popup(cx).is_none());
	assert_eq!(view.read_with(cx, |v, cx| v.composer_text(cx)), "/status");
	assert!(view.read_with(cx, |v, _| v.is_idle()), "Esc on the popup must not abort");

	cx.update_window(handle, |_, window, cx| window.press("enter", cx)).unwrap();
	wait_until(cx, Duration::from_secs(20), "command ran", |cx| view.read_with(cx, |v, _| v.statuses().contains_key("fixture")));
	wait_until(cx, Duration::from_secs(10), "composer idle", |cx| view.read_with(cx, |v, _| v.is_idle()));
	assert_eq!(view.read_with(cx, |v, cx| v.composer_text(cx)), "");
	assert!(view.read_with(cx, |v, _| v.assistant_text().is_none()), "an extension command must not call the model");
	_ = std::fs::remove_dir_all(&dir);
}

/// `!cmd` runs through pi's `bash` command: output streams into a shell
/// row, the exit code shows, and Esc stops a long command.
#[gpui_kit::test]
#[ignore = "spawns pi"]
async fn shell_command_streams_and_aborts(cx: &mut TestAppContext) {
	let (handle, view, dir) = open_chat(cx, "shell", &["--no-extensions", "--no-tools"]);
	type_and(cx, handle, "!printf a; sleep 0.3; printf b; exit 3", "enter");
	wait_until(cx, Duration::from_secs(20), "shell finishes", |cx| view.read_with(cx, |v, _| v.is_idle() && !v.rows().is_empty()));
	let row = view.read_with(cx, |v, _| v.rows().into_iter().next().unwrap());
	assert_eq!(row.role, Role::Bash { command: "printf a; sleep 0.3; printf b; exit 3".into(), exit_code: Some(3), cancelled: false, excluded: false });
	assert_eq!(row.blocks, [Block::Output("ab".into())]);
	cx.update_window(handle, |_, window, cx| {
		window.render_frame(cx);
		assert!(window.try_find(("shell", 0usize)).is_some());
	})
	.unwrap();

	type_and(cx, handle, "!sleep 30", "enter");
	wait_until(cx, Duration::from_secs(10), "shell running", |cx| view.read_with(cx, |v, _| !v.is_idle()));
	let stopped = Instant::now();
	cx.update_window(handle, |_, window, cx| window.press("escape", cx)).unwrap();
	wait_until(cx, Duration::from_secs(15), "shell aborted", |cx| view.read_with(cx, |v, _| v.is_idle()));
	assert!(stopped.elapsed() < Duration::from_secs(15), "abort took {:?}", stopped.elapsed());
	let last = view.read_with(cx, |v, _| v.rows().pop().unwrap());
	assert!(matches!(last.role, Role::Bash { cancelled: true, .. }), "{last:?}");
	_ = std::fs::remove_dir_all(&dir);
}

/// Status bar menus, the auto-compact switch, compact, and the panel, which
/// lists every `get_state` field. Session-only changes.
#[gpui_kit::test]
#[ignore = "spawns pi"]
async fn status_bar_controls_and_detail_panel(cx: &mut TestAppContext) {
	let (handle, view, dir) = open_chat(cx, "status", &["--no-extensions", "--no-tools"]);
	wait_until(cx, Duration::from_secs(30), "state, stats and choices load", |cx| {
		view.read_with(cx, |v, _| v.state().is_some() && v.has_stats() && v.choices().0 > 1)
	});
	cx.update_window(handle, |_, window, cx| {
		window.render_frame(cx);
		window.click("status-panel", cx);
		window.render_frame(cx);
		for key in [
			"sessionId", "sessionName", "sessionFile", "model", "thinkingLevel", "isStreaming", "isCompacting",
			"steeringMode", "followUpMode", "autoCompactionEnabled", "messageCount", "pendingMessageCount",
		] {
			assert!(window.try_find(format!("state-{key}")).is_some(), "panel has no row for {key}");
		}
	})
	.unwrap();

	// Model menu: pick another model.
	let (models, current) = view.read_with(cx, |v, _| {
		(v.models_for_test(), v.state().unwrap().model.as_ref().map(|m| (m.provider.clone(), m.id.clone())))
	});
	let (pick, target) = models.iter().cloned().enumerate().find(|(_, m)| Some(m) != current.as_ref()).expect("another model");
	cx.update_window(handle, |_, window, cx| {
		window.render_frame(cx);
		window.click("status-model", cx);
		window.render_frame(cx);
		window.within("popup-menu").click(pick, cx);
	})
	.unwrap();
	wait_until(cx, Duration::from_secs(20), "model applied", |cx| {
		view.read_with(cx, |v, _| !v.control_pending() && v.state().unwrap().model.as_ref().map(|m| (m.provider.clone(), m.id.clone())) == Some(target.clone()))
	});

	// Thinking menu, when the new model has levels to choose from.
	wait_until(cx, Duration::from_secs(20), "levels reload", |cx| view.read_with(cx, |v, _| !v.control_pending()));
	let (levels, level) = view.read_with(cx, |v, _| (v.thinking_levels_for_test(), v.state().unwrap().thinking_level));
	if let Some((pick, other)) = levels.iter().copied().enumerate().find(|(_, l)| Some(*l) != level) {
		cx.update_window(handle, |_, window, cx| {
			window.render_frame(cx);
			window.click("status-thinking", cx);
			window.render_frame(cx);
			window.within("popup-menu").click(pick, cx);
		})
		.unwrap();
		wait_until(cx, Duration::from_secs(20), "thinking level applied", |cx| {
			view.read_with(cx, |v, _| !v.control_pending() && v.state().unwrap().thinking_level == Some(other))
		});
	}

	let auto = view.read_with(cx, |v, _| v.state().unwrap().auto_compaction_enabled);
	cx.update_window(handle, |_, window, cx| {
		window.render_frame(cx);
		window.click("panel-auto-compact", cx);
	})
	.unwrap();
	wait_until(cx, Duration::from_secs(20), "auto-compact toggled", |cx| {
		view.read_with(cx, |v, _| !v.control_pending() && v.state().unwrap().auto_compaction_enabled == !auto)
	});

	// Compact on an empty session: pi refuses and says why.
	cx.update_window(handle, |_, window, cx| {
		window.render_frame(cx);
		window.click("status-compact", cx);
	})
	.unwrap();
	wait_until(cx, Duration::from_secs(30), "compact reports skip", |cx| {
		view.read_with(cx, |v, _| !v.control_pending()) && info_notice(&view, cx, "压缩未执行").is_some()
	});
	let errors: Vec<_> = view.read_with(cx, |v, _| v.notices().iter().filter(|n| matches!(n, Notice::Error(_))).cloned().collect());
	assert!(errors.is_empty(), "error notices: {errors:?}");
	_ = std::fs::remove_dir_all(&dir);
}

/// Without credentials the tab says so and offers the accounts panel,
/// whose own process starts anyway and lists providers.
#[gpui_kit::test]
#[ignore = "spawns pi"]
async fn missing_credentials_point_to_login(cx: &mut TestAppContext) {
	let agent = temp_dir("agent-nologin");
	let sessions = temp_dir("nologin-sessions");
	setup(cx);
	let mut env = vec![("PI_CODING_AGENT_DIR".to_owned(), agent.display().to_string()), ("PI_OFFLINE".to_owned(), "1".to_owned())];
	// Provider keys in the test environment would give pi a model.
	for (key, _) in std::env::vars() {
		if key.ends_with("_API_KEY") || key.ends_with("_OAUTH_TOKEN") || key.ends_with("_AUTH_TOKEN") {
			env.push((key, String::new()));
		}
	}
	let base = SessionOptions { session_dir: Some(sessions.clone()), env, ..Default::default() };
	let cwd = sessions.clone();
	let (handle, workspace) = cx.update(|cx| {
		gpui_kit::open_window(window_options(1200., 800.), cx, |window, cx| cx.new(|cx| Workspace::new(base, cwd, window, cx))).expect("open window")
	});
	let tab = workspace.read_with(cx, |w, _| w.tab_views()[0].clone());
	wait_until(cx, Duration::from_secs(60), "needs-login state", |cx| tab.read_with(cx, |v, _| v.needs_login()));
	cx.update_window(handle, |_, window, cx| {
		window.render_frame(cx);
		window.click("needs-login-open", cx);
		window.render_frame(cx);
	})
	.unwrap();
	let accounts = workspace.read_with(cx, |w, _| w.accounts_view()).expect("accounts panel opened");
	wait_until(cx, Duration::from_secs(60), "providers listed", |cx| accounts.read_with(cx, |v, _| v.providers().len() > 10));
	assert!(accounts.read_with(cx, |v, _| v.credentials().is_empty()));
	cx.update_window(handle, |_, window, cx| window.press("escape", cx)).unwrap();
	assert!(workspace.read_with(cx, |w, _| w.accounts_view()).is_none());
	_ = std::fs::remove_dir_all(&agent);
	_ = std::fs::remove_dir_all(&sessions);
}

/// In a scratch agent dir: API-key login through the panel stores the key
/// in auth.json, OAuth reaches the browser link and the paste-code prompt
/// and cancels cleanly, and logout removes the credential after confirming.
#[gpui_kit::test]
#[ignore = "spawns pi"]
async fn accounts_login_flows_and_logout(cx: &mut TestAppContext) {
	setup(cx);
	let agent = temp_dir("agent-accounts");
	let base = SessionOptions { env: vec![("PI_CODING_AGENT_DIR".to_owned(), agent.display().to_string())], ..Default::default() };
	let (handle, accounts) = cx.update(|cx| {
		gpui_kit::open_window(window_options(900., 800.), cx, |window, cx| cx.new(|cx| crate::accounts::AccountsView::new(&base, window, cx)))
			.expect("open window")
	});
	wait_until(cx, Duration::from_secs(60), "providers listed", |cx| accounts.read_with(cx, |v, _| v.providers().len() > 10));
	let render = |cx: &mut TestAppContext| cx.update_window(handle, |_, window, cx| window.render_frame(cx)).unwrap();
	let search = |cx: &mut TestAppContext, text: &str| {
		cx.update_window(handle, |_, window, cx| {
			window.click("accounts-search", cx);
			window.press("cmd-a", cx);
			window.press("backspace", cx);
			window.input(text, cx);
			window.render_frame(cx);
		})
		.unwrap()
	};

	// OpenRouter offers OAuth and an API key: 登录 shows both, and the key
	// method asks for it, masked.
	search(cx, "openrouter");
	cx.update_window(handle, |_, window, cx| {
		assert!(window.try_find(("provider", 1usize)).is_none(), "search narrows to one row");
		window.click(("provider-login", 0usize), cx);
		window.render_frame(cx);
		assert!(window.try_find(("method-oauth", 0usize)).is_some(), "both methods offered");
		window.click(("method-api-key", 0usize), cx);
	})
	.unwrap();
	wait_until(cx, Duration::from_secs(20), "key prompt", |cx| {
		accounts.read_with(cx, |v, _| v.flow().and_then(|f| f.prompt.as_ref()).is_some_and(|p| p.kind == crate::login::PromptKind::Secret))
	});
	let answer = accounts.read_with(cx, |v, _| v.answer_state());
	assert!(answer.read_with(cx, |s, _| s.presentation().is_masked()), "secret input is masked");
	render(cx);
	cx.update_window(handle, |_, window, cx| {
		window.click("login-answer", cx);
		window.input("sk-or-pi-desktop-test", cx);
		window.press("enter", cx);
	})
	.unwrap();
	wait_until(cx, Duration::from_secs(20), "logged in", |cx| accounts.read_with(cx, |v, _| v.flow().is_none() && v.credentials().iter().any(|c| c.provider_id == "openrouter")));
	let auth = std::fs::read_to_string(agent.join("auth.json")).unwrap();
	assert!(auth.contains("sk-or-pi-desktop-test"), "key stored in the scratch auth.json");

	// Anthropic has OAuth and an API key: pick OAuth, then browser login.
	search(cx, "anthropic");
	cx.update_window(handle, |_, window, cx| window.click(("provider-login", 0usize), cx)).unwrap();
	render(cx);
	cx.update_window(handle, |_, window, cx| window.click(("method-oauth", 0usize), cx)).unwrap();
	wait_until(cx, Duration::from_secs(20), "method select", |cx| {
		accounts.read_with(cx, |v, _| v.flow().and_then(|f| f.prompt.as_ref()).is_some_and(|p| matches!(p.kind, crate::login::PromptKind::Select(_))))
	});
	render(cx);
	cx.update_window(handle, |_, window, cx| window.click(("login-option", 0usize), cx)).unwrap();
	wait_until(cx, Duration::from_secs(30), "authorize link and code prompt", |cx| {
		accounts.read_with(cx, |v, _| {
			v.flow().is_some_and(|f| f.url.is_some() && f.prompt.as_ref().is_some_and(|p| p.kind == crate::login::PromptKind::ManualCode))
		})
	});
	let url = accounts.read_with(cx, |v, _| v.flow().unwrap().url.clone().unwrap());
	assert!(url.starts_with("https://"), "{url}");
	assert_eq!(cx.opened_url().as_deref(), Some(url.as_str()), "the browser opens the authorize URL");
	render(cx);
	cx.update_window(handle, |_, window, cx| window.click("login-copy", cx)).unwrap();
	assert_eq!(cx.read_from_clipboard().and_then(|item| item.text()).as_deref(), Some(url.as_str()));
	cx.update_window(handle, |_, window, cx| window.click("login-cancel", cx)).unwrap();
	wait_until(cx, Duration::from_secs(20), "login cancelled", |cx| accounts.read_with(cx, |v, _| v.flow().is_none()));
	assert!(matches!(accounts.read_with(cx, |v, _| v.message().cloned()), Some(crate::accounts::Message::Info(t)) if t == "已取消登录"));
	assert!(accounts.read_with(cx, |v, _| v.credentials().iter().all(|c| c.provider_id != "anthropic")), "cancel stored nothing");

	// Logout OpenRouter after confirming.
	search(cx, "openrouter");
	cx.update_window(handle, |_, window, cx| {
		window.click(("provider-logout", 0usize), cx);
		window.render_frame(cx);
		window.click(("provider-confirm", 0usize), cx);
	})
	.unwrap();
	wait_until(cx, Duration::from_secs(20), "logged out", |cx| accounts.read_with(cx, |v, _| v.credentials().is_empty()));
	assert!(!std::fs::read_to_string(agent.join("auth.json")).unwrap_or_default().contains("sk-or-pi-desktop-test"));
	drop(accounts);
	_ = std::fs::remove_dir_all(&agent);
}

/// ⌘N/⌘1–9/⌘W manage tabs; a new session writes no file until it is used.
#[gpui_kit::test]
#[ignore = "spawns pi"]
async fn shortcuts_open_switch_and_close_tabs(cx: &mut TestAppContext) {
	let root = temp_dir("tabs");
	std::fs::create_dir_all(root.join("sessions")).unwrap();
	let (handle, workspace) = open_workspace(cx, &root.join("sessions"), &root, &["--no-extensions", "--no-tools"]);
	let press = |cx: &mut TestAppContext, key: &str| cx.update_window(handle, |_, window, cx| window.press(key, cx)).unwrap();
	let tabs = |cx: &mut TestAppContext| workspace.read_with(cx, |w, _| (w.tab_views().len(), w.active_index()));
	press(cx, "secondary-n");
	press(cx, "secondary-n");
	assert_eq!(tabs(cx), (3, 2));
	press(cx, "secondary-1");
	assert_eq!(tabs(cx), (3, 0));
	let views = workspace.read_with(cx, |w, _| w.tab_views());
	for view in &views {
		wait_until(cx, Duration::from_secs(90), "tab connects", |cx| view.read_with(cx, |v, _| v.is_connected()));
	}
	let first_pid = views[0].read_with(cx, |v, _| v.pid()).unwrap();
	press(cx, "secondary-w");
	assert_eq!(tabs(cx), (2, 0));
	wait_until(cx, Duration::from_secs(10), "closed tab's process exits", |_| !process_alive(first_pid));
	press(cx, "secondary-w");
	press(cx, "secondary-w");
	assert_eq!(tabs(cx).0, 1, "closing the last tab leaves a fresh one");
	let fresh = workspace.read_with(cx, |w, _| w.tab_views()[0].clone());
	wait_until(cx, Duration::from_secs(90), "fresh tab connects", |cx| fresh.read_with(cx, |v, _| v.is_connected()));
	assert!(workspace.read_with(cx, |w, _| w.listed().is_empty()));
	_ = std::fs::remove_dir_all(&root);
}

/// ⌘K runs a theme action and drops a slash command in the composer; ⌘M
/// lists only models, starting on the current one.
#[gpui_kit::test]
#[ignore = "spawns pi"]
async fn palette_and_model_shortcuts(cx: &mut TestAppContext) {
	use gpui_kit::component::ActiveTheme as _;
	let root = temp_dir("palette");
	std::fs::create_dir_all(root.join("sessions")).unwrap();
	let (handle, workspace) = open_workspace(cx, &root.join("sessions"), &root, &["--no-extensions", "--no-tools"]);
	let view = workspace.read_with(cx, |w, _| w.tab_views()[0].clone());
	wait_until(cx, Duration::from_secs(90), "choices load", |cx| view.read_with(cx, |v, _| v.is_connected() && v.state().is_some() && v.choices().0 > 1));
	let press = |cx: &mut TestAppContext, key: &str| {
		cx.update_window(handle, |_, window, cx| {
			window.press(key, cx);
			window.render_frame(cx);
		})
		.unwrap()
	};
	let typed = |cx: &mut TestAppContext, text: &str| {
		cx.update_window(handle, |_, window, cx| {
			window.input(text, cx);
			window.render_frame(cx);
		})
		.unwrap();
		cx.run_until_parked();
	};
	let palette = |cx: &mut TestAppContext| workspace.read_with(cx, |w, _| w.palette_view());

	press(cx, "secondary-k");
	let open = palette(cx).expect("⌘K opens the palette");
	typed(cx, "主题 深色");
	assert_eq!(open.read_with(cx, |p, cx| p.titles(cx))[0], "主题：深色");
	press(cx, "enter");
	assert!(palette(cx).is_none(), "running a pick closes the palette");
	assert!(cx.read(|cx| cx.theme().is_dark()));
	cx.update_window(handle, |_, window, cx| crate::theme::set_choice(crate::theme::ThemeChoice::System, window, cx)).unwrap();

	press(cx, "secondary-k");
	typed(cx, "/compact");
	press(cx, "enter");
	assert_eq!(view.read_with(cx, |v, cx| v.composer_text(cx)), "/compact ");

	press(cx, "secondary-m");
	let models = palette(cx).expect("⌘M opens the model picker");
	let (titles, selected) = models.read_with(cx, |p, cx| (p.titles(cx), p.selected()));
	let current = view.read_with(cx, |v, _| v.state().unwrap().model.clone().unwrap());
	assert_eq!(titles.len(), view.read_with(cx, |v, _| v.choices().0), "models only");
	assert_eq!(titles[selected], current.id, "starts on the current model");
	press(cx, "escape");
	assert!(palette(cx).is_none());
	_ = std::fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------- model calls

#[gpui_kit::test]
#[ignore = "spawns pi and calls a model"]
async fn sends_prompt_and_renders_streamed_reply(cx: &mut TestAppContext) {
	let (handle, view, dir) = open_chat(cx, "reply", &["--no-tools"]);
	type_and(cx, handle, "只回复：你好，桌面端", "enter");
	assert_eq!(view.read_with(cx, |v, cx| v.composer_text(cx)), "", "composer clears after sending");
	wait_until(cx, Duration::from_secs(180), "reply settles", |cx| view.read_with(cx, |v, _| v.is_idle() && v.assistant_text().is_some()));
	assert_eq!(view.read_with(cx, |v, _| v.user_texts()), ["只回复：你好，桌面端"]);
	let reply = view.read_with(cx, |v, _| v.assistant_text()).unwrap();
	assert!(reply.contains("你好"), "unexpected reply: {reply:?}");
	assert!(view.read_with(cx, |v, _| v.has_stats()));
	cx.update_window(handle, |_, window, cx| window.render_frame(cx)).unwrap();
	_ = std::fs::remove_dir_all(&dir);
}

/// pi's built-in tools: bash streams into its card, edit's diff parses with
/// line numbers and counts.
#[gpui_kit::test]
#[ignore = "spawns pi and calls a model"]
async fn bash_and_edit_cards(cx: &mut TestAppContext) {
	let (handle, view, dir) = open_chat(cx, "tools", &["--no-extensions", "--tools", "read,edit,bash"]);
	std::fs::write(dir.join("demo.rs"), "fn main() {\n    let a = 1;\n    println!(\"{}\", a);\n}\n").unwrap();
	type_and(
		cx,
		handle,
		"First run the bash command `printf hi`. Then in demo.rs rename the variable a to b (both lines) using one edit call. Nothing else.",
		"enter",
	);
	wait_until(cx, Duration::from_secs(240), "turn settles", |cx| view.read_with(cx, |v, _| v.is_idle() && v.assistant_text().is_some()));
	let (bash, diff) = view.read_with(cx, |v, _| {
		let runs = v.tool_runs();
		let calls: Vec<(String, String)> = v
			.rows()
			.into_iter()
			.flat_map(|r| r.blocks)
			.filter_map(|b| match b {
				Block::ToolCall { id, name, .. } => Some((id, name)),
				_ => None,
			})
			.collect();
		let bash = calls.iter().find(|(_, n)| n == "bash").and_then(|(id, _)| runs.get(id)).map(|r| r.output.clone());
		let diff = calls.iter().filter(|(_, n)| n == "edit").find_map(|(id, _)| runs.get(id)?.diff());
		(bash, diff)
	});
	assert_eq!(bash.as_deref().map(str::trim), Some("hi"));
	let diff = diff.expect("edit ran with a diff");
	assert_eq!((diff.added, diff.removed), (2, 2), "{diff:?}");
	assert!(diff.lines.iter().any(|l| l.number == Some(2)), "line numbers parsed: {diff:?}");
	assert!(std::fs::read_to_string(dir.join("demo.rs")).unwrap().contains("let b = 1;"));
	cx.update_window(handle, |_, window, cx| window.render_frame(cx)).unwrap();
	_ = std::fs::remove_dir_all(&dir);
}

/// While bash runs: Enter queues a follow-up (chip), × withdraws it, an IME
/// composition's Esc does not abort, and an empty composer's Esc stops the
/// run with one abort.
#[gpui_kit::test]
#[ignore = "spawns pi and calls a model"]
async fn busy_enter_queues_chip_removal_and_esc_aborts(cx: &mut TestAppContext) {
	let (handle, view, dir) = open_chat(cx, "abort", &["--no-extensions", "--tools", "bash"]);
	type_and(cx, handle, "Run this exact bash command and nothing else: sleep 60", "enter");
	wait_until(cx, Duration::from_secs(180), "bash starts", |cx| view.read_with(cx, |v, _| v.tool_running("bash")));

	type_and(cx, handle, "排队的追问", "enter");
	wait_until(cx, Duration::from_secs(20), "follow-up chip", |cx| view.read_with(cx, |v, _| v.queue().follow_up == ["排队的追问"]));
	cx.update_window(handle, |_, window, cx| {
		window.render_frame(cx);
		window.click(("queue-remove", 0usize), cx);
	})
	.unwrap();
	wait_until(cx, Duration::from_secs(20), "chip removed", |cx| view.read_with(cx, |v, _| v.queue().is_empty() && !v.control_pending()));

	let composer = view.read_with(cx, |v, _| v.composer());
	cx.update_window(handle, |_, window, cx| {
		window.click("composer", cx);
		let bounds = window.find("composer").bounds();
		let mut ime = TestInput::new(ElementInputHandler::new(bounds, composer.clone()), window);
		ime.compose_text(None, "ni", None, cx);
		assert!(ime.marked_text_range(cx).is_some(), "composition is marked");
		window.press("escape", cx);
	})
	.unwrap();
	assert_eq!(view.read_with(cx, |v, _| v.aborts), 0, "Esc during IME composition sent an abort");
	cx.update_window(handle, |_, window, cx| {
		composer.update(cx, |state, cx| state.clean(window, cx));
		window.render_frame(cx);
	})
	.unwrap();

	let stopped = Instant::now();
	cx.update_window(handle, |_, window, cx| window.press("escape", cx)).unwrap();
	assert_eq!(view.read_with(cx, |v, _| v.aborts), 1, "one Esc must send one abort");
	wait_until(cx, Duration::from_secs(30), "abort settles", |cx| view.read_with(cx, |v, _| v.is_idle()));
	assert!(stopped.elapsed() < Duration::from_secs(30), "abort took {:?}", stopped.elapsed());
	assert!(view.read_with(cx, |v, _| !v.tool_running("bash")), "bash still marked running");
	assert_eq!(view.read_with(cx, |v, _| v.user_texts()), ["Run this exact bash command and nothing else: sleep 60"]);
	_ = std::fs::remove_dir_all(&dir);
}

/// ⌘Enter queues a steer (delivered after the running tool call); a
/// follow-up chip's 插队 promotes it to a steer.
#[gpui_kit::test]
#[ignore = "spawns pi and calls a model"]
async fn steer_and_chip_promotion(cx: &mut TestAppContext) {
	let (handle, view, dir) = open_chat(cx, "steer", &["--no-extensions", "--tools", "bash"]);
	type_and(cx, handle, "Run this exact bash command and nothing else: sleep 8", "enter");
	wait_until(cx, Duration::from_secs(180), "bash starts", |cx| view.read_with(cx, |v, _| v.tool_running("bash")));

	type_and(cx, handle, "FOLLOW-A: reply only OK", "enter");
	wait_until(cx, Duration::from_secs(20), "follow-up chip", |cx| view.read_with(cx, |v, _| v.queue().follow_up == ["FOLLOW-A: reply only OK"]));
	cx.update_window(handle, |_, window, cx| {
		window.render_frame(cx);
		window.click(("queue-promote", 0usize), cx);
	})
	.unwrap();
	wait_until(cx, Duration::from_secs(20), "promoted to steer", |cx| {
		view.read_with(cx, |v, _| v.queue_log().iter().any(|q| q.steering == ["FOLLOW-A: reply only OK"] && q.follow_up.is_empty()))
	});
	type_and(cx, handle, "STEER-B: also reply OK", "secondary-enter");
	wait_until(cx, Duration::from_secs(20), "steer queued", |cx| {
		view.read_with(cx, |v, _| v.queue_log().iter().any(|q| q.steering.iter().any(|s| s.starts_with("STEER-B"))))
	});
	assert!(
		view.read_with(cx, |v, _| v.queue_log().iter().all(|q| q.follow_up.iter().all(|f| !f.starts_with("STEER-B")))),
		"⌘Enter queued a follow-up"
	);
	wait_until(cx, Duration::from_secs(180), "both delivered and settled", |cx| {
		view.read_with(cx, |v, _| {
			let texts = v.user_texts();
			v.is_idle() && texts.iter().any(|t| t.starts_with("FOLLOW-A")) && texts.iter().any(|t| t.starts_with("STEER-B"))
		})
	});
	_ = std::fs::remove_dir_all(&dir);
}

/// A solid-colour PNG, built by hand so the test needs no image crate.
fn solid_png(width: u32, height: u32, rgb: [u8; 3]) -> Vec<u8> {
	fn crc32(bytes: &[u8]) -> u32 {
		let mut crc = !0u32;
		for &b in bytes {
			crc ^= b as u32;
			for _ in 0..8 {
				crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
			}
		}
		!crc
	}
	fn adler32(bytes: &[u8]) -> u32 {
		let (mut a, mut b) = (1u32, 0u32);
		for &x in bytes {
			a = (a + x as u32) % 65521;
			b = (b + a) % 65521;
		}
		b << 16 | a
	}
	fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
		out.extend((data.len() as u32).to_be_bytes());
		let start = out.len();
		out.extend(kind);
		out.extend(data);
		let crc = crc32(&out[start..]);
		out.extend(crc.to_be_bytes());
	}
	let mut raw = Vec::new();
	for _ in 0..height {
		raw.push(0);
		for _ in 0..width {
			raw.extend(rgb);
		}
	}
	let mut zlib = vec![0x78, 0x01];
	let blocks: Vec<_> = raw.chunks(65_535).collect();
	for (i, block) in blocks.iter().enumerate() {
		zlib.push(u8::from(i + 1 == blocks.len()));
		let len = block.len() as u16;
		zlib.extend(len.to_le_bytes());
		zlib.extend((!len).to_le_bytes());
		zlib.extend(*block);
	}
	zlib.extend(adler32(&raw).to_be_bytes());
	let mut ihdr = Vec::new();
	ihdr.extend(width.to_be_bytes());
	ihdr.extend(height.to_be_bytes());
	ihdr.extend([8, 2, 0, 0, 0]);
	let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
	chunk(&mut png, b"IHDR", &ihdr);
	chunk(&mut png, b"IDAT", &zlib);
	chunk(&mut png, b"IEND", &[]);
	png
}

/// ⌘V with an image attaches it once, × removes another, and the model sees
/// the sent image; the transcript shows it.
#[gpui_kit::test]
#[ignore = "spawns pi and calls a model"]
async fn pasted_image_reaches_the_model(cx: &mut TestAppContext) {
	use gpui_kit::{ClipboardItem, Image, ImageFormat};
	let (handle, view, dir) = open_chat(cx, "image", &["--no-tools"]);
	let paste = |cx: &mut TestAppContext, item: ClipboardItem| {
		cx.write_to_clipboard(item);
		cx.update_window(handle, |_, window, cx| {
			window.render_frame(cx);
			window.click("composer", cx);
			window.press("cmd-v", cx);
			window.render_frame(cx);
		})
		.unwrap();
	};
	paste(cx, ClipboardItem::new_string("颜色？".into()));
	assert_eq!(view.read_with(cx, |v, cx| v.composer_text(cx)), "颜色？", "text paste still types");
	let red = Image::from_bytes(ImageFormat::Png, solid_png(64, 64, [255, 0, 0]));
	paste(cx, ClipboardItem::new_image(&red));
	paste(cx, ClipboardItem::new_image(&red));
	assert_eq!(view.read_with(cx, |v, _| v.attachment_labels()), ["图片.png"], "same image attaches once");
	let blue = Image::from_bytes(ImageFormat::Png, solid_png(64, 64, [0, 0, 255]));
	paste(cx, ClipboardItem::new_image(&blue));
	cx.update_window(handle, |_, window, cx| window.click(("attachment-remove", 1usize), cx)).unwrap();
	assert_eq!(view.read_with(cx, |v, _| v.attachment_labels().len()), 1, "× removes the blue one");
	cx.update_window(handle, |_, window, cx| {
		window.input("只用一个中文词回答这张图片的颜色", cx);
		window.press("enter", cx);
	})
	.unwrap();
	assert!(view.read_with(cx, |v, _| v.attachment_labels().is_empty()), "attachments clear on send");
	wait_until(cx, Duration::from_secs(180), "reply settles", |cx| view.read_with(cx, |v, _| v.is_idle() && v.assistant_text().is_some()));
	let reply = view.read_with(cx, |v, _| v.assistant_text()).unwrap();
	assert!(reply.contains('红'), "model did not see the red image: {reply:?}");
	let (row, user) =
		view.read_with(cx, |v, _| v.rows().into_iter().enumerate().find(|(_, r)| r.role == Role::User).map(|(i, r)| (i, r.blocks)).unwrap());
	let block = user.iter().position(|b| matches!(b, Block::Image { data, .. } if !data.is_empty())).expect("user message has an image");
	cx.update_window(handle, |_, window, cx| {
		window.render_frame(cx);
		assert!(window.try_find(("transcript-image", row * 64 + block)).is_some(), "transcript shows the image");
	})
	.unwrap();
	_ = std::fs::remove_dir_all(&dir);
}

/// A session created outside the app (`pi -p`, same files as the TUI) is
/// listed with its first message as title, opens with its history, and
/// continues; a killed process reconnects with `--session`; fork opens a
/// copy in a new file; ⌘W stops the process.
#[gpui_kit::test]
#[ignore = "spawns pi and calls a model"]
async fn external_session_is_listed_resumed_reconnected_and_forked(cx: &mut TestAppContext) {
	let root = temp_dir("sessions");
	let (sessions, work) = (root.join("sessions"), root.join("work"));
	std::fs::create_dir_all(&sessions).unwrap();
	std::fs::create_dir_all(&work).unwrap();
	let status = std::process::Command::new(crate::launch_env::locate_pi())
		.args(["-p", "--no-tools", "--no-extensions", "--session-dir"])
		.arg(&sessions)
		.arg("记住暗号：蓝鲸42。只回复：好")
		.current_dir(&work)
		.stdout(std::process::Stdio::null())
		.stderr(std::process::Stdio::null())
		.status()
		.expect("run pi -p");
	assert!(status.success());

	let (handle, workspace) = open_workspace(cx, &sessions, &work, &["--no-tools", "--no-extensions"]);
	wait_until(cx, Duration::from_secs(30), "session listed", |cx| workspace.read_with(cx, |w, _| w.listed().len() == 1));
	let (original, title) = workspace.read_with(cx, |w, _| (w.listed()[0].path.clone(), w.listed()[0].title.clone()));
	assert_eq!(title.as_deref(), Some("记住暗号：蓝鲸42。只回复：好"));

	cx.update_window(handle, |_, window, cx| {
		window.render_frame(cx);
		window.click(("session-row", 0usize), cx);
	})
	.unwrap();
	assert_eq!(workspace.read_with(cx, |w, _| (w.tab_views().len(), w.active_index())), (2, 1));
	let tab = workspace.read_with(cx, |w, _| w.tab_views()[1].clone());
	wait_until(cx, Duration::from_secs(90), "opened tab connects", |cx| tab.read_with(cx, |v, _| v.is_connected()));
	assert_eq!(tab.read_with(cx, |v, _| v.user_texts()), ["记住暗号：蓝鲸42。只回复：好"]);
	assert_eq!(tab.read_with(cx, |v, _| v.session_file().map(Path::to_owned)), Some(original.clone()));

	cx.update_window(handle, |_, window, cx| {
		window.press("secondary-1", cx);
		window.render_frame(cx);
		window.click(("session-row", 0usize), cx);
	})
	.unwrap();
	assert_eq!(workspace.read_with(cx, |w, _| (w.tab_views().len(), w.active_index())), (2, 1), "switches to the open tab");

	type_and(cx, handle, "暗号是什么？只回复暗号本身", "enter");
	wait_until(cx, Duration::from_secs(180), "continued reply", |cx| tab.read_with(cx, |v, _| v.is_idle() && v.assistant_texts().len() == 2));
	assert!(tab.read_with(cx, |v, _| v.assistant_texts()[1].contains("蓝鲸42")), "{:?}", tab.read_with(cx, |v, _| v.assistant_texts()));
	assert!(std::fs::read_to_string(&original).unwrap().contains("暗号是什么"), "turn persisted to the session file");

	let pid = tab.read_with(cx, |v, _| v.pid()).expect("pid");
	std::process::Command::new("kill").args(["-9", &pid.to_string()]).status().unwrap();
	wait_until(cx, Duration::from_secs(30), "disconnect noticed", |cx| tab.read_with(cx, |v, _| v.is_disconnected()));
	cx.update_window(handle, |_, window, cx| {
		window.render_frame(cx);
		window.click("reconnect-banner", cx);
	})
	.unwrap();
	wait_until(cx, Duration::from_secs(90), "reconnected", |cx| tab.read_with(cx, |v, _| v.is_connected()));
	assert_ne!(tab.read_with(cx, |v, _| v.pid()), Some(pid));
	assert_eq!(tab.read_with(cx, |v, _| v.user_texts()).len(), 2, "history reloaded from the file");

	cx.update_window(handle, |_, window, cx| {
		workspace.update(cx, |w, cx| {
			let s = w.listed().iter().find(|s| s.path == original).unwrap().clone();
			w.open_session(&s.path, &s.cwd, true, window, cx);
		});
	})
	.unwrap();
	let fork = workspace.read_with(cx, |w, _| w.tab_views()[2].clone());
	wait_until(cx, Duration::from_secs(90), "fork connects", |cx| fork.read_with(cx, |v, _| v.is_connected() && v.session_file().is_some_and(|f| f != original)));
	assert_eq!(fork.read_with(cx, |v, _| v.user_texts()).len(), 2, "fork carries the history");
	wait_until(cx, Duration::from_secs(30), "fork listed", |cx| workspace.read_with(cx, |w, _| w.listed().len() == 2));

	let fork_pid = fork.read_with(cx, |v, _| v.pid()).expect("fork pid");
	cx.update_window(handle, |_, window, cx| window.press("secondary-w", cx)).unwrap();
	assert_eq!(workspace.read_with(cx, |w, _| w.tab_views().len()), 2);
	wait_until(cx, Duration::from_secs(10), "forked process exits", |_| !process_alive(fork_pid));
	_ = std::fs::remove_dir_all(&root);
}

struct Blank;

impl gpui_kit::Render for Blank {
	fn render(&mut self, _: &mut gpui_kit::Window, _: &mut gpui_kit::Context<Self>) -> impl gpui_kit::IntoElement {
		gpui_kit::div()
	}
}

/// pi-free: a manual theme choice overrides the system appearance; "follow
/// the system" goes back to it (the test window is light).
#[gpui_kit::test]
async fn theme_choice_overrides_and_follows_system(cx: &mut TestAppContext) {
	use gpui_kit::component::ActiveTheme as _;
	use crate::theme::ThemeChoice;
	cx.update(gpui_kit::init);
	cx.update(crate::theme::apply);
	let (handle, _) = cx.update(|cx| gpui_kit::open_window(WindowOptions::default(), cx, |_, cx| cx.new(|_| Blank)).unwrap());
	let dark = |cx: &mut TestAppContext| cx.read(|cx| (cx.theme().is_dark(), crate::theme::palette(cx).background));
	assert_eq!(dark(cx), (false, crate::theme::LIGHT.background));
	for (choice, expect) in [
		(ThemeChoice::Dark, (true, crate::theme::DARK.background)),
		(ThemeChoice::Light, (false, crate::theme::LIGHT.background)),
		(ThemeChoice::System, (false, crate::theme::LIGHT.background)),
	] {
		cx.update_window(handle, |_, window, cx| crate::theme::set_choice(choice, window, cx)).unwrap();
		assert_eq!(dark(cx), expect, "{choice:?}");
		assert_eq!(cx.read(crate::theme::choice), choice);
	}
}
