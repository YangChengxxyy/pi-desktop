//! Modal answers to pi's dialog requests (`select`, `confirm`, `input`,
//! `editor`). One dialog is shown at a time; the rest wait in
//! `SessionModel::dialogs`. A dialog with a `timeout` shows the time left;
//! pi answers it with the default when it runs out.

use std::time::{Duration, Instant};

use gpui_kit::base::TestSupportExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState, Textarea, TextareaState};
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::prelude::*;
use gpui_kit::*;
use pi_rpc::{UiRequest, UiResponse};

use crate::theme;

pub enum DialogEvent {
	Answered(UiResponse),
}

impl EventEmitter<DialogEvent> for DialogView {}

pub struct DialogView {
	request:  UiRequest,
	/// Text for `input`.
	text:     Option<Entity<InputState>>,
	/// Text for `editor`.
	area:     Option<Entity<TextareaState>>,
	/// When pi gives up waiting, for `timeout` dialogs.
	deadline: Option<Instant>,
	_tick:    Option<Task<()>>,
}

impl DialogView {
	pub fn new(request: UiRequest, window: &mut Window, cx: &mut Context<Self>) -> Self {
		let mut view = Self { request, text: None, area: None, deadline: None, _tick: None };
		match &view.request {
			UiRequest::Input { placeholder, .. } => {
				let placeholder = placeholder.clone().unwrap_or_default();
				let state = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
				state.update(cx, |state, cx| state.focus(window, cx));
				view.text = Some(state);
			},
			UiRequest::Editor { prefill, .. } => {
				let prefill = prefill.clone().unwrap_or_default();
				let state = cx.new(|cx| TextareaState::new(window, cx).rows(8).default_value(prefill));
				state.update(cx, |state, cx| state.focus(window, cx));
				view.area = Some(state);
			},
			_ => {},
		}
		if let Some(ms) = view.request.timeout_ms() {
			view.deadline = Some(Instant::now() + Duration::from_millis(ms));
			// Repaint the countdown once a second.
			view._tick = Some(cx.spawn(async move |this, cx| {
				loop {
					cx.background_executor().timer(Duration::from_secs(1)).await;
					if this.update(cx, |_, cx| cx.notify()).is_err() {
						return;
					}
				}
			}));
		}
		view
	}

	pub fn id(&self) -> &str {
		self.request.dialog_id().unwrap_or_default()
	}

	/// When pi answers the dialog itself.
	pub fn deadline(&self) -> Option<Instant> {
		self.deadline
	}

	/// The request method, for tests.
	#[cfg(test)]
	pub(crate) fn kind(&self) -> &'static str {
		match &self.request {
			UiRequest::Select { .. } => "select",
			UiRequest::Confirm { .. } => "confirm",
			UiRequest::Input { .. } => "input",
			UiRequest::Editor { .. } => "editor",
			_ => "other",
		}
	}

	#[cfg(test)]
	pub(crate) fn select_options(&self) -> Option<Vec<String>> {
		match &self.request {
			UiRequest::Select { options, .. } => Some(options.clone()),
			_ => None,
		}
	}

	#[cfg(test)]
	pub(crate) fn title(&self) -> String {
		match &self.request {
			UiRequest::Select { title, .. }
			| UiRequest::Confirm { title, .. }
			| UiRequest::Input { title, .. }
			| UiRequest::Editor { title, .. } => title.clone(),
			_ => String::new(),
		}
	}

	fn answer(&mut self, response: UiResponse, cx: &mut Context<Self>) {
		cx.emit(DialogEvent::Answered(response));
	}

	fn submit(&mut self, cx: &mut Context<Self>) {
		let id = self.id().to_owned();
		let value = match &self.request {
			UiRequest::Input { .. } => self.text.as_ref().map(|t| t.read(cx).value().to_string()),
			UiRequest::Editor { .. } => self.area.as_ref().map(|t| t.read(cx).value().to_string()),
			_ => return,
		};
		self.answer(UiResponse::Value { id, value: value.unwrap_or_default() }, cx);
	}
}

impl Render for DialogView {
	fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
		let p = theme::palette(cx);
		let id = self.id().to_owned();
		let (title, body): (String, AnyElement) = match &self.request {
			UiRequest::Confirm { title, message, .. } => {
				(title.clone(), div().text_sm().whitespace_normal().child(message.clone()).into_any_element())
			},
			UiRequest::Select { title, options, .. } => {
				let rows = options.iter().enumerate().map(|(index, option)| {
					let response = UiResponse::Value { id: id.clone(), value: option.clone() };
					div()
						.id(("select-option", index))
						.flex()
						.flex_col()
						.px_3()
						.py_2()
						.rounded(px(6.))
						.border_1()
						.border_color(rgb(p.border))
						.cursor_pointer()
						.hover(|style| style.bg(rgb(p.accent)))
						.child(div().text_sm().font_weight(FontWeight::MEDIUM).child(option.clone()))
						.on_click(cx.listener(move |this, _, _, cx| this.answer(response.clone(), cx)))
						.test_support()
				});
				(title.clone(), div().flex().flex_col().gap_1().children(rows).into_any_element())
			},
			UiRequest::Input { title, .. } => (
				title.clone(),
				div().id("dialog-input").child(Input::new(self.text.as_ref().expect("input state"))).test_support().into_any_element(),
			),
			UiRequest::Editor { title, .. } => (
				title.clone(),
				div().id("dialog-editor").child(Textarea::new(self.area.as_ref().expect("editor state"))).test_support().into_any_element(),
			),
			_ => (String::new(), div().into_any_element()),
		};

		let remaining = self.deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()).as_secs());
		let cancel_id = id.clone();
		let mut actions = div()
			.flex()
			.items_center()
			.gap_2()
			.child(div().flex_1().text_xs().text_color(rgb(p.muted_foreground)).children(
				remaining.map(|secs| format!("{secs} 秒后自动取消")),
			))
			.child(
				Button::new("dialog-cancel")
					.ghost()
					.label("取消")
					.on_click(cx.listener(move |this, _, _, cx| this.answer(UiResponse::Cancelled { id: cancel_id.clone() }, cx))),
			);
		actions = match &self.request {
			UiRequest::Confirm { .. } => {
				let (no, yes) = (id.clone(), id.clone());
				actions
					.child(Button::new("dialog-no").outline().label("否").on_click(cx.listener(move |this, _, _, cx| {
						this.answer(UiResponse::Confirmed { id: no.clone(), confirmed: false }, cx)
					})))
					.child(Button::new("dialog-yes").primary().label("是").on_click(cx.listener(move |this, _, _, cx| {
						this.answer(UiResponse::Confirmed { id: yes.clone(), confirmed: true }, cx)
					})))
			},
			UiRequest::Select { .. } => actions,
			_ => actions.child(
				Button::new("dialog-submit").primary().label("提交").on_click(cx.listener(|this, _, _, cx| this.submit(cx))),
			),
		};

		div()
			.id("dialog")
			.w(px(560.))
			.max_h(px(620.))
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
			.child(div().text_base().font_weight(FontWeight::SEMIBOLD).whitespace_normal().child(title))
			.child(div().id("dialog-body").flex_1().overflow_y_scroll().child(body))
			.child(actions)
	}
}
