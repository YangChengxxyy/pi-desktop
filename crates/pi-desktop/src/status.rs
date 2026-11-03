//! Text for the status bar and the detail panel, derived from `get_state`
//! and `get_session_stats`. Free of GPUI so it can be tested directly.

use pi_rpc::{SessionState, SessionStats};

/// `84k`, `1.2M`: token counts short enough for the status bar.
pub fn tokens(n: i64) -> String {
	match n {
		..1_000 => n.to_string(),
		1_000..1_000_000 => format!("{}k", (n as f64 / 1_000.).round() as i64),
		_ => {
			let m = n as f64 / 1_000_000.;
			if (m - m.round()).abs() < 0.05 { format!("{}M", m.round() as i64) } else { format!("{m:.1}M") }
		},
	}
}

/// Context use above which the bar turns amber, then red.
pub const CONTEXT_WARN: f64 = 80.;
pub const CONTEXT_DANGER: f64 = 95.;

/// Context use from `get_session_stats`: (percent, used tokens, window).
/// `None` without a model; the percent is `None` right after a compaction,
/// until the next response reports usage.
pub fn context(stats: &SessionStats) -> Option<(Option<f64>, Option<i64>, i64)> {
	let usage = stats.context_usage.as_ref()?;
	Some((usage.percent, usage.tokens, usage.context_window))
}

/// `42% (84k / 200k)`, or `? / 200k` after a compaction.
pub fn context_label(stats: &SessionStats) -> Option<String> {
	let (percent, used, window) = context(stats)?;
	Some(match (percent, used) {
		(Some(percent), Some(used)) => format!("{percent:.0}% ({} / {})", tokens(used), tokens(window)),
		_ => format!("? / {}", tokens(window)),
	})
}

fn yes_no(value: bool) -> String {
	(if value { "是" } else { "否" }).into()
}

fn or_dash(value: Option<String>) -> String {
	value.filter(|v| !v.is_empty()).unwrap_or_else(|| "—".into())
}

/// One row per `get_state` field, keyed by its wire name, so the panel shows
/// every field even when it is unset.
pub fn state_rows(state: &SessionState) -> Vec<(&'static str, &'static str, String)> {
	vec![
		("sessionId", "会话 ID", state.session_id.clone()),
		("sessionName", "名称", or_dash(state.session_name.clone())),
		("sessionFile", "文件", or_dash(state.session_file.clone())),
		(
			"model",
			"模型",
			or_dash(state.model.as_ref().filter(|m| !m.is_unknown()).map(|m| {
				let window = if m.context_window > 0 { format!(" · {}", tokens(m.context_window)) } else { String::new() };
				format!("{}/{}{window}", m.provider, m.id)
			})),
		),
		("thinkingLevel", "思考档位", or_dash(state.thinking_level.map(|t| t.as_str().to_owned()))),
		("isStreaming", "输出中", yes_no(state.is_streaming)),
		("isCompacting", "压缩中", yes_no(state.is_compacting)),
		("steeringMode", "steer 模式", or_dash(state.steering_mode.clone())),
		("followUpMode", "follow-up 模式", or_dash(state.follow_up_mode.clone())),
		("autoCompactionEnabled", "自动压缩", (if state.auto_compaction_enabled { "开" } else { "关" }).into()),
		("messageCount", "消息数", state.message_count.to_string()),
		("pendingMessageCount", "排队数", state.pending_message_count.to_string()),
	]
}

#[cfg(test)]
mod tests {
	use serde_json::json;

	use super::*;

	#[::core::prelude::v1::test]
	fn token_counts_shorten() {
		assert_eq!(tokens(950), "950");
		assert_eq!(tokens(84_200), "84k");
		assert_eq!(tokens(1_000_000), "1M");
		assert_eq!(tokens(1_048_576), "1M");
		assert_eq!(tokens(1_250_000), "1.2M");
	}

	#[::core::prelude::v1::test]
	fn context_after_compaction_has_no_percent() {
		let stats: SessionStats = serde_json::from_value(json!({"contextUsage":{"tokens":84000,"contextWindow":200000,"percent":42.0}})).unwrap();
		assert_eq!(context_label(&stats).as_deref(), Some("42% (84k / 200k)"));
		let compacted: SessionStats = serde_json::from_value(json!({"contextUsage":{"tokens":null,"contextWindow":200000,"percent":null}})).unwrap();
		assert_eq!(context_label(&compacted).as_deref(), Some("? / 200k"));
		assert_eq!(context_label(&SessionStats::default()), None);
	}

	/// The `get_state` record pi 1.5 sent (model trimmed): every field has a
	/// row, including the ones it omits when unset.
	#[::core::prelude::v1::test]
	fn state_rows_cover_every_wire_field() {
		let wire = json!({
			"model":{"id":"auto/kimi-k3","name":"Kimi K3","provider":"magpie","contextWindow":1048576},
			"thinkingLevel":"high","isStreaming":false,"isCompacting":false,"steeringMode":"one-at-a-time",
			"followUpMode":"one-at-a-time","sessionFile":"/f.jsonl","sessionId":"s","sessionName":"n",
			"autoCompactionEnabled":true,"messageCount":0,"pendingMessageCount":0
		});
		let state: SessionState = serde_json::from_value(wire.clone()).unwrap();
		let rows = state_rows(&state);
		let keys: Vec<&str> = wire.as_object().unwrap().keys().map(String::as_str).collect();
		for key in &keys {
			assert!(rows.iter().any(|(k, ..)| k == key), "no row for {key}");
		}
		assert_eq!(rows.len(), keys.len(), "a row has no wire field");
		let value = |key: &str| rows.iter().find(|(k, ..)| *k == key).unwrap().2.clone();
		assert_eq!(value("model"), "magpie/auto/kimi-k3 · 1M");
		let unknown: SessionState = serde_json::from_value(json!({"model":{"id":"unknown","provider":"unknown"},"sessionId":"x"})).unwrap();
		assert_eq!(state_rows(&unknown)[3].2, "—", "pi's placeholder model is not a model");
	}
}
