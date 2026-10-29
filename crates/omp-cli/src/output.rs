//! Text and NDJSON output.
//!
//! `text` prints the final assistant text on stdout and progress on stderr
//! (`print-mode.ts:228-231`); `json` emits one header line plus every event as
//! NDJSON (`print-mode.ts:112-156`).

use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::ValueEnum;

use crate::{Result, agent::Event};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum OutputMode {
	/// Human-readable final text.
	#[default]
	Text,
	/// Newline-delimited session header and events.
	Json,
}

#[derive(Debug)]
pub struct Reporter<W: Write, E: Write> {
	mode: OutputMode,
	out: W,
	err: E,
	working_announced: bool,
	final_text: Option<String>,
	api: String,
	provider: String,
	model: String,
}

impl<W: Write, E: Write> Reporter<W, E> {
	pub fn new(mode: OutputMode, out: W, err: E) -> Self {
		Self {
			mode,
			out,
			err,
			working_announced: false,
			final_text: None,
			api: "openai-completions".to_string(),
			provider: "openai".to_string(),
			model: "gpt-4o".to_string(),
		}
	}

	pub fn with_model_info(
		mut self,
		api: impl Into<String>,
		provider: impl Into<String>,
		model: impl Into<String>,
	) -> Self {
		self.api = api.into();
		self.provider = provider.into();
		self.model = model.into();
		self
	}

	pub fn set_model_info(
		&mut self,
		api: impl Into<String>,
		provider: impl Into<String>,
		model: impl Into<String>,
	) {
		self.api = api.into();
		self.provider = provider.into();
		self.model = model.into();
	}

	/// NDJSON session header. Emitted once, JSON mode only.
	pub fn session_header(&mut self, id: &str, cwd: &std::path::Path) -> Result<()> {
		if self.mode != OutputMode::Json {
			return Ok(());
		}
		let header = serde_json::json!({
			"type": "session",
			"version": 1,
			"id": id,
			"timestamp": iso8601(SystemTime::now()),
			"cwd": cwd.display().to_string(),
		});
		writeln!(self.out, "{header}")?;
		self.out.flush()?;
		Ok(())
	}

	/// Called before a turn starts: text mode announces work on stderr.
	pub fn begin_turn(&mut self) -> Result<()> {
		if self.mode == OutputMode::Text && !self.working_announced {
			writeln!(self.err, "Working...")?;
			self.working_announced = true;
		}
		Ok(())
	}

	/// Emit one event: NDJSON in JSON mode, stderr progress in text mode.
	pub fn event(&mut self, event: &Event) -> Result<()> {
		if self.mode == OutputMode::Json {
			let ts = current_timestamp_ms();
			match event {
				Event::MessageStart { role } => {
					let envelope = serde_json::json!({
						"type": "message_start",
						"message": { "role": role }
					});
					writeln!(self.out, "{envelope}")?;
				},
				Event::MessageUpdate { delta } => {
					let envelope = serde_json::json!({
						"type": "message_update",
						"assistantMessageEvent": {
							"type": "text_delta",
							"contentIndex": 0,
							"delta": delta,
						}
					});
					writeln!(self.out, "{envelope}")?;
				},
				Event::MessageEnd { message, stop_reason, api, provider, model } => {
					let eff_api = api.as_deref().unwrap_or(&self.api);
					let eff_provider = provider.as_deref().unwrap_or(&self.provider);
					let eff_model = model.as_deref().unwrap_or(&self.model);
					let eff_stop = stop_reason.as_deref().unwrap_or(if message.tool_calls.is_empty() {
						"stop"
					} else {
						"toolUse"
					});
					let asst_event = match eff_stop {
						"aborted" => serde_json::json!({
							"type": "error",
							"reason": "aborted",
						}),
						"error" => serde_json::json!({
							"type": "error",
							"reason": "error",
						}),
						other => serde_json::json!({
							"type": "done",
							"reason": other,
						}),
					};
					let done_envelope = serde_json::json!({
						"type": "message_update",
						"assistantMessageEvent": asst_event,
					});
					writeln!(self.out, "{done_envelope}")?;
					let asst_msg = format_ts_assistant_message(
						message,
						eff_api,
						eff_provider,
						eff_model,
						Some(eff_stop),
						ts,
					);
					let envelope = serde_json::json!({
						"type": "message_end",
						"message": asst_msg,
					});
					writeln!(self.out, "{envelope}")?;
				},
				Event::ToolStart { id, tool, args } => {
					let envelope = serde_json::json!({
						"type": "tool_execution_start",
						"toolCallId": id,
						"toolName": tool,
						"args": args,
					});
					writeln!(self.out, "{envelope}")?;
				},
				Event::ToolEnd { id, tool, result, error } => {
					let envelope = serde_json::json!({
						"type": "tool_execution_end",
						"toolCallId": id,
						"toolName": tool,
						"result": result,
						"isError": error.is_some(),
					});
					writeln!(self.out, "{envelope}")?;
				},
				Event::TurnEnd { message, tool_results, stop_reason, api, provider, model } => {
					let eff_api = api.as_deref().unwrap_or(&self.api);
					let eff_provider = provider.as_deref().unwrap_or(&self.provider);
					let eff_model = model.as_deref().unwrap_or(&self.model);
					let asst_msg = format_ts_assistant_message(
						message,
						eff_api,
						eff_provider,
						eff_model,
						Some(stop_reason.as_str()),
						ts,
					);
					let tool_results_json: Vec<serde_json::Value> = tool_results
						.iter()
						.map(|r| {
							let tool_name = message
								.tool_calls
								.iter()
								.find(|c| c.id == r.tool_call_id)
								.map(|c| c.name.as_str())
								.unwrap_or("tool");
							format_ts_tool_result(&r.tool_call_id, tool_name, &r.content, r.is_error, ts)
						})
						.collect();
					let envelope = serde_json::json!({
						"type": "turn_end",
						"message": asst_msg,
						"toolResults": tool_results_json,
					});
					writeln!(self.out, "{envelope}")?;
				},
				Event::AgentEnd { messages, api, provider, model } => {
					let eff_api = api.as_deref().unwrap_or(&self.api);
					let eff_provider = provider.as_deref().unwrap_or(&self.provider);
					let eff_model = model.as_deref().unwrap_or(&self.model);
					let msgs_json: Vec<serde_json::Value> = messages
						.iter()
						.map(|m| format_ts_message(m, eff_api, eff_provider, eff_model, ts))
						.collect();
					let envelope = serde_json::json!({
						"type": "agent_end",
						"messages": msgs_json,
					});
					writeln!(self.out, "{envelope}")?;
				},
			}
			self.out.flush()?;
		}
		match event {
			Event::MessageEnd { message, .. } => {
				self.final_text = Some(message.content.clone());
			},
			Event::AgentEnd { messages, .. } => {
				if let Some(last_asst) = messages.iter().rfind(|m| m.role == pi_agent::Role::Assistant) {
					self.final_text = Some(last_asst.content.clone());
				}
			},
			Event::ToolStart { tool, .. } => {
				if self.mode == OutputMode::Text {
					writeln!(self.err, "→ {tool}")?;
				}
			},
			Event::ToolEnd { tool, error: Some(error), .. } => {
				if self.mode == OutputMode::Text {
					writeln!(self.err, "✗ {tool}: {error}")?;
				}
			},
			_ => {},
		}
		Ok(())
	}

	/// Flush the final assistant text (text mode) and stdout.
	pub fn finish(&mut self) -> Result<()> {
		if self.mode == OutputMode::Text
			&& let Some(text) = self.final_text.take()
		{
			writeln!(self.out, "{text}")?;
		}
		self.out.flush()?;
		self.err.flush()?;
		Ok(())
	}
}

fn current_timestamp_ms() -> u64 {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map(|d| u64::try_from(d.as_millis()).unwrap_or(0))
		.unwrap_or(0)
}

fn format_ts_assistant_message(
	message: &pi_agent::Message,
	api: &str,
	provider: &str,
	model: &str,
	stop_reason: Option<&str>,
	timestamp: u64,
) -> serde_json::Value {
	let mut content = Vec::new();
	if !message.content.is_empty() {
		content.push(serde_json::json!({
			"type": "text",
			"text": message.content,
		}));
	}
	for call in &message.tool_calls {
		content.push(serde_json::json!({
			"type": "toolCall",
			"id": call.id,
			"name": call.name,
			"arguments": call.arguments,
		}));
	}
	let stop_reason_ts = match stop_reason {
		Some("tool_use") | Some("toolUse") => "toolUse",
		Some("end_turn") | Some("stop") => "stop",
		Some("aborted") => "aborted",
		Some("error") => "error",
		Some(other) => other,
		None => {
			if message.tool_calls.is_empty() {
				"stop"
			} else {
				"toolUse"
			}
		},
	};
	let mut asst = serde_json::json!({
		"role": "assistant",
		"content": content,
		"api": api,
		"provider": provider,
		"model": model,
		"stopReason": stop_reason_ts,
		"timestamp": timestamp,
	});
	// TS `AssistantMessage.usage` — emitted only when the provider actually
	// reported usage; an unreported turn stays absent instead of serializing
	// zeros-as-fact. `cost` is omitted too because no price is known here.
	if let Some(usage) = crate::session::reported_usage(message) {
		asst["usage"] = serde_json::json!(usage);
	}
	asst
}

fn format_ts_tool_result(
	tool_call_id: &str,
	tool_name: &str,
	content: &str,
	is_error: bool,
	timestamp: u64,
) -> serde_json::Value {
	serde_json::json!({
		"role": "toolResult",
		"toolCallId": tool_call_id,
		"toolName": tool_name,
		"content": [
			{
				"type": "text",
				"text": content,
			}
		],
		"isError": is_error,
		"timestamp": timestamp,
	})
}

fn format_ts_message(
	message: &pi_agent::Message,
	api: &str,
	provider: &str,
	model: &str,
	timestamp: u64,
) -> serde_json::Value {
	match message.role {
		pi_agent::Role::User => {
			let content = if message.parts.is_empty() {
				vec![serde_json::json!({
					"type": "text",
					"text": message.content,
				})]
			} else {
				message
					.parts
					.iter()
					.map(|part| match part {
						pi_agent::ContentPart::Text { text } => serde_json::json!({
							"type": "text",
							"text": text,
						}),
						pi_agent::ContentPart::Image { data, mime_type } => serde_json::json!({
							"type": "image",
							"data": data,
							"mimeType": mime_type,
						}),
					})
					.collect()
			};
			serde_json::json!({
				"role": "user",
				"content": content,
				"timestamp": timestamp,
			})
		},
		pi_agent::Role::Assistant => format_ts_assistant_message(message, api, provider, model, None, timestamp),
		pi_agent::Role::Tool => format_ts_tool_result(
			message.tool_call_id.as_deref().unwrap_or_default(),
			message.name.as_deref().unwrap_or("tool"),
			&message.content,
			message.is_error,
			timestamp,
		),
		pi_agent::Role::System => serde_json::json!({
			"role": "system",
			"content": [
				{
					"type": "text",
					"text": message.content,
				}
			],
			"timestamp": timestamp,
		}),
	}
}

/// Session id: wall-clock nanos XOR pid, hex. Unique enough for a headless run.
#[must_use]
pub fn session_id() -> String {
	let nanos = SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map_or(0, |duration| duration.as_nanos());
	format!("{:x}{:x}", std::process::id(), nanos)
}

/// RFC 3339 UTC timestamp without a chrono dependency.
#[must_use]
pub fn iso8601(time: SystemTime) -> String {
	let duration = time.duration_since(UNIX_EPOCH).unwrap_or_default();
	let seconds = duration.as_secs();
	let days = i64::try_from(seconds / 86_400).unwrap_or(0);
	let seconds_of_day = seconds % 86_400;
	let (year, month, day) = civil_from_days(days);
	format!(
		"{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
		seconds_of_day / 3_600,
		(seconds_of_day % 3_600) / 60,
		seconds_of_day % 60,
		duration.subsec_millis(),
	)
}

/// Howard Hinnant's days-to-civil conversion.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
	let z = days + 719_468;
	let era = z.div_euclid(146_097);
	let day_of_era = z.rem_euclid(146_097);
	let year_of_era =
		(day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
	let year = year_of_era + era * 400;
	let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
	let month_prime = (5 * day_of_year + 2) / 153;
	let day = u32::try_from(day_of_year - (153 * month_prime + 2) / 5 + 1).unwrap_or(1);
	let month = u32::try_from(if month_prime < 10 {
		month_prime + 3
	} else {
		month_prime - 9
	})
	.unwrap_or(1);
	let year = if month <= 2 { year + 1 } else { year };
	(year, month, day)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn json_mode_emits_header_then_ndjson_events() {
		let mut out = Vec::new();
		let mut err = Vec::new();
		{
			let mut reporter = Reporter::new(OutputMode::Json, &mut out, &mut err)
				.with_model_info("openai-completions", "openai", "gpt-4o");
			reporter
				.session_header("abc", std::path::Path::new("/work"))
				.expect("header");
			reporter
				.event(&Event::message_end(pi_agent::Message::assistant("final")))
				.expect("event");
			reporter.finish().expect("finish");
		}
		let out = String::from_utf8(out).expect("utf8");
		let lines: Vec<&str> = out.lines().collect();
		assert_eq!(lines.len(), 3);
		assert!(lines[0].starts_with(r#"{"type":"session""#));
		let update: serde_json::Value = serde_json::from_str(lines[1]).expect("json");
		assert_eq!(update, serde_json::json!({"assistantMessageEvent": {"reason": "stop", "type": "done"}, "type": "message_update"}));
		let v: serde_json::Value = serde_json::from_str(lines[2]).expect("json");
		assert_eq!(v["type"], "message_end");
		assert_eq!(v["message"]["role"], "assistant");
		assert_eq!(v["message"]["content"][0]["type"], "text");
		assert_eq!(v["message"]["content"][0]["text"], "final");
		assert_eq!(v["message"]["api"], "openai-completions");
		assert_eq!(v["message"]["provider"], "openai");
		assert_eq!(v["message"]["model"], "gpt-4o");
		assert_eq!(v["message"]["stopReason"], "stop");
		assert!(v["message"].get("providerPayload").is_none());
		assert!(v["message"].get("usage").is_none());
		assert!(err.is_empty());
	}

	#[test]
	fn json_mode_emits_reported_usage_without_fake_cost() {
		let mut out = Vec::new();
		let mut err = Vec::new();
		{
			let mut reporter = Reporter::new(OutputMode::Json, &mut out, &mut err)
				.with_model_info("openai-completions", "openai", "gpt-4o");
			let mut asst = pi_agent::Message::assistant("final");
			asst.usage = Some(pi_agent::TokenUsage {
				input: 10,
				output: 4,
				cache_read: 2,
				cache_write: 1,
				total_tokens: 17,
				reasoning_tokens: None,
			});
			reporter.event(&Event::message_end(asst)).expect("event");
			reporter.finish().expect("finish");
		}
		let out = String::from_utf8(out).expect("utf8");
		let line = out.lines().last().expect("message_end line");
		let v: serde_json::Value = serde_json::from_str(line).expect("json");
		assert_eq!(v["type"], "message_end");
		// TS `Usage` shape: camelCase counters, no `cost` when no price is known.
		assert_eq!(v["message"]["usage"], serde_json::json!({
			"input": 10,
			"output": 4,
			"cacheRead": 2,
			"cacheWrite": 1,
			"totalTokens": 17,
		}));
		assert!(err.is_empty());
	}

	#[test]
	fn json_mode_tool_batch_printable_envelope() {
		let mut out = Vec::new();
		let mut err = Vec::new();
		{
			let mut reporter = Reporter::new(OutputMode::Json, &mut out, &mut err)
				.with_model_info("test-api", "test-prov", "test-model");
			reporter
				.event(&Event::MessageStart { role: "assistant".to_string() })
				.unwrap();
			reporter
				.event(&Event::MessageUpdate { delta: "reading file".to_string() })
				.unwrap();
			let mut asst = pi_agent::Message::assistant("reading file");
			asst.tool_calls.push(pi_agent::ToolCall {
				id: "call_1".to_string(),
				name: "read".to_string(),
				arguments: serde_json::json!({"path": "a.txt"}),
			});
			reporter
				.event(&Event::message_end(asst.clone()))
				.unwrap();
			reporter
				.event(&Event::ToolStart {
					id: "call_1".to_string(),
					tool: "read".to_string(),
					args: serde_json::json!({"path": "a.txt"}),
				})
				.unwrap();
			reporter
				.event(&Event::ToolEnd {
					id: "call_1".to_string(),
					tool: "read".to_string(),
					result: "file data".to_string(),
					error: None,
				})
				.unwrap();
			reporter
				.event(&Event::TurnEnd {
					message: asst.clone(),
					tool_results: vec![pi_agent::ToolResult {
						tool_call_id: "call_1".to_string(),
						content: "file data".to_string(),
						is_error: false,
						terminate: false,
						details: None,
					}],
					stop_reason: "tool_use".to_string(),
					api: None,
					provider: None,
					model: None,
				})
				.unwrap();
			reporter
				.event(&Event::AgentEnd {
					messages: vec![
						pi_agent::Message::user("read it"),
						asst,
						pi_agent::Message::tool("call_1", "file data"),
					],
					api: None,
					provider: None,
					model: None,
				})
				.unwrap();
		}
		let out = String::from_utf8(out).expect("utf8");
		let lines: Vec<&str> = out.lines().collect();
		assert_eq!(lines.len(), 8);
		assert!(lines[0].contains(r#""type":"message_start""#));
		assert!(lines[1].contains(r#""type":"text_delta""#));
		assert!(lines[1].contains(r#""delta":"reading file""#));
		assert!(lines[2].contains(r#""type":"done""#));
		assert!(lines[2].contains(r#""reason":"toolUse""#));
		let me: serde_json::Value = serde_json::from_str(lines[3]).unwrap();
		assert_eq!(me["type"], "message_end");
		assert_eq!(me["message"]["content"][0]["type"], "text");
		assert_eq!(me["message"]["content"][1]["type"], "toolCall");
		assert_eq!(me["message"]["content"][1]["name"], "read");
		let ts: serde_json::Value = serde_json::from_str(lines[4]).unwrap();
		assert_eq!(ts["type"], "tool_execution_start");
		assert_eq!(ts["toolName"], "read");
		let te: serde_json::Value = serde_json::from_str(lines[5]).unwrap();
		assert_eq!(te["type"], "tool_execution_end");
		assert_eq!(te["result"], "file data");
		assert_eq!(te["isError"], false);
		let tne: serde_json::Value = serde_json::from_str(lines[6]).unwrap();
		assert_eq!(tne["type"], "turn_end");
		assert_eq!(tne["toolResults"][0]["toolName"], "read");
		let ae: serde_json::Value = serde_json::from_str(lines[7]).unwrap();
		assert_eq!(ae["type"], "agent_end");
		assert_eq!(ae["messages"].as_array().unwrap().len(), 3);
	}

	#[test]
	fn text_mode_keeps_progress_on_stderr() {
		let mut out = Vec::new();
		let mut err = Vec::new();
		{
			let mut reporter = Reporter::new(OutputMode::Text, &mut out, &mut err);
			reporter.begin_turn().expect("begin");
			reporter
				.event(&Event::ToolStart {
					id: "call-1".into(),
					tool: "read".into(),
					args: serde_json::json!({}),
				})
				.expect("tool");
			reporter
				.event(&Event::AgentEnd {
					messages: vec![pi_agent::Message::assistant("answer")],
					api: None,
					provider: None,
					model: None,
				})
				.expect("end");
			reporter.finish().expect("finish");
		}
		assert_eq!(String::from_utf8(out).expect("utf8"), "answer\n");
		let err = String::from_utf8(err).expect("utf8");
		assert!(err.contains("Working..."));
		assert!(err.contains("→ read"));
	}

	#[test]
	fn agent_end_preserves_ordered_image_and_text_parts() {
		let mut out = Vec::new();
		let mut err = Vec::new();
		{
			let mut reporter = Reporter::new(OutputMode::Json, &mut out, &mut err);
			let user_msg = pi_agent::Message::user_with_parts(vec![
				pi_agent::ContentPart::text("describe this image:"),
				pi_agent::ContentPart::image("aW1hZ2VkYXRh", "image/png").unwrap(),
			]);
			reporter
				.event(&Event::AgentEnd {
					messages: vec![user_msg, pi_agent::Message::assistant("I see the image")],
					api: Some("openai-completions".to_string()),
					provider: Some("local".to_string()),
					model: Some("vision-model".to_string()),
				})
				.unwrap();
		}
		let out_str = String::from_utf8(out).expect("utf8");
		let ae: serde_json::Value = serde_json::from_str(out_str.trim()).unwrap();
		assert_eq!(ae["type"], "agent_end");
		let msgs = ae["messages"].as_array().expect("messages");
		assert_eq!(msgs.len(), 2);

		let user_content = msgs[0]["content"].as_array().expect("user content array");
		assert_eq!(user_content.len(), 2, "must have text and image parts");
		assert_eq!(user_content[0]["type"], "text");
		assert_eq!(user_content[0]["text"], "describe this image:");
		assert_eq!(user_content[1]["type"], "image");
		assert_eq!(user_content[1]["data"], "aW1hZ2VkYXRh");
		assert_eq!(user_content[1]["mimeType"], "image/png");
	}

	#[test]
	fn iso8601_encodes_epoch_and_leap_day() {
		assert_eq!(iso8601(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
		assert_eq!(
			iso8601(UNIX_EPOCH + std::time::Duration::from_secs(951_782_400)),
			"2000-02-29T00:00:00.000Z"
		);
	}
}
