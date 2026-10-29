//! Session persistence: TypeScript v3 JSONL logs, 256-byte title slots,
//! encoded directory layouts, terminal breadcrumbs, and legacy v1 compatibility.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use pi_agent::tools::todo::{TodoPhase, USER_TODO_EDIT_CUSTOM_TYPE};
use serde::{Deserialize, Serialize};

use crate::agent::Event;
use crate::{dirs, output};

/// Directory under the agent dir that holds CLI session logs.
pub const SESSIONS_SUBDIR: &str = "sessions";
pub const CURRENT_SESSION_VERSION: u32 = 3;
pub const SESSION_TITLE_SLOT_BYTES: usize = 256;
pub const SESSION_TITLE_SLOT_ENTRY_TYPE: &str = "title";
/// TS `USER_INTERRUPT_LABEL` (`packages/coding-agent/src/session/messages.ts`).
const USER_INTERRUPT_LABEL: &str = "Interrupted by user";
const LOG_EXTENSION: &str = "jsonl";

fn default_session_type() -> String {
	"session".to_string()
}

fn default_session_version() -> u32 {
	CURRENT_SESSION_VERSION
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionHeader {
	#[serde(default = "default_session_type")]
	#[serde(rename = "type")]
	pub kind: String,
	#[serde(default = "default_session_version")]
	pub version: u32,
	pub id: String,
	pub timestamp: String,
	pub cwd: String,
	#[serde(default, alias = "modelId")]
	pub model: Option<String>,
	#[serde(default)]
	pub provider: Option<String>,
	#[serde(default)]
	pub title: Option<String>,
	#[serde(default, rename = "titleSource")]
	pub title_source: Option<String>,
	#[serde(default, rename = "additionalDirectories")]
	pub additional_directories: Vec<String>,
	#[serde(default, rename = "parentSession")]
	pub parent_session: Option<String>,
	#[serde(default, rename = "previousSessionFiles")]
	pub previous_session_files: Vec<String>,
	#[serde(default, rename = "providerPromptCacheKey")]
	pub provider_prompt_cache_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TitleSlot {
	pub title: String,
	#[serde(default)]
	pub source: Option<String>,
	#[serde(rename = "updatedAt")]
	pub updated_at: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct TitleSlotLine {
	#[serde(rename = "type")]
	kind: String,
	v: u32,
	title: String,
	#[serde(skip_serializing_if = "Option::is_none")]
	source: Option<String>,
	#[serde(rename = "updatedAt")]
	updated_at: String,
	pad: String,
}

fn make_title_slot_raw(title: &str, source: Option<&str>, updated_at: &str, pad: &str) -> String {
	let entry = TitleSlotLine {
		kind: SESSION_TITLE_SLOT_ENTRY_TYPE.to_string(),
		v: 1,
		title: title.to_string(),
		source: source.map(str::to_string),
		updated_at: updated_at.to_string(),
		pad: pad.to_string(),
	};
	let json = serde_json::to_string(&entry).unwrap_or_default();
	format!("{json}\n")
}

/// Serialize the fixed-width first-line title slot, exactly 256 UTF-8 bytes including newline.
pub fn serialize_title_slot(title: &str, source: Option<&str>, updated_at: &str) -> String {
	let code_points: Vec<char> = title.chars().collect();
	let mut low = 0;
	let mut high = code_points.len();
	let mut best_mid = 0;

	while low <= high {
		let mid = (low + high) / 2;
		let candidate: String = code_points[..mid].iter().collect();
		let unpadded = make_title_slot_raw(&candidate, source, updated_at, "");
		if unpadded.len() <= SESSION_TITLE_SLOT_BYTES {
			best_mid = mid;
			low = mid + 1;
		} else {
			if mid == 0 {
				break;
			}
			high = mid - 1;
		}
	}

	let best_title: String = code_points[..best_mid].iter().collect();
	let unpadded = make_title_slot_raw(&best_title, source, updated_at, "");
	let pad_bytes = SESSION_TITLE_SLOT_BYTES.saturating_sub(unpadded.len());
	let pad = " ".repeat(pad_bytes);
	let line = make_title_slot_raw(&best_title, source, updated_at, &pad);
	debug_assert_eq!(line.as_bytes().len(), SESSION_TITLE_SLOT_BYTES);
	line
}

/// Parse a physical title slot JSONL line.
pub fn parse_title_slot(line: &str) -> Option<TitleSlot> {
	let first_line = line.lines().next()?;
	let parsed: TitleSlotLine = serde_json::from_str(first_line).ok()?;
	if parsed.kind != SESSION_TITLE_SLOT_ENTRY_TYPE || parsed.v != 1 {
		return None;
	}
	// TS parseTitleSlotObject: unknown source invalidates the slot.
	if let Some(source) = &parsed.source
		&& source != "auto"
		&& source != "user"
	{
		return None;
	}
	Some(TitleSlot {
		title: parsed.title,
		source: parsed.source,
		updated_at: parsed.updated_at,
	})
}

/// Replace the physical fixed-width title slot in session content.
pub fn overlay_title_slot(content: &str, new_slot: &str) -> String {
	let slot_bytes = new_slot.as_bytes();
	let content_bytes = content.as_bytes();
	if content_bytes.len() <= slot_bytes.len() {
		return new_slot.to_string();
	}
	let mut out = Vec::with_capacity(content_bytes.len());
	out.extend_from_slice(slot_bytes);
	out.extend_from_slice(&content_bytes[slot_bytes.len()..]);
	String::from_utf8(out).unwrap_or_else(|_| content.to_string())
}

/// TS cost block (`packages/catalog/src/types.ts` Usage.cost).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct Cost {
	#[serde(default)]
	pub input: f64,
	#[serde(default)]
	pub output: f64,
	#[serde(default, rename = "cacheRead")]
	pub cache_read: f64,
	#[serde(default, rename = "cacheWrite")]
	pub cache_write: f64,
	#[serde(default)]
	pub total: f64,
}

/// TS `Usage` (`packages/catalog/src/types.ts`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Usage {
	#[serde(default)]
	pub input: u64,
	#[serde(default)]
	pub output: u64,
	#[serde(default, rename = "cacheRead")]
	pub cache_read: u64,
	#[serde(default, rename = "cacheWrite")]
	pub cache_write: u64,
	#[serde(default, rename = "totalTokens")]
	pub total_tokens: u64,
	#[serde(default, rename = "contextTokens", skip_serializing_if = "Option::is_none")]
	pub context_tokens: Option<u64>,
	#[serde(default, rename = "reasoningTokens", skip_serializing_if = "Option::is_none")]
	pub reasoning_tokens: Option<u64>,
	#[serde(default)]
	pub cost: Cost,
	/// Unknown forward fields (`cttl`, `server`, `orchestration`, ...) preserved verbatim.
	#[serde(flatten)]
	pub extra: serde_json::Map<String, serde_json::Value>,
}

/// One typed content part: TS `TextContent` | `ImageContent`, plus lossless
/// passthrough. `toolCall` blocks keep `id`/`name`/`arguments` here — never
/// flattened into an OpenAI-only `tool_calls` shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContentPart {
	#[serde(rename = "type")]
	pub kind: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub text: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub data: Option<String>,
	#[serde(default, rename = "mimeType", skip_serializing_if = "Option::is_none")]
	pub mime_type: Option<String>,
	#[serde(flatten)]
	pub extra: serde_json::Map<String, serde_json::Value>,
}

impl ContentPart {
	#[must_use]
	pub fn text(text: impl Into<String>) -> Self {
		Self {
			kind: "text".to_string(),
			text: Some(text.into()),
			data: None,
			mime_type: None,
			extra: serde_json::Map::new(),
		}
	}

	#[must_use]
	pub fn image(data: impl Into<String>, mime_type: impl Into<String>) -> Self {
		let data = data.into();
		let mime = mime_type.into();
		let (clean_data, norm_mime) = match pi_agent::state::normalize_image_payload(&data, &mime) {
			Ok((clean_b64, m)) => (clean_b64, m),
			Err(_) => (data, mime),
		};
		Self {
			kind: "image".to_string(),
			text: None,
			data: Some(clean_data),
			mime_type: Some(norm_mime),
			extra: serde_json::Map::new(),
		}
	}

	pub fn try_image(data: impl Into<String>, mime_type: impl Into<String>) -> Result<Self, String> {
		let data = data.into();
		let mime = mime_type.into();
		let (clean_b64, norm_mime) = pi_agent::state::normalize_image_payload(&data, &mime)?;
		Ok(Self {
			kind: "image".to_string(),
			text: None,
			data: Some(clean_b64),
			mime_type: Some(norm_mime),
			extra: serde_json::Map::new(),
		})
	}

	#[must_use]
	pub fn is_image(&self) -> bool {
		self.kind == "image"
	}
}

/// Message content: TS `string | (TextContent | ImageContent)[]` form.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
	Text(String),
	Parts(Vec<ContentPart>),
	/// Forward-compatible passthrough for unknown content shapes.
	Raw(serde_json::Value),
}

impl MessageContent {
	#[must_use]
	pub fn text(&self) -> String {
		match self {
			Self::Text(text) => text.clone(),
			Self::Parts(parts) => parts
				.iter()
				.filter_map(|part| part.text.as_deref())
				.collect::<String>(),
			Self::Raw(value) => value
				.as_array()
				.map(|items| {
					items
						.iter()
						.filter_map(|item| item.get("text").and_then(serde_json::Value::as_str))
						.collect::<String>()
				})
				.unwrap_or_else(|| value.as_str().unwrap_or_default().to_string()),
		}
	}

	#[must_use]
	pub fn parts(&self) -> Vec<ContentPart> {
		match self {
			Self::Text(text) => vec![ContentPart::text(text.clone())],
			Self::Parts(parts) => parts.clone(),
			Self::Raw(_) => Vec::new(),
		}
	}

	#[must_use]
	pub fn has_image(&self) -> bool {
		match self {
			Self::Raw(value) => value.to_string().contains("\"image\""),
			_ => self.parts().iter().any(ContentPart::is_image),
		}
	}
}

/// Typed TS message (`packages/ai/src/types.ts` `Message` union): known fields
/// are typed, unknown/optional fields are preserved verbatim in `extra`.
/// `usage: None` means the provider reported none (explicitly unavailable) —
/// never zero-filled.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionMessage {
	pub role: String,
	pub content: MessageContent,
	#[serde(default, rename = "toolCallId", skip_serializing_if = "Option::is_none")]
	pub tool_call_id: Option<String>,
	#[serde(default, rename = "toolName", skip_serializing_if = "Option::is_none")]
	pub tool_name: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub api: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub provider: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub model: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub usage: Option<Usage>,
	#[serde(default, rename = "stopReason", skip_serializing_if = "Option::is_none")]
	pub stop_reason: Option<String>,
	#[serde(default, rename = "errorMessage", skip_serializing_if = "Option::is_none")]
	pub error_message: Option<String>,
	#[serde(default, rename = "timestamp", skip_serializing_if = "Option::is_none")]
	pub timestamp_ms: Option<u64>,
	#[serde(default, rename = "isError", skip_serializing_if = "Option::is_none")]
	pub is_error: Option<bool>,
	/// `details`, `attribution`, `responseId`, `providerPayload`, ... preserved.
	#[serde(flatten)]
	pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Canonicalizes stop reasons to match the TypeScript v3 schema (`StopReason`: "stop" | "toolUse" | "aborted" | "error" | "length").
#[must_use]
pub fn canonical_stop_reason(reason: &str) -> &str {
	match reason {
		"end_turn" | "stop" => "stop",
		"tool_use" | "toolUse" => "toolUse",
		"aborted" => "aborted",
		"error" => "error",
		"length" | "max_tokens" => "length",
		other => other,
	}
}

impl SessionMessage {
	#[must_use]
	pub fn user(content: MessageContent, timestamp_ms: u64) -> Self {
		Self {
			role: "user".to_string(),
			content,
			tool_call_id: None,
			tool_name: None,
			api: None,
			provider: None,
			model: None,
			usage: None,
			stop_reason: None,
			error_message: None,
			timestamp_ms: Some(timestamp_ms),
			is_error: None,
			extra: serde_json::Map::new(),
		}
	}

	/// Normalize optional metadata: empty strings are absence, never a value.
	fn opt_meta(value: impl Into<String>) -> Option<String> {
		let value = value.into();
		(!value.is_empty()).then_some(value)
	}

	#[must_use]
	pub fn assistant(
		content: MessageContent,
		api: impl Into<String>,
		provider: impl Into<String>,
		model: impl Into<String>,
		usage: Option<Usage>,
		stop_reason: impl Into<String>,
		timestamp_ms: u64,
	) -> Self {
		let stop = stop_reason.into();
		let canon_stop = if stop.is_empty() {
			None
		} else {
			Some(canonical_stop_reason(&stop).to_string())
		};
		Self {
			role: "assistant".to_string(),
			content,
			tool_call_id: None,
			tool_name: None,
			api: Self::opt_meta(api),
			provider: Self::opt_meta(provider),
			model: Self::opt_meta(model),
			usage,
			stop_reason: canon_stop,
			error_message: None,
			timestamp_ms: Some(timestamp_ms),
			is_error: None,
			extra: serde_json::Map::new(),
		}
	}

	#[must_use]
	pub fn tool_result(
		tool_call_id: impl Into<String>,
		tool_name: impl Into<String>,
		content: MessageContent,
		is_error: bool,
		timestamp_ms: u64,
	) -> Self {
		Self {
			role: "toolResult".to_string(),
			content,
			tool_call_id: Some(tool_call_id.into()),
			tool_name: Some(tool_name.into()),
			api: None,
			provider: None,
			model: None,
			usage: None,
			stop_reason: None,
			error_message: None,
			timestamp_ms: Some(timestamp_ms),
			is_error: Some(is_error),
			extra: serde_json::Map::new(),
		}
	}

	/// Reject writes that would need invented metadata. `usage` is optional:
	/// absent means the provider reported none (explicitly unavailable).
	pub fn validate(&self) -> Result<(), String> {
		if self.role == "assistant" {
			let mut missing = Vec::new();
			if self.api.is_none() {
				missing.push("api");
			}
			if self.provider.is_none() {
				missing.push("provider");
			}
			if self.model.is_none() {
				missing.push("model");
			}
			if self.stop_reason.is_none() {
				missing.push("stopReason");
			}
			if !missing.is_empty() {
				return Err(format!(
					"cannot persist assistant message: metadata {} unavailable — storage never \
					 invents provider metadata",
					missing.join("/")
				));
			}
		}
		self.validate_compat()
	}

	/// Compat-path validation: only fields whose values are real data must be
	/// present. Route metadata (`api`/`provider`/`model`/`usage`) is omitted
	/// when the source (e.g. a `TurnRecord` or a legacy v1 log) never carried
	/// it — never fabricated.
	pub fn validate_compat(&self) -> Result<(), String> {
		let ts_missing = self.timestamp_ms.is_none();
		match self.role.as_str() {
			"user" => {
				if ts_missing {
					return Err("cannot persist user message: timestamp unavailable".to_string());
				}
				if let MessageContent::Parts(parts) = &self.content {
					for (index, p) in parts.iter().enumerate() {
						if p.is_image() {
							if p.data.is_none() {
								return Err(format!("cannot persist message: image part missing data at index {index}"));
							}
							if p.mime_type.is_none() {
								return Err(format!("cannot persist message: image part missing mimeType at index {index}"));
							}
						} else if p.kind == "text" {
							if p.text.is_none() {
								return Err(format!("cannot persist message: text part missing text at index {index}"));
							}
						} else {
							return Err(format!("cannot persist message: unsupported content part kind at index {index}"));
						}
					}
				}
			},
			"assistant" => {
				if ts_missing {
					return Err("cannot persist assistant message: timestamp unavailable".to_string());
				}
			},
			"toolResult" => {
				let mut missing = Vec::new();
				if self.tool_call_id.is_none() {
					missing.push("toolCallId");
				}
				if self.tool_name.is_none() {
					missing.push("toolName");
				}
				if self.is_error.is_none() {
					missing.push("isError");
				}
				if ts_missing {
					missing.push("timestamp");
				}
				if !missing.is_empty() {
					return Err(format!(
						"cannot persist toolResult message: metadata {} unavailable",
						missing.join("/")
					));
				}
				if let MessageContent::Parts(parts) = &self.content {
					for (index, p) in parts.iter().enumerate() {
						if p.is_image() {
							if p.data.is_none() {
								return Err(format!("cannot persist message: image part missing data at index {index}"));
							}
							if p.mime_type.is_none() {
								return Err(format!("cannot persist message: image part missing mimeType at index {index}"));
							}
						} else if p.kind == "text" {
							if p.text.is_none() {
								return Err(format!("cannot persist message: text part missing text at index {index}"));
							}
						} else {
							return Err(format!("cannot persist message: unsupported content part kind at index {index}"));
						}
					}
				}
			},
			other => {
				if ts_missing {
					return Err(format!(
						"cannot persist `{other}` message: timestamp unavailable"
					));
				}
			},
		}
		Ok(())
	}
}

/// Typed TS `SessionMessageEntry` (`session-entries.ts`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageEntry {
	#[serde(rename = "type")]
	pub kind: String,
	#[serde(default)]
	pub id: String,
	#[serde(default, rename = "parentId")]
	pub parent_id: Option<String>,
	#[serde(default)]
	pub timestamp: String,
	pub message: SessionMessage,
	#[serde(flatten)]
	pub extra: serde_json::Map<String, serde_json::Value>,
}

impl MessageEntry {
	#[must_use]
	pub fn new(
		id: impl Into<String>,
		parent_id: Option<String>,
		timestamp: impl Into<String>,
		message: SessionMessage,
	) -> Self {
		Self {
			kind: "message".to_string(),
			id: id.into(),
			parent_id,
			timestamp: timestamp.into(),
			message,
			extra: serde_json::Map::new(),
		}
	}

	/// Entry-level validity required before any write.
	pub fn validate(&self) -> Result<(), String> {
		if self.id.is_empty() {
			return Err("cannot persist message entry: id unavailable".to_string());
		}
		if self.timestamp.is_empty() {
			return Err("cannot persist message entry: entry timestamp unavailable".to_string());
		}
		self.message.validate()
	}

	/// Compat-path entry validity: entry identity plus real-data message fields.
	pub fn validate_compat(&self) -> Result<(), String> {
		if self.id.is_empty() {
			return Err("cannot persist message entry: id unavailable".to_string());
		}
		if self.timestamp.is_empty() {
			return Err("cannot persist message entry: entry timestamp unavailable".to_string());
		}
		self.message.validate_compat()
	}
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallRecord {
	pub id: String,
	pub name: String,
	pub arguments: serde_json::Value,
	pub result: String,
	pub is_error: bool,
	#[serde(
		default,
		deserialize_with = "deserialize_turn_parts",
		skip_serializing_if = "Vec::is_empty"
	)]
	pub result_parts: Vec<ContentPart>,
	/// Structured result payload (TS `details`), preserved across resume/fork so
	/// tools with session state (e.g. `todo` phases) rehydrate losslessly.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub details: Option<serde_json::Value>,
}

fn deserialize_turn_parts<'de, D>(deserializer: D) -> Result<Vec<ContentPart>, D::Error>
where
	D: serde::Deserializer<'de>,
{
	let value = serde_json::Value::deserialize(deserializer)?;
	if value.is_null() {
		return Ok(Vec::new());
	}
	if let Some(arr) = value.as_array() {
		if arr.is_empty() {
			return Ok(Vec::new());
		}
		let mut parts = Vec::new();
		for (index, item) in arr.iter().enumerate() {
			if item.get("type").is_some() {
				if let Ok(part) = serde_json::from_value::<ContentPart>(item.clone()) {
					if part.is_image() {
						if part.data.is_none() || part.mime_type.is_none() {
							return Err(serde::de::Error::custom(format!(
								"malformed image part in turn record at index {index}: missing data or mimeType"
							)));
						}
					} else if part.kind != "text" {
						return Err(serde::de::Error::custom(format!(
							"unsupported content part kind in turn record at index {index}"
						)));
					} else if part.text.is_none() {
						return Err(serde::de::Error::custom(format!(
							"malformed text part in turn record at index {index}: missing text"
						)));
					}
					parts.push(part);
				} else {
					return Err(serde::de::Error::custom(format!(
						"unsupported or malformed content part in turn record at index {index}"
					)));
				}
			} else if let Ok(img) = serde_json::from_value::<pi_agent::ImagePart>(item.clone()) {
				parts.push(ContentPart::image(img.data_base64, img.mime_type));
			} else {
				return Err(serde::de::Error::custom(format!(
					"unsupported or malformed content part in turn record at index {index}"
				)));
			}
		}
		return Ok(parts);
	}
	Err(serde::de::Error::custom(
		"expected array of content parts"
	))
}
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TurnRecord {
	pub prompt: String,
	#[serde(default)]
	pub assistant: String,
	#[serde(default)]
	pub stop_reason: String,
	#[serde(default)]
	pub tool_calls: Vec<ToolCallRecord>,
	#[serde(
		default,
		deserialize_with = "deserialize_turn_parts",
		alias = "images",
		alias = "image_parts",
		skip_serializing_if = "Vec::is_empty"
	)]
	pub parts: Vec<ContentPart>,
}

impl TurnRecord {
	pub fn images(&self) -> Vec<pi_agent::ImagePart> {
		self.parts
			.iter()
			.filter(|p| p.is_image())
			.filter_map(|p| {
				let data = p.data.as_deref()?;
				let mime = p.mime_type.as_deref()?;
				Some(pi_agent::ImagePart::new(mime, data))
			})
			.collect()
	}
}

/// Builds a [`TurnRecord`] from the runner's event stream.
#[derive(Debug, Default)]
pub struct TurnBuilder {
	record: TurnRecord,
	pending: BTreeMap<String, usize>,
}

impl TurnBuilder {
	#[must_use]
	pub fn new(prompt: impl Into<String>) -> Self {
		Self {
			record: TurnRecord {
				prompt: prompt.into(),
				assistant: String::new(),
				stop_reason: String::new(),
				tool_calls: Vec::new(),
				parts: Vec::new(),
			},
			pending: BTreeMap::new(),
		}
	}

	#[must_use]
	pub fn new_with_parts(prompt: impl Into<String>, parts: Vec<ContentPart>) -> Self {
		Self {
			record: TurnRecord {
				prompt: prompt.into(),
				assistant: String::new(),
				stop_reason: String::new(),
				tool_calls: Vec::new(),
				parts,
			},
			pending: BTreeMap::new(),
		}
	}

	#[must_use]
	pub fn new_with_images(prompt: impl Into<String>, images: Vec<pi_agent::ImagePart>) -> Self {
		let prompt = prompt.into();
		let mut parts = Vec::new();
		if !prompt.is_empty() {
			parts.push(ContentPart::text(prompt.clone()));
		}
		for img in images {
			parts.push(ContentPart::image(img.data_base64, img.mime_type));
		}
		Self {
			record: TurnRecord {
				prompt,
				assistant: String::new(),
				stop_reason: String::new(),
				tool_calls: Vec::new(),
				parts,
			},
			pending: BTreeMap::new(),
		}
	}
	pub fn observe(&mut self, event: &Event) {
		match event {
			Event::MessageEnd { message, .. } => {
				self.record.assistant = message.content.clone();
			},
			Event::AgentEnd { messages, .. } => {
				if let Some(last_asst) = messages.iter().rfind(|m| m.role == pi_agent::Role::Assistant) {
					self.record.assistant = last_asst.content.clone();
				}
			},
			Event::TurnEnd { stop_reason, tool_results, .. } => {
				self.record.stop_reason = canonical_stop_reason(stop_reason).to_string();
				// Structured tool details ride on TurnEnd, never on the provider
				// wire; keep them on the recorded call for session persistence.
				for result in tool_results {
					if let Some(call) = self
						.record
						.tool_calls
						.iter_mut()
						.find(|call| call.id == result.tool_call_id)
					{
						call.details = result.details.clone();
					}
				}
			},
			Event::ToolStart { id, tool, args } => {
				self
					.pending
					.insert(id.clone(), self.record.tool_calls.len());
				self.record.tool_calls.push(ToolCallRecord {
					id: id.clone(),
					name: tool.clone(),
					arguments: args.clone(),
					result: String::new(),
					is_error: false,
					result_parts: Vec::new(),
					details: None,
				});
			},
			Event::ToolEnd { id, result, error, .. } => {
				if let Some(index) = self.pending.remove(id)
					&& let Some(call) = self.record.tool_calls.get_mut(index)
				{
					call.result = result.clone();
					call.is_error = error.is_some();
				}
			},
			_ => {},
		}
	}

	#[must_use]
	pub fn finish(mut self) -> TurnRecord {
		// Step1 invariant: a tool call without `ToolEnd` was interrupted — it
		// keeps a paired error result (`pi_agent::INTERRUPTED_TOOL_RESULT`),
		// never a silent empty result.
		for index in self.pending.values() {
			if let Some(call) = self.record.tool_calls.get_mut(*index) {
				call.result = "[Tool execution interrupted by user]".to_string();
				call.is_error = true;
			}
		}
		self.record
	}
}

/// Where and how to persist a session.
#[derive(Debug, Clone)]
pub struct SessionOptions {
	pub enabled: bool,
	pub cwd: PathBuf,
	pub model: Option<String>,
	pub provider: Option<String>,
	pub dir: Option<PathBuf>,
	pub id: Option<String>,
	pub continue_latest: bool,
	/// Suppress automatic first-prompt titles for this invocation only.
	/// Runtime-only: never read back from or written to the session log.
	pub no_title: bool,
}

impl SessionOptions {
	#[must_use]
	pub fn default_dir() -> PathBuf {
		Self::default_sessions_root()
	}

	#[must_use]
	pub fn default_sessions_root() -> PathBuf {
		dirs::agent_subdir(SESSIONS_SUBDIR, dirs::AgentDirCategory::Data)
	}
}

/// Raw or structured session entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEntry {
	#[serde(rename = "type")]
	pub kind: String,
	#[serde(default)]
	pub id: String,
	#[serde(default, rename = "parentId")]
	pub parent_id: Option<String>,
	#[serde(default)]
	pub timestamp: String,
	#[serde(flatten)]
	pub extra: serde_json::Value,
}

impl SessionEntry {
	pub fn validate(&self) -> Result<(), String> {
		if self.id.is_empty() {
			return Err("cannot persist session entry: id unavailable".to_string());
		}
		if self.timestamp.is_empty() {
			return Err("cannot persist session entry: timestamp unavailable".to_string());
		}
		Ok(())
	}
}

/// Session log handle. Failures are collected as warnings so a broken log never
/// changes the answer or the exit code.
#[derive(Debug, Default)]
pub struct SessionStore {
	enabled: bool,
	id: String,
	file: Option<PathBuf>,
	header: Option<SessionHeader>,
	history: Vec<TurnRecord>,
	messages: Vec<MessageEntry>,
	entries: Vec<SessionEntry>,
	warnings: Vec<String>,
	is_v1: bool,
	title_slot: Option<TitleSlot>,
	corrupt_reason: Option<String>,
	/// Per-invocation auto-title suppression; not persisted.
	no_title: bool,
	/// Route stamp from [`SessionOptions`] — real configured values only.
	model: Option<String>,
	provider: Option<String>,
}

impl SessionStore {
	/// Open (or resume) a session. Never fails: IO problems disable persistence
	/// and surface through [`take_warnings`](Self::take_warnings).
	pub fn open(options: &SessionOptions) -> Self {
		if !options.enabled {
			return Self::default();
		}

		let sessions_root = options
			.dir
			.clone()
			.unwrap_or_else(SessionOptions::default_sessions_root);

		let mut store = Self {
			enabled: false,
			model: options.model.clone(),
			provider: options.provider.clone(),
			no_title: options.no_title,
			..Self::default()
		};

		let (dir, file_override) = match &options.dir {
			Some(dir) => (dir.clone(), None),
			None => {
				let computed = compute_default_session_dir(&options.cwd, Some(&sessions_root));
				(computed, None)
			},
		};

		if let Err(err) = fs::create_dir_all(&dir) {
			store.warn(format!("cannot create session directory {}: {err}", dir.display()));
			return store;
		}

		let mut chosen_file: Option<PathBuf> = file_override;

		if chosen_file.is_none() && options.continue_latest {
			match continue_recent(&options.cwd, Some(&dir)) {
				Ok(Some(path)) => {
					chosen_file = Some(path);
				},
				Ok(None) => {
					store.warn(format!("no previous session in {}; starting a new one", dir.display()));
				},
				Err(err) => store.warn(err),
			}
		}

		if chosen_file.is_none() {
			if let Some(req_id) = &options.id {
				if !valid_session_id(req_id) {
					store.warn(format!(
						"invalid session id `{req_id}`: allowed characters are [A-Za-z0-9._-] and the id may \
						 not be `.`/`..`; session persistence disabled"
					));
					return store;
				}
				// Look for matching file in dir
				if let Ok(matched) = find_session_by_id_or_stem(&dir, req_id) {
					chosen_file = matched;
				}
			}
		}

		if let Some(file) = &chosen_file
			&& file.is_file()
		{
			let content = match fs::read_to_string(file) {
				Ok(c) => c,
				Err(err) => {
					let reason = format!("cannot read session log {}: {err}", file.display());
					store.file = Some(file.clone());
					store.corrupt_reason = Some(reason.clone());
					store.warn(format!("{reason}; session persistence disabled"));
					return store;
				},
			};

			match parse_session_content(file, &content) {
				Ok(ParsedSession { header, turns, messages, entries, is_v1, title_slot }) => {
					store.id = header.id.clone();
					store.header = Some(header);
					store.history = turns;
					store.messages = messages;
					store.entries = entries;
					store.file = Some(file.clone());
					store.is_v1 = is_v1;
					store.title_slot = title_slot;
					store.enabled = true;
					return store;
				},
				Err(err) => {
					store.file = Some(file.clone());
					store.corrupt_reason = Some(err.clone());
					store.warn(format!("{err}; session persistence disabled"));
					return store;
				},
			}
		}

		// New session
		let id = options.id.clone().unwrap_or_else(output::session_id);
		if !valid_session_id(&id) {
			store.warn(format!(
				"invalid session id `{id}`: allowed characters are [A-Za-z0-9._-] and the id may \
				 not be `.`/`..`; session persistence disabled"
			));
			return store;
		}

		let now_iso = output::iso8601(SystemTime::now());
		let ts = file_safe_timestamp(&now_iso);
		let file = dir.join(format!("{ts}_{id}.{LOG_EXTENSION}"));

		let title_slot_str = serialize_title_slot("", Some("auto"), &now_iso);
		let header = SessionHeader {
			kind: "session".to_string(),
			version: CURRENT_SESSION_VERSION,
			id: id.clone(),
			timestamp: now_iso,
			cwd: options.cwd.display().to_string(),
			model: options.model.clone(),
			provider: options.provider.clone(),
			title: None,
			title_source: Some("auto".to_string()),
			additional_directories: Vec::new(),
			parent_session: None,
			previous_session_files: Vec::new(),
			provider_prompt_cache_key: None,
		};

		let header_line = match serde_json::to_string(&header) {
			Ok(h) => format!("{h}\n"),
			Err(err) => {
				store.warn(format!("cannot serialize session header: {err}; session persistence disabled"));
				return store;
			},
		};

		let mut init_bytes = Vec::with_capacity(SESSION_TITLE_SLOT_BYTES + header_line.len());
		init_bytes.extend_from_slice(title_slot_str.as_bytes());
		init_bytes.extend_from_slice(header_line.as_bytes());

		let create_res = create_session_log_exclusive(&file, &init_bytes);

		if let Err(err) = create_res {
			let reason = if err.kind() == std::io::ErrorKind::AlreadyExists {
				format!(
					"session log {} already exists; refusing to overwrite existing file or symlink target",
					file.display()
				)
			} else {
				format!("cannot initialize session log {}: {err}", file.display())
			};
			store.file = Some(file.clone());
			store.corrupt_reason = Some(reason.clone());
			store.warn(format!("{reason}; session persistence disabled"));
			return store;
		}

		let _ = write_terminal_breadcrumb(&options.cwd, &file, true, None);

		store.id = id;
		store.header = Some(header);
		store.file = Some(file);
		store.title_slot = parse_title_slot(&title_slot_str);
		store.is_v1 = false;
		store.enabled = true;
		store
	}

	#[must_use]
	pub fn enabled(&self) -> bool {
		self.enabled
	}

	#[must_use]
	pub fn id(&self) -> &str {
		&self.id
	}

	#[must_use]
	pub fn file(&self) -> Option<&Path> {
		self.file.as_deref()
	}

	#[must_use]
	pub fn header(&self) -> Option<&SessionHeader> {
		self.header.as_ref()
	}
	#[must_use]
	pub fn title_slot(&self) -> Option<&TitleSlot> {
		self.title_slot.as_ref()
	}

	/// Latest todo phases recorded on this session branch, scanning newest
	/// entry first (TS `getLatestTodoPhasesFromEntries`). A `user_todo_edit`
	/// custom entry wins by log order — including `{phases: []}` clears — and a
	/// successful `todo` tool result contributes its `details.phases`; error
	/// results are skipped because their details echo the pre-call state.
	#[must_use]
	pub fn latest_todo_phases(&self) -> Vec<TodoPhase> {
		for entry in self.entries.iter().rev() {
			match entry.kind.as_str() {
				"custom" => {
					if entry.extra.get("customType").and_then(serde_json::Value::as_str)
						!= Some(USER_TODO_EDIT_CUSTOM_TYPE)
					{
						continue;
					}
					let Some(phases) = entry.extra.get("data").and_then(|data| data.get("phases")) else {
						continue;
					};
					if !phases.is_array() {
						continue;
					}
					if let Ok(phases) = serde_json::from_value::<Vec<TodoPhase>>(phases.clone()) {
						return phases;
					}
				},
				"message" => {
					let Some(message) = entry.extra.get("message") else {
						continue;
					};
					if message.get("role").and_then(serde_json::Value::as_str) != Some("toolResult")
						|| message.get("toolName").and_then(serde_json::Value::as_str) != Some("todo")
						|| message
							.get("isError")
							.and_then(serde_json::Value::as_bool)
							.unwrap_or(false)
					{
						continue;
					}
					let Some(phases) = message
						.get("details")
						.and_then(|details| details.get("phases"))
					else {
						continue;
					};
					if !phases.is_array() {
						continue;
					}
					if let Ok(phases) = serde_json::from_value::<Vec<TodoPhase>>(phases.clone()) {
						return phases;
					}
				},
				_ => {},
			}
		}
		Vec::new()
	}

	#[must_use]
	pub fn history(&self) -> &[TurnRecord] {
		&self.history
	}

	#[must_use]
	pub fn is_v1(&self) -> bool {
		self.is_v1
	}

	pub fn entries(&self) -> &[SessionEntry] {
		&self.entries
	}

	/// Typed message entries (TS `SessionMessageEntry`).
	#[must_use]
	pub fn messages(&self) -> &[MessageEntry] {
		&self.messages
	}

	#[must_use]
	pub fn has_image_content(&self) -> bool {
		self.messages.iter().any(|entry| entry.message.content.has_image())
	}

	/// Active leaf: the last journal entry. Branching appends a
	/// `branch_summary` whose `parentId` is the branch point, so the journal
	/// order alone reconstructs the active path after reload (TS
	/// `SessionManager.#leaf` + last-physical-entry rule).
	#[must_use]
	pub fn leaf_id(&self) -> Option<&str> {
		self
			.entries
			.last()
			.map(|entry| entry.id.as_str())
			.filter(|id| !id.is_empty())
	}

	#[must_use]
	pub fn has_entry(&self, id: &str) -> bool {
		!id.is_empty() && self.entries.iter().any(|entry| entry.id == id)
	}

	/// Warning when the journal has no single coherent `parentId` tree
	/// (legacy/foreign imports emit repeated roots or dangling parents). Such
	/// logs fall back to journal order so a resumed conversation is never
	/// silently truncated.
	#[must_use]
	pub fn branch_chain_warning(&self) -> Option<String> {
		let by_id: HashMap<&str, &SessionEntry> = self
			.entries
			.iter()
			.filter(|entry| !entry.id.is_empty())
			.map(|entry| (entry.id.as_str(), entry))
			.collect();
		let dangling = self.entries.iter().any(|entry| {
			entry
				.parent_id
				.as_deref()
				.filter(|parent| !parent.is_empty())
				.is_some_and(|parent| !by_id.contains_key(parent))
		});
		let mut roots = self.entries.iter().filter(|entry| {
			!entry.id.is_empty()
				&& entry
					.parent_id
					.as_deref()
					.filter(|parent| !parent.is_empty())
					.is_none()
		});
		let _first_root = roots.next();
		// Later parentless entries are valid only for a rewind-to-root summary.
		let unexpected_root = roots.any(|entry| {
			entry.kind != "branch_summary"
				|| entry.extra.get("fromId").and_then(serde_json::Value::as_str) != Some("root")
		});
		if dangling || unexpected_root {
			Some(
				"session branch chain is incomplete (imported/legacy entries); using journal order"
					.to_string(),
			)
		} else {
			None
		}
	}

	/// Root-to-leaf path of the active branch, following `parentId` links.
	/// Missing or cyclic parents terminate the walk, never panic. Incoherent
	/// chains fall back to journal order (see [`Self::branch_chain_warning`]).
	#[must_use]
	pub fn branch_entries(&self) -> Vec<&SessionEntry> {
		if self.branch_chain_warning().is_some() {
			return self
				.entries
				.iter()
				.filter(|entry| !entry.id.is_empty())
				.collect();
		}
		let mut by_id: HashMap<&str, &SessionEntry> = HashMap::with_capacity(self.entries.len());
		for entry in &self.entries {
			if !entry.id.is_empty() {
				by_id.insert(entry.id.as_str(), entry);
			}
		}
		let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
		let mut path = Vec::new();
		let mut cursor = self.entries.last();
		while let Some(entry) = cursor {
			if entry.id.is_empty() || !seen.insert(entry.id.as_str()) {
				break;
			}
			path.push(entry);
			cursor = entry
				.parent_id
				.as_deref()
				.filter(|parent| !parent.is_empty())
				.and_then(|parent| by_id.get(parent).copied());
		}
		path.reverse();
		path
	}

	/// Whether the active branch needs the exact context override: rewinds,
	/// custom entries, or abandoned messages make `history()` lossy.
	#[must_use]
	pub fn has_branch_context(&self) -> bool {
		self.branch_chain_warning().is_some()
			|| self.messages.len() != self.branch_messages().len()
			|| self
				.entries
				.iter()
				.any(|entry| entry.kind == "branch_summary" || entry.kind == "custom_message")
	}

	/// Typed messages on the active branch, in branch order. Abandoned entries
	/// stay in [`Self::messages`] but are excluded here (they are not ancestors
	/// of the leaf).
	#[must_use]
	pub fn branch_messages(&self) -> Vec<&MessageEntry> {
		let ids: std::collections::HashSet<&str> = self
			.branch_entries()
			.into_iter()
			.map(|entry| entry.id.as_str())
			.collect();
		// A branch cut may land after the checkpoint result while the same
		// assistant message also called a sibling tool. Production appends
		// each later toolResult under the previous result, so parentage cannot
		// identify the batch; tool-call ids are unique, so match by those.
		let mut batch_call_ids: std::collections::HashSet<&str> = std::collections::HashSet::new();
		for entry in self
			.messages
			.iter()
			.filter(|entry| ids.contains(entry.id.as_str()) && entry.message.role == "assistant")
		{
			batch_call_ids.extend(
				entry
					.message
					.content
					.parts()
					.iter()
					.filter(|part| part.kind == "toolCall")
					.filter_map(|part| {
						part.extra
							.get("id")
							.and_then(serde_json::Value::as_str)
					}),
			);
		}
		self
			.messages
			.iter()
			.filter(|entry| {
				ids.contains(entry.id.as_str())
					|| (entry.message.role == "toolResult"
						&& entry
							.message
							.tool_call_id
							.as_deref()
							.is_some_and(|id| batch_call_ids.contains(id)))
			})
			.collect()
	}

	/// `TurnRecord` view of the active branch (TS `buildSessionContext` message
	/// rebuild). Abandoned entries never contribute.
	pub fn branch_history(&self) -> Result<Vec<TurnRecord>, String> {
		let messages: Vec<MessageEntry> = self
			.branch_messages()
			.into_iter()
			.cloned()
			.collect();
		derive_turn_history(&messages)
	}

	/// TS `SessionManager.branchWithSummary`: append a `branch_summary` whose
	/// parent is `branch_from`, moving the leaf there without modifying or
	/// deleting abandoned entries. `branch_from == None` starts a new root.
	pub fn branch_with_summary(
		&mut self,
		branch_from: Option<&str>,
		summary: &str,
		details: serde_json::Value,
	) -> Result<String, String> {
		if let Some(id) = branch_from
			&& !self.has_entry(id)
		{
			return Err(format!("Entry {id} not found"));
		}
		let id = format!("entry-{}", output::session_id());
		let mut extra = serde_json::json!({
			"fromId": branch_from.unwrap_or("root"),
			"summary": summary,
		});
		if !details.is_null()
			&& let Some(map) = extra.as_object_mut()
		{
			map.insert("details".to_string(), details);
		}
		self.append_entry(SessionEntry {
			kind: "branch_summary".to_string(),
			id: id.clone(),
			parent_id: branch_from.map(str::to_string),
			timestamp: output::iso8601(SystemTime::now()),
			extra,
		})?;
		Ok(id)
	}

	/// TS `SessionManager.appendCustomMessageEntry` for the checkpoint lane:
	/// persisted `custom_message` with `display` control and details.
	pub fn append_custom_message(
		&mut self,
		custom_type: &str,
		content: &str,
		display: bool,
		details: serde_json::Value,
	) -> Result<String, String> {
		let id = format!("entry-{}", output::session_id());
		let mut extra = serde_json::json!({
			"customType": custom_type,
			"content": content,
			"display": display,
			"attribution": "agent",
		});
		if !details.is_null()
			&& let Some(map) = extra.as_object_mut()
		{
			map.insert("details".to_string(), serde_json::Value::Object(details.clone()));
		}
		let parent_id = self.leaf_id().map(str::to_string);
		self.append_entry(SessionEntry {
			kind: "custom_message".to_string(),
			id: id.clone(),
			parent_id,
			timestamp: output::iso8601(SystemTime::now()),
			extra,
		})?;
		Ok(id)
	}

	#[must_use]
	pub fn corrupt_reason(&self) -> Option<&str> {
		self.corrupt_reason.as_deref()
	}

	/// Append one raw/custom entry (`custom`, `title_change`, ...) with
	/// rollback: a failed write truncates back to the original size and the
	/// in-memory state is untouched.
	pub fn append_entry(&mut self, entry: SessionEntry) -> Result<(), String> {
		if let Some(reason) = &self.corrupt_reason {
			return Err(format!("cannot append to corrupted session log: {reason}"));
		}
		if !self.enabled {
			return Ok(());
		}
		if self.is_v1 {
			return Err("cannot append to legacy v1 session log: read-only; use import_legacy_v1".to_string());
		}
		if let Some(header) = &self.header {
			if header.version != CURRENT_SESSION_VERSION {
				return Err(format!(
					"cannot append to session log: unsupported session version {}; expected version {CURRENT_SESSION_VERSION}",
					header.version
				));
			}
		}
		entry.validate()?;
		let Some(file) = self.file.clone() else {
			return Ok(());
		};
		let line = format!("{}\n", serde_json::to_string(&entry).map_err(|e| e.to_string())?);
		self.append_lines_rolled_back(&file, &[line])?;
		self.entries.push(entry);
		Ok(())
	}

	/// Append one typed TS message entry. Validates required metadata first —
	/// missing metadata is an explicit error, never an invented value.
	pub fn append_message(&mut self, entry: &MessageEntry) -> Result<(), String> {
		self.append_messages(std::slice::from_ref(entry))
	}

	/// Append typed TS message entries as one atomic batch: a single rollback
	/// window, and no in-memory/history mutation until the write succeeds.
	pub fn append_messages(&mut self, entries: &[MessageEntry]) -> Result<(), String> {
		self.append_messages_inner(entries, true)
	}

	/// `strict` requires full route metadata (live typed path); the compat path
	/// validates only real-data fields and omits what the source never carried.
	fn append_messages_inner(
		&mut self,
		entries: &[MessageEntry],
		strict: bool,
	) -> Result<(), String> {
		if let Some(reason) = &self.corrupt_reason {
			return Err(format!("cannot append to corrupted session log: {reason}"));
		}
		if !self.enabled {
			return Ok(());
		}
		if self.is_v1 {
			return Err("cannot append to legacy v1 session log: read-only; use import_legacy_v1".to_string());
		}
		if let Some(header) = &self.header {
			if header.version != CURRENT_SESSION_VERSION {
				return Err(format!(
					"cannot append to session log: unsupported session version {}; expected version {CURRENT_SESSION_VERSION}",
					header.version
				));
			}
		}
		if entries.is_empty() {
			return Ok(());
		}
		for entry in entries {
			if strict {
				entry.validate()?;
			} else {
				entry.validate_compat()?;
			}
		}
		// Preflight candidate history derivation before ANY disk or memory mutation
		let candidate_history = {
			let mut candidate_messages = Vec::with_capacity(self.messages.len() + entries.len());
			candidate_messages.extend_from_slice(&self.messages);
			candidate_messages.extend(entries.iter().cloned());
			derive_turn_history(&candidate_messages)?
		};

		let lines: Vec<String> = entries
			.iter()
			.map(|entry| {
				serde_json::to_string(entry)
					.map(|line| format!("{line}\n"))
					.map_err(|err| format!("cannot serialize message entry: {err}"))
			})
			.collect::<Result<_, _>>()?;
		// Automatic first-prompt title: one gate shared by the live
		// `append_message` path and the compat `append_turn` path. Only a real
		// user text message qualifies; tool/system/assistant batches never do.
		let now_iso = output::iso8601(SystemTime::now());
		let title_prompt = self.auto_title_text(entries);
		let title_slot = title_prompt
			.as_ref()
			.map(|prompt| serialize_title_slot(prompt, Some("auto"), &now_iso));
		let orig_leading_bytes = title_slot
			.as_ref()
			.and_then(|_| self.file.as_deref())
			.and_then(read_title_slot_bytes);
		if let (Some(file), Some(slot)) = (self.file.as_deref(), title_slot.as_deref())
			&& let Err(err) = overwrite_title_slot_rolled_back(file, slot)
		{
			return Err(err);
		}
		let Some(file) = self.file.clone() else {
			self.apply_auto_title(title_prompt, title_slot);
			self.history = candidate_history;
			return Ok(());
		};
		if let Err(err) = self.append_lines_rolled_back(&file, &lines) {
			if let Some(bytes) = &orig_leading_bytes {
				restore_title_slot_bytes(&file, bytes);
			}
			return Err(err);
		}
		for (entry, line) in entries.iter().zip(&lines) {
			self.messages.push(entry.clone());
			if let Ok(raw) = serde_json::from_str::<SessionEntry>(line) {
				self.entries.push(raw);
			}
		}
		self.apply_auto_title(title_prompt, title_slot);
		self.history = candidate_history;
		Ok(())
	}

	/// The text of the first real user message in `entries`, when automatic
	/// first-prompt titling is allowed for this invocation. `None` while
	/// disabled (`--no-title`/`PI_NO_TITLE`), once a title exists, or for
	/// assistant/tool batches.
	fn auto_title_text(&self, entries: &[MessageEntry]) -> Option<String> {
		// The in-place write targets the leading 256-byte slot. Slotless v3
		// logs keep the header on line 1, so writing there would destroy it:
		// never auto-migrate old formats; require an existing slot.
		if self.no_title
			|| self.title_slot.is_none()
			|| !self.header.as_ref().is_some_and(|header| header.title.is_none())
		{
			return None;
		}
		entries.iter().find_map(|entry| {
			let message = &entry.message;
			(message.role == "user")
				.then(|| message.content.text())
				.filter(|text| !text.is_empty())
		})
	}

	fn apply_auto_title(&mut self, prompt: Option<String>, slot: Option<String>) {
		let (Some(prompt), Some(slot)) = (prompt, slot) else {
			return;
		};
		if let Some(header) = &mut self.header {
			header.title = Some(prompt);
			header.title_source = Some("auto".to_string());
		}
		self.title_slot = parse_title_slot(&slot);
	}

	/// TS `session-storage.ts#writeNow`: write all lines, truncate back to the
	/// pre-write size when any write fails (disk full / short write).
	fn append_lines_rolled_back(&self, file: &Path, lines: &[String]) -> Result<(), String> {
		let mut handle = OpenOptions::new()
			.create(true)
			.append(true)
			.open(file)
			.map_err(|err| format!("cannot open session log {}: {err}", file.display()))?;
		let original_size = handle
			.metadata()
			.map(|metadata| metadata.len())
			.map_err(|err| format!("cannot stat session log {}: {err}", file.display()))?;
		for (_line_idx, line) in lines.iter().enumerate() {
			let write_res = (|| -> std::io::Result<()> {
				#[cfg(test)]
				if let Some(fault) = check_fault_injection() {
					match fault {
						FaultInjection::FailAppendAfterLines(n) => {
							if _line_idx >= n {
								return Err(std::io::Error::new(
									std::io::ErrorKind::Other,
									"simulated disk full / ENOSPC during append",
								));
							}
						},
						FaultInjection::FailAppendAfterBytes(n) => {
							let line_bytes = line.as_bytes();
							let partial = n.min(line_bytes.len());
							handle.write_all(&line_bytes[..partial])?;
							return Err(std::io::Error::new(
								std::io::ErrorKind::Other,
								"simulated short write during append",
							));
						},
						_ => {},
					}
				}
				handle.write_all(line.as_bytes())
			})();
			if let Err(err) = write_res {
				return Err(match handle.set_len(original_size) {
					Ok(()) => format!(
						"cannot write session log {}: {err}; partial append rolled back to \
						 {original_size} bytes",
						file.display()
					),
					Err(rollback_err) => format!(
						"cannot write session log {}: {err}; partial bytes could not be rolled back: \
						 {rollback_err}",
						file.display()
					),
				});
			}
		}
		let flush_res = (|| -> std::io::Result<()> {
			#[cfg(test)]
			if let Some(FaultInjection::FailAppendFlush) = check_fault_injection() {
				return Err(std::io::Error::new(
					std::io::ErrorKind::Other,
					"simulated flush error during append",
				));
			}
			handle.flush()
		})();
		if let Err(err) = flush_res {
			return Err(match handle.set_len(original_size) {
				Ok(()) => format!(
					"cannot flush session log {}: {err}; append rolled back to {original_size} bytes",
					file.display()
				),
				Err(rollback_err) => format!(
					"cannot flush session log {}: {err}; rollback to {original_size} bytes failed: {rollback_err}",
					file.display()
				),
			});
		}
		Ok(())
	}
	pub fn take_warnings(&mut self) -> Vec<String> {
		std::mem::take(&mut self.warnings)
	}

	/// Compatibility write path. `TurnRecord` is a read view without route
	/// metadata: `api`/`usage` are omitted (never invented), `provider`/`model`
	/// are stamped from the configured [`SessionOptions`] route when present,
	/// and tool-call arguments/results are the recorded real data. Use
	/// [`append_message`](Self::append_message) when full metadata is available.
	pub fn append_turn(&mut self, turn: &TurnRecord) -> Result<(), String> {
		if let Some(reason) = &self.corrupt_reason {
			return Err(format!("cannot append to corrupted session log: {reason}"));
		}
		if !self.enabled {
			return Ok(());
		}
		if self.is_v1 {
			return Err("cannot append to legacy v1 session log: read-only; use import_legacy_v1".to_string());
		}
		let now_time = SystemTime::now();
		let now_iso = output::iso8601(now_time);
		let now_ms = now_time
			.duration_since(std::time::UNIX_EPOCH)
			.map(|d| d.as_millis() as u64)
			.unwrap_or(0);

		let mut entries = Vec::new();
		let user_content = if !turn.parts.is_empty() {
			let mut normalized = Vec::with_capacity(turn.parts.len());
			for (index, part) in turn.parts.iter().enumerate() {
				if part.is_image() {
					let Some(data) = part.data.as_deref() else {
						return Err(format!("cannot append turn with image part missing data at index {index}"));
					};
					let Some(mime) = part.mime_type.as_deref() else {
						return Err(format!("cannot append turn with image part missing mimeType at index {index}"));
					};
					let (clean_b64, norm_mime) = pi_agent::state::normalize_image_payload(data, mime)
						.map_err(|err| format!("cannot append turn with invalid image part at index {index}: {err}"))?;
					let mut norm_part = part.clone();
					norm_part.data = Some(clean_b64);
					norm_part.mime_type = Some(norm_mime);
					normalized.push(norm_part);
				} else if part.kind == "text" {
					if part.text.is_none() {
						return Err(format!(
							"cannot append turn with text part missing text at index {index}"
						));
					}
					normalized.push(part.clone());
				} else {
					return Err(format!(
						"cannot append turn with unsupported content part kind at index {index}"
					));
				}
			}
			MessageContent::Parts(normalized)
		} else {
			MessageContent::Text(turn.prompt.clone())
		};
		let user_id = format!("msg-{}", output::session_id());
		entries.push(MessageEntry::new(
			user_id.clone(),
			self.messages.last().map(|last| last.id.clone()).filter(|id| !id.is_empty()),
			now_iso.clone(),
			SessionMessage::user(user_content, now_ms),
		));
		let mut parent_id = Some(user_id);

		if !turn.assistant.is_empty() || !turn.tool_calls.is_empty() {
			let mut parts = Vec::new();
			if !turn.assistant.is_empty() {
				parts.push(ContentPart::text(turn.assistant.clone()));
			}
			for tc in &turn.tool_calls {
				let mut part = ContentPart {
					kind: "toolCall".to_string(),
					text: None,
					data: None,
					mime_type: None,
					extra: serde_json::Map::new(),
				};
				part.extra.insert("id".to_string(), serde_json::json!(tc.id));
				part.extra.insert("name".to_string(), serde_json::json!(tc.name));
				part.extra.insert("arguments".to_string(), tc.arguments.clone());
				parts.push(part);
			}
			let asst_id = format!("msg-{}", output::session_id());
			entries.push(MessageEntry::new(
				asst_id.clone(),
				parent_id.take(),
				now_iso.clone(),
				SessionMessage {
					role: "assistant".to_string(),
					content: MessageContent::Parts(parts),
					tool_call_id: None,
					tool_name: None,
					// `api` and `usage` are unknown from a `TurnRecord`: absent,
					// never invented.
					api: None,
					provider: self.provider.clone(),
					model: self.model.clone(),
					usage: None,
					stop_reason: Some(if turn.stop_reason.is_empty() {
						if turn.tool_calls.is_empty() { "stop".to_string() } else { "toolUse".to_string() }
					} else {
						canonical_stop_reason(&turn.stop_reason).to_string()
					}),
					error_message: (canonical_stop_reason(&turn.stop_reason) == "aborted")
						.then(|| USER_INTERRUPT_LABEL.to_string()),
					timestamp_ms: Some(now_ms),
					is_error: None,
					extra: serde_json::Map::new(),
				},
			));
			parent_id = Some(asst_id);

			for tc in &turn.tool_calls {
				let res_id = format!("msg-{}", output::session_id());
				let content = if !tc.result_parts.is_empty() {
					let mut normalized_parts = Vec::with_capacity(tc.result_parts.len());
					for (index, part) in tc.result_parts.iter().enumerate() {
						if part.is_image() {
							let Some(data) = part.data.as_deref() else {
								return Err(format!(
									"tool result image part at index {index} missing base64 data"
								));
							};
							let Some(mime) = part.mime_type.as_deref() else {
								return Err(format!(
									"tool result image part at index {index} missing mimeType"
								));
							};
							let (clean_b64, norm_mime) = pi_agent::state::normalize_image_payload(data, mime)?;
							normalized_parts.push(ContentPart::image(clean_b64, norm_mime));
						} else if part.kind == "text" {
							let Some(text) = part.text.clone() else {
								return Err(format!(
									"tool result text part at index {index} missing text string"
								));
							};
							normalized_parts.push(ContentPart::text(text));
						} else {
							return Err(format!(
								"unsupported content part kind {:?} in tool result at index {index}",
								part.kind
							));
						}
					}
					MessageContent::Parts(normalized_parts)
				} else {
					MessageContent::Text(tc.result.clone())
				};
				let mut message = SessionMessage {
					timestamp_ms: Some(now_ms),
					..SessionMessage::tool_result(
						tc.id.clone(),
						tc.name.clone(),
						content,
						tc.is_error,
						now_ms,
					)
				};
				if let Some(details) = &tc.details {
					message.extra.insert("details".to_string(), details.clone());
				}
				entries.push(MessageEntry::new(
					res_id.clone(),
					parent_id.take(),
					now_iso.clone(),
					message,
				));
				parent_id = Some(res_id);
			}
		}

		// Snapshot disk and in-memory state for atomic rollback if any step fails.
		let orig_file_size = self
			.file
			.as_ref()
			.and_then(|f| fs::metadata(f).ok().map(|m| m.len()));
		let orig_leading_bytes = self.file.as_ref().and_then(|f| {
			use std::io::Read;
			let mut b = [0u8; SESSION_TITLE_SLOT_BYTES];
			let mut h = fs::File::open(f).ok()?;
			h.read_exact(&mut b).ok()?;
			Some(b)
		});
		let orig_messages_len = self.messages.len();
		let orig_entries_len = self.entries.len();
		let orig_history_len = self.history.len();
		let orig_header = self.header.clone();
		let orig_title_slot = self.title_slot.clone();

		let execute = (|| -> Result<(), String> {
			self.append_messages_inner(&entries, false)?;
			Ok(())
		})();

		if let Err(err) = execute {
			// Rollback disk: restore original file size (truncate appended entries)
			// and restore original leading 256 bytes (title slot).
			if let Some(file) = &self.file {
				if let Some(leading) = &orig_leading_bytes {
					use std::io::{Seek, SeekFrom, Write};
					if let Ok(mut h) = OpenOptions::new().write(true).open(file) {
						if h.seek(SeekFrom::Start(0)).is_ok() && h.write_all(leading).is_ok() {
							let _ = h.flush();
						}
					}
				}
				if let Some(orig_size) = orig_file_size {
					if let Ok(h) = OpenOptions::new().write(true).open(file) {
						let _ = h.set_len(orig_size);
					}
				}
			}
			// Rollback memory: restore exact in-memory state.
			self.messages.truncate(orig_messages_len);
			self.entries.truncate(orig_entries_len);
			self.history.truncate(orig_history_len);
			self.header = orig_header;
			self.title_slot = orig_title_slot;
			return Err(err);
		}

		Ok(())
	}
	fn warn(&mut self, message: String) {
		self.warnings.push(message);
	}
}
#[cfg(test)]
/// Test fault injection targets for storage rollback guarantees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultInjection {
	/// Normal operation.
	None,
	/// Fail append when writing line index >= n (earlier lines are written to disk first).
	FailAppendAfterLines(usize),
	/// Fail append mid-line after writing partial bytes.
	FailAppendAfterBytes(usize),
	/// Fail append flush after all lines written.
	FailAppendFlush,
	/// Fail title slot write after writing n bytes of the new slot.
	FailTitleSlotWriteAfterBytes(usize),
	/// Fail title slot flush after writing the full 256-byte slot.
	FailTitleSlotFlush,
}

#[cfg(test)]
thread_local! {
	static FAULT_INJECTION: std::cell::Cell<Option<FaultInjection>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub fn set_fault_injection(fault: Option<FaultInjection>) {
	FAULT_INJECTION.with(|cell| cell.set(fault));
}

#[cfg(test)]
fn check_fault_injection() -> Option<FaultInjection> {
	FAULT_INJECTION.with(|cell| cell.get())
}

fn read_title_slot_bytes(file: &Path) -> Option<[u8; SESSION_TITLE_SLOT_BYTES]> {
	use std::io::Read;
	let mut bytes = [0u8; SESSION_TITLE_SLOT_BYTES];
	let mut handle = fs::File::open(file).ok()?;
	handle.read_exact(&mut bytes).ok()?;
	Some(bytes)
}

fn restore_title_slot_bytes(file: &Path, bytes: &[u8; SESSION_TITLE_SLOT_BYTES]) {
	use std::io::{Seek, SeekFrom, Write};
	if let Ok(mut handle) = OpenOptions::new().write(true).open(file) {
		if handle.seek(SeekFrom::Start(0)).is_ok() && handle.write_all(bytes).is_ok() {
			let _ = handle.flush();
		}
	}
}

/// TS `session-storage.ts#updateSessionTitle`: overwrite the leading 256-byte
/// title slot in place. If the write or flush fails, the original 256 bytes
/// are restored to prevent bricking the session log for subsequent reopens.
pub fn overwrite_title_slot_rolled_back(file: &Path, slot: &str) -> Result<(), String> {
	use std::io::{Read, Seek, SeekFrom};
	let mut handle = OpenOptions::new()
		.read(true)
		.write(true)
		.open(file)
		.map_err(|err| format!("cannot open session log {} for title slot write: {err}", file.display()))?;

	// Snapshot exact 256 bytes for rollback.
	let mut original = [0u8; SESSION_TITLE_SLOT_BYTES];
	handle
		.read_exact(&mut original)
		.map_err(|err| format!("cannot read exact 256-byte title slot from {}: {err}", file.display()))?;

	handle
		.seek(SeekFrom::Start(0))
		.map_err(|err| format!("cannot seek session log {}: {err}", file.display()))?;

	let slot_bytes = slot.as_bytes();
	let write_and_flush = (|| -> std::io::Result<()> {
		#[cfg(test)]
		if let Some(fault) = check_fault_injection() {
			match fault {
				FaultInjection::FailTitleSlotWriteAfterBytes(n) => {
					let to_write = n.min(slot_bytes.len());
					handle.write_all(&slot_bytes[..to_write])?;
					return Err(std::io::Error::new(
						std::io::ErrorKind::Other,
						"simulated short write in title slot",
					));
				},
				FaultInjection::FailTitleSlotFlush => {
					handle.write_all(slot_bytes)?;
					return Err(std::io::Error::new(
						std::io::ErrorKind::Other,
						"simulated flush error in title slot",
					));
				},
				_ => {},
			}
		}
		handle.write_all(slot_bytes)?;
		handle.flush()?;
		Ok(())
	})();

	if let Err(err) = write_and_flush {
		// Restore original bytes on write or flush failure.
		let rollback_status = (|| -> std::io::Result<()> {
			handle.seek(SeekFrom::Start(0))?;
			handle.write_all(&original)?;
			handle.flush()?;
			Ok(())
		})();
		return Err(match rollback_status {
			Ok(()) => format!(
				"cannot write title slot to {}: {err}; original slot bytes restored",
				file.display()
			),
			Err(rb_err) => format!(
				"cannot write title slot to {}: {err}; rollback of original slot failed: {rb_err}",
				file.display()
			),
		});
	}
	Ok(())
}

/// Parse UTC `YYYY-MM-DDTHH:MM:SS[.mmm]Z` into epoch milliseconds.
fn parse_iso8601_ms(iso: &str) -> Option<u64> {
	let (date, time) = iso.split_once('T')?;
	let (year, month, day) = {
		let mut parts = date.split('-');
		let y = parts.next()?.parse::<i64>().ok()?;
		let m = parts.next()?.parse::<u32>().ok()?;
		let d = parts.next()?.parse::<u32>().ok()?;
		if parts.next().is_some() {
			return None;
		}
		(y, m, d)
	};
	let time = time.strip_suffix('Z')?;
	let (hms, frac) = match time.split_once('.') {
		Some((hms, frac)) => (hms, Some(frac)),
		None => (time, None),
	};
	let mut parts = hms.split(':');
	let hour: u64 = parts.next()?.parse().ok()?;
	let minute: u64 = parts.next()?.parse().ok()?;
	let second: u64 = parts.next()?.parse().ok()?;
	if parts.next().is_some() {
		return None;
	}
	let millis: u64 = match frac {
		Some(digits) if !digits.is_empty() && digits.len() <= 3 && digits.bytes().all(|b| b.is_ascii_digit()) => {
			format!("{digits}000")[..3].parse().ok()?
		},
		None => 0,
		_ => return None,
	};
	let is_leap = (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0);
	let max_day = match month {
		1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
		4 | 6 | 9 | 11 => 30,
		2 if is_leap => 29,
		2 => 28,
		_ => return None,
	};
	if day < 1 || day > max_day || hour >= 24 || minute >= 60 || second >= 60 {
		return None;
	}
	// Days from civil (Howard Hinnant).
	let year = if month <= 2 { year - 1 } else { year };
	let era = if year >= 0 { year } else { year - 399 } / 400;
	let year_of_era = (year - era * 400) as u64;
	let month_u = u64::from(month);
	let day_of_year = (153 * (if month_u > 2 { month_u - 3 } else { month_u + 9 }) + 2) / 5
		+ u64::from(day)
		- 1;
	let day_of_era =
		year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
	let days = era * 146_097 + day_of_era as i64 - 719_468;
	Some(
		(u64::try_from(days).ok()? * 86_400_000)
			+ hour * 3_600_000
			+ minute * 60_000
			+ second * 1000
			+ millis,
	)
}

pub fn file_safe_timestamp(iso: &str) -> String {
	iso.replace([':', '.'], "-")
}

pub fn is_valid_file_safe_timestamp(s: &str) -> bool {
	let Some((date, time)) = s.split_once('T') else {
		return false;
	};
	let date_parts: Vec<&str> = date.split('-').collect();
	let [y, m, d] = date_parts.as_slice() else {
		return false;
	};
	if y.len() != 4 || !y.bytes().all(|b| b.is_ascii_digit()) {
		return false;
	}
	if m.len() != 2 || !m.bytes().all(|b| b.is_ascii_digit()) {
		return false;
	}
	if d.len() != 2 || !d.bytes().all(|b| b.is_ascii_digit()) {
		return false;
	}
	if !matches!(m.parse::<u32>().ok(), Some(1..=12)) {
		return false;
	}
	if !matches!(d.parse::<u32>().ok(), Some(1..=31)) {
		return false;
	}

	let Some(time_no_z) = time.strip_suffix('Z').or_else(|| time.strip_suffix('z')) else {
		return false;
	};
	let time_parts: Vec<&str> = time_no_z.split('-').collect();
	let (h, min, sec, ms) = match time_parts.as_slice() {
		[h, min, sec] if h.len() == 2 && min.len() == 2 && sec.len() == 2 => (*h, *min, *sec, None),
		[h, min, sec, ms]
			if h.len() == 2 && min.len() == 2 && sec.len() == 2 && !ms.is_empty() && ms.len() <= 3 =>
		{
			(*h, *min, *sec, Some(*ms))
		},
		_ => return false,
	};

	if !h.bytes().all(|b| b.is_ascii_digit())
		|| !min.bytes().all(|b| b.is_ascii_digit())
		|| !sec.bytes().all(|b| b.is_ascii_digit())
	{
		return false;
	}
	if let Some(ms_str) = ms {
		if !ms_str.bytes().all(|b| b.is_ascii_digit()) {
			return false;
		}
	}

	let hour: u32 = match h.parse().ok() {
		Some(h) if h < 24 => h,
		_ => return false,
	};
	let minute: u32 = match min.parse().ok() {
		Some(m) if m < 60 => m,
		_ => return false,
	};
	let second: u32 = match sec.parse().ok() {
		Some(s) if s < 60 => s,
		_ => return false,
	};
	let _ = (hour, minute, second);

	let iso = match ms {
		Some(ms_val) => format!("{date}T{h}:{min}:{sec}.{ms_val}Z"),
		None => format!("{date}T{h}:{min}:{sec}Z"),
	};
	parse_iso8601_ms(&iso).is_some()
}
pub fn valid_session_id(id: &str) -> bool {
	!id.is_empty()
		&& id != "."
		&& id != ".."
		&& id
			.chars()
			.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

struct SessionDirPaths {
	current: PathBuf,
	legacy_absolute: PathBuf,
	legacy_home: Option<PathBuf>,
	root: PathBuf,
}

/// Current and pre-migration session directories for `cwd`.
///
/// Pure: computes paths only, never creates, renames, or removes anything.
fn session_dir_paths(cwd: &Path, sessions_root: Option<&Path>) -> SessionDirPaths {
	let raw_cwd = if cwd.is_absolute() {
		cwd.to_path_buf()
	} else {
		std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")).join(cwd)
	};
	let resolved_cwd = fs::canonicalize(&raw_cwd).unwrap_or_else(|_| raw_cwd.clone());
	let raw_home = dirs::home_dir();
	let canonical_home = raw_home.as_ref().and_then(|h| fs::canonicalize(h).ok());
	let raw_tmp = std::env::temp_dir();
	let canonical_tmp = fs::canonicalize(&raw_tmp).unwrap_or_else(|_| raw_tmp.clone());

	let try_home_rel = canonical_home.as_ref().and_then(|ch| resolved_cwd.strip_prefix(ch).ok())
		.or_else(|| raw_home.as_ref().and_then(|rh| raw_cwd.strip_prefix(rh).ok()));

	let try_tmp_rel = resolved_cwd.strip_prefix(&canonical_tmp).ok()
		.or_else(|| raw_cwd.strip_prefix(&raw_tmp).ok());

	let encoded_dir_name = if let Some(rel) = try_home_rel {
		let rel_str = rel.to_string_lossy().replace(['/', '\\', ':'], "-");
		if rel_str.is_empty() {
			"-".to_string()
		} else {
			format!("-{rel_str}")
		}
	} else if let Some(rel) = try_tmp_rel {
		let rel_str = rel.to_string_lossy().replace(['/', '\\', ':'], "-");
		if rel_str.is_empty() {
			"-tmp".to_string()
		} else {
			format!("-tmp-{rel_str}")
		}
	} else {
		let abs_str = resolved_cwd.to_string_lossy();
		let stripped = abs_str.trim_start_matches(|c| c == '/' || c == '\\');
		let replaced = stripped.replace(['/', '\\', ':'], "-");
		format!("--{replaced}--")
	};
	let root = sessions_root
		.map(PathBuf::from)
		.unwrap_or_else(SessionOptions::default_sessions_root);
	let legacy_absolute = {
		let abs_str = resolved_cwd.to_string_lossy();
		let stripped = abs_str.trim_start_matches(|c| c == '/' || c == '\\');
		let replaced = stripped.replace(['/', '\\', ':'], "-");
		root.join(format!("--{replaced}--"))
	};
	let legacy_home = raw_home.as_ref().and_then(|home| {
		let rel = raw_cwd.strip_prefix(home).ok()?;
		let home_encoded = home
			.to_string_lossy()
			.trim_start_matches(|c| c == '/' || c == '\\')
			.replace(['/', '\\', ':'], "-");
		let rel_str = rel.to_string_lossy().replace(['/', '\\', ':'], "-");
		let name = if rel_str.is_empty() {
			format!("--{home_encoded}--")
		} else {
			format!("--{home_encoded}-{rel_str}--")
		};
		Some(root.join(name))
	});
	SessionDirPaths {
		current: root.join(encoded_dir_name),
		legacy_absolute,
		legacy_home,
		root,
	}
}

/// Read-only session directories a completion helper must scan.
///
/// Includes the current encoded directory and the legacy absolute-path
/// directory that `compute_default_session_dir` would otherwise migrate.
/// De-duplicated; nothing is created or moved.
#[must_use]
pub fn session_completion_dirs(cwd: &Path, sessions_root: Option<&Path>) -> Vec<PathBuf> {
	let paths = session_dir_paths(cwd, sessions_root);
	let mut dirs = vec![paths.current];
	for legacy in [Some(paths.legacy_absolute), paths.legacy_home].into_iter().flatten() {
		if !dirs.contains(&legacy) {
			dirs.push(legacy);
		}
	}
	dirs
}

pub fn compute_default_session_dir(cwd: &Path, sessions_root: Option<&Path>) -> PathBuf {
	let paths = session_dir_paths(cwd, sessions_root);
	migrate_home_session_dirs(&paths.root);
	migrate_legacy_absolute_session_dir(&paths.legacy_absolute, &paths.current);
	paths.current
}

fn migrate_session_dir_path(old_path: &Path, new_path: &Path) {
	if old_path == new_path || !old_path.is_dir() {
		return;
	}
	if new_path.is_dir() {
		if let Ok(entries) = fs::read_dir(old_path) {
			for entry in entries.flatten() {
				let src = entry.path();
				let dst = new_path.join(entry.file_name());
				if !dst.exists() {
					let _ = fs::rename(&src, &dst);
				}
			}
		}
		let _ = fs::remove_dir(old_path);
	} else {
		let _ = fs::rename(old_path, new_path);
	}
}

fn migrate_home_session_dirs(sessions_root: &Path) {
	let Ok(entries) = fs::read_dir(sessions_root) else {
		return;
	};
	let Some(home) = dirs::home_dir() else {
		return;
	};
	let home_encoded = home
		.to_string_lossy()
		.trim_start_matches(|c| c == '/' || c == '\\')
		.replace(['/', '\\', ':'], "-");
	let old_prefix = format!("--{home_encoded}-");
	let old_exact = format!("--{home_encoded}--");

	for entry in entries.flatten() {
		let name = entry.file_name().to_string_lossy().to_string();
		let remainder = if name == old_exact {
			Some("")
		} else if name.starts_with(&old_prefix) && name.ends_with("--") {
			Some(&name[old_prefix.len()..name.len() - 2])
		} else {
			None
		};

		if let Some(rem) = remainder {
			let new_name = if rem.is_empty() {
				"-".to_string()
			} else {
				format!("-{rem}")
			};
			let old_path = sessions_root.join(&name);
			let new_path = sessions_root.join(new_name);
			migrate_session_dir_path(&old_path, &new_path);
		}
	}
}

fn migrate_legacy_absolute_session_dir(legacy_dir: &Path, session_dir: &Path) {
	if legacy_dir == session_dir || !legacy_dir.exists() {
		return;
	}
	migrate_session_dir_path(legacy_dir, session_dir);
}

#[derive(Debug)]
struct ParsedSession {
	header: SessionHeader,
	turns: Vec<TurnRecord>,
	messages: Vec<MessageEntry>,
	entries: Vec<SessionEntry>,
	is_v1: bool,
	title_slot: Option<TitleSlot>,
}

/// Derive the read-view `TurnRecord` sequence from typed message entries.
/// `TurnRecord` is never a write source; this is a lossy view for callers that
/// only need prompt/assistant/tool text.
pub(crate) fn derive_turn_history(messages: &[MessageEntry]) -> Result<Vec<TurnRecord>, String> {
	let mut turns = Vec::new();
	let mut current_prompt: Option<String> = None;
	let mut current_parts: Vec<ContentPart> = Vec::new();
	let mut current_asst = String::new();
	let mut current_stop = String::new();
	let mut current_tool_calls: Vec<ToolCallRecord> = Vec::new();
	let mut current_matched: std::collections::HashSet<String> = std::collections::HashSet::new();

	let flush =
		|prompt: &mut Option<String>,
		 parts: &mut Vec<ContentPart>,
		 asst: &mut String,
		 stop: &mut String,
		 calls: &mut Vec<ToolCallRecord>,
		 matched: &mut std::collections::HashSet<String>,
		 turns: &mut Vec<TurnRecord>| {
			if let Some(prompt) = prompt.take() {
				// A branch cut can exclude a sibling result while keeping its
				// call; never replay that as an empty success.
				for call in calls.iter_mut().filter(|call| !matched.contains(&call.id)) {
					call.is_error = true;
					if call.result.is_empty() {
						call.result = "Tool result missing (interrupted)".to_string();
					}
				}
				matched.clear();
				turns.push(TurnRecord {
					prompt,
					assistant: std::mem::take(asst),
					stop_reason: if stop.is_empty() {
						"stop".to_string()
					} else {
						canonical_stop_reason(stop).to_string()
					},
					tool_calls: std::mem::take(calls),
					parts: std::mem::take(parts),
				});
			}
		};

	for entry in messages {
		match entry.message.role.as_str() {
			"user" => {
				flush(
					&mut current_prompt,
					&mut current_parts,
					&mut current_asst,
					&mut current_stop,
					&mut current_tool_calls,
					&mut current_matched,
					&mut turns,
				);
				match &entry.message.content {
					MessageContent::Text(text) => {
						current_prompt = Some(text.clone());
						current_parts = Vec::new();
					}
					MessageContent::Parts(parts) => {
						let mut text_parts = Vec::new();
						for (index, p) in parts.iter().enumerate() {
							if p.is_image() {
								let Some(data) = p.data.as_deref() else {
									return Err(format!("image part missing data payload at index {index}"));
								};
								let Some(mime) = p.mime_type.as_deref() else {
									return Err(format!("image part missing mimeType at index {index}"));
								};
								pi_agent::state::normalize_image_payload(data, mime)
									.map_err(|err| format!("invalid image payload in session log at index {index}: {err}"))?;
							} else if p.kind == "text" {
								let Some(t) = p.text.as_deref() else {
									return Err(format!(
										"malformed text content part in session log at index {index}: missing text"
									));
								};
								text_parts.push(t);
							} else {
								return Err(format!(
									"unsupported content part kind in session log at index {index}"
								));
							}
						}
						current_prompt = Some(text_parts.join("\n"));
						current_parts = parts.clone();
					}
					MessageContent::Raw(_) => {
						return Err("unsupported or malformed raw message content shape in session log".to_string());
					}
				}
			},
			"assistant" => {
				if let Some(stop) = &entry.message.stop_reason {
					current_stop = canonical_stop_reason(stop).to_string();
				}
				for part in entry.message.content.parts() {
					if part.kind == "toolCall" {
						current_tool_calls.push(ToolCallRecord {
							id: part
								.extra
								.get("id")
								.and_then(serde_json::Value::as_str)
								.unwrap_or_default()
								.to_string(),
							name: part
								.extra
								.get("name")
								.and_then(serde_json::Value::as_str)
								.unwrap_or_default()
								.to_string(),
							arguments: part
								.extra
								.get("arguments")
								.cloned()
								.unwrap_or(serde_json::Value::Null),
							result: String::new(),
							is_error: false,
							result_parts: Vec::new(),
							details: None,
						});
					}
				}
				let text = entry.message.content.text();
				if !text.is_empty() {
					current_asst = text;
				}
			},
			"toolResult" => {
				let call_id = entry.message.tool_call_id.as_deref().unwrap_or_default();
				let is_error = entry.message.is_error.unwrap_or(false);
				let (result_text, result_parts) = match &entry.message.content {
					MessageContent::Text(text) => (text.clone(), Vec::new()),
					MessageContent::Parts(parts) => {
						let mut text_parts = Vec::new();
						for (index, p) in parts.iter().enumerate() {
							if p.is_image() {
								let Some(data) = p.data.as_deref() else {
									return Err(format!(
										"malformed image part in toolResult at index {index}: missing data"
									));
								};
								let Some(mime) = p.mime_type.as_deref() else {
									return Err(format!(
										"malformed image part in toolResult at index {index}: missing mimeType"
									));
								};
								pi_agent::state::normalize_image_payload(data, mime).map_err(|err| {
									format!("invalid image part in toolResult at index {index}: {err}")
								})?;
							} else if p.kind == "text" {
								let Some(t) = p.text.as_deref() else {
									return Err(format!(
										"malformed text part in toolResult at index {index}: missing text"
									));
								};
								text_parts.push(t);
							} else {
								return Err(format!(
									"unsupported content part kind in toolResult at index {index}"
								));
							}
						}
						let text = if !text_parts.is_empty() {
							text_parts.join("\n")
						} else {
							String::new()
						};
						(text, parts.clone())
					}
					MessageContent::Raw(_) => {
						return Err("unsupported or malformed raw message content shape in toolResult".to_string());
					}
				};

				for call in &mut current_tool_calls {
					if call.id == call_id {
						call.result = result_text.clone();
						call.result_parts = result_parts.clone();
						call.is_error = is_error;
						call.details = entry.message.extra.get("details").cloned();
						current_matched.insert(call_id.to_string());
						break;
					}
				}
			},
			_ => {},
		}
	}

	flush(
		&mut current_prompt,
		&mut current_parts,
		&mut current_asst,
		&mut current_stop,
		&mut current_tool_calls,
		&mut current_matched,
		&mut turns,
	);
	Ok(turns)
}

fn format_safe_serde_error(err: &serde_json::Error) -> String {
	let msg = err.to_string();
	if let Some(rest) = msg.strip_prefix("missing field ") {
		return format!("missing required field {rest}");
	}
	if let Some(rest) = msg.strip_prefix("duplicate field ") {
		return format!("duplicate field {rest}");
	}
	let loc = if err.line() > 0 && err.column() > 0 {
		format!(" at line {} column {}", err.line(), err.column())
	} else {
		String::new()
	};
	if msg.starts_with("invalid type:") {
		return format!("invalid field type{loc}");
	}
	if msg.starts_with("invalid value:") {
		return format!("invalid field value{loc}");
	}
	if msg.starts_with("unknown variant ") {
		return format!("unknown variant{loc}");
	}
	match err.classify() {
		serde_json::error::Category::Syntax => {
			format!("syntax error{loc}")
		},
		serde_json::error::Category::Data => {
			format!("data schema mismatch{loc}")
		},
		serde_json::error::Category::Eof => {
			format!("unexpected end of input{loc}")
		},
		serde_json::error::Category::Io => "I/O error".to_string(),
	}
}

/// Create a session log exclusively (O_EXCL). Fails if the file exists or is a symlink,
/// leaving pre-existing files and symlink targets unmodified.
pub fn create_session_log_exclusive(file: &Path, init_bytes: &[u8]) -> std::io::Result<()> {
	let mut f = OpenOptions::new()
		.write(true)
		.create_new(true)
		.open(file)?;
	f.write_all(init_bytes)?;
	f.flush()?;
	Ok(())
}

fn parse_session_content(file: &Path, text: &str) -> Result<ParsedSession, String> {
	if text.trim().is_empty() {
		return Err(format!("session log {} is empty", file.display()));
	}
	if !text.ends_with('\n') {
		return Err(format!("session log {} has incomplete/unclosed tail record; appending disabled", file.display()));
	}
	let mut lines = text.lines();
	let first_line = lines.next().ok_or_else(|| format!("session log {} is empty", file.display()))?;

	let (title_slot, header_line, start_entries_at_second_line) = match parse_title_slot(first_line) {
		Some(slot) => {
			let second_line = lines
				.next()
				.ok_or_else(|| format!("session log {} has title slot but no session header", file.display()))?;
			(Some(slot), second_line, true)
		},
		None => (None, first_line, false),
	};

	let header_val: serde_json::Value = serde_json::from_str(header_line)
		.map_err(|err| format!("invalid session header in {}: {}", file.display(), format_safe_serde_error(&err)))?;

	let kind = header_val.get("type").and_then(serde_json::Value::as_str).unwrap_or_default();
	let version = header_val.get("version").and_then(serde_json::Value::as_u64);
	let is_v1 = kind == "header" && version == Some(1);
	let is_v3 = kind == "session" && version == Some(CURRENT_SESSION_VERSION as u64);

	if kind == "session" && version != Some(CURRENT_SESSION_VERSION as u64) {
		let ver_str = version.map(|v| v.to_string()).unwrap_or_else(|| "missing".to_string());
		return Err(format!(
			"session log {} has unsupported session version `{ver_str}`; expected version {CURRENT_SESSION_VERSION}",
			file.display()
		));
	}

	if !is_v1 && !is_v3 {
		return Err(format!("session log {} has invalid session header record type `{kind}`", file.display()));
	}

	if is_v1 {
		let (header, turns) = parse_v1_log(file, text)?;
		return Ok(ParsedSession {
			header,
			turns,
			messages: Vec::new(),
			entries: Vec::new(),
			is_v1: true,
			title_slot: None,
		});
	}

	let mut header: SessionHeader = serde_json::from_value(header_val)
		.map_err(|err| format!("cannot parse session header in {}: {}", file.display(), format_safe_serde_error(&err)))?;

	if let Some(slot) = &title_slot {
		if !slot.title.is_empty() {
			header.title = Some(slot.title.clone());
			header.title_source = slot.source.clone();
		}
	}

	let mut messages: Vec<MessageEntry> = Vec::new();
	let mut entries = Vec::new();

	let remaining_lines = if start_entries_at_second_line {
		lines
	} else {
		// We already consumed first_line which was header_line
		lines
	};

	let mut line_number = if start_entries_at_second_line { 2 } else { 1 };
	for line in remaining_lines {
		line_number += 1;
		if line.trim().is_empty() {
			continue;
		}
		let val: serde_json::Value = match serde_json::from_str(line) {
			Ok(v) => v,
			Err(err) => {
				let safe_err = format_safe_serde_error(&err);
				return Err(format!(
					"session log {} has malformed record at line {line_number}: {safe_err}; appending disabled to prevent corrupting log",
					file.display()
				));
			},
		};

		let entry_type = val.get("type").and_then(serde_json::Value::as_str).unwrap_or_default();
		if entry_type.is_empty() {
			return Err(format!(
				"session log {} has malformed record at line {line_number}: missing record type discriminator; appending disabled to prevent corrupting log",
				file.display()
			));
		}
		if entry_type == "message" {
			match serde_json::from_value::<MessageEntry>(val.clone()) {
				Ok(entry) => messages.push(entry),
				Err(err) => {
					let safe_err = format_safe_serde_error(&err);
					return Err(format!(
						"session log {} has malformed message entry at line {line_number}: {safe_err}; appending disabled to prevent corrupting log",
						file.display()
					));
				},
			}
		}

		if let Ok(entry) = serde_json::from_value::<SessionEntry>(val) {
			entries.push(entry);
		}
	}

	let turns = derive_turn_history(&messages)?;

	Ok(ParsedSession {
		header,
		turns,
		messages,
		entries,
		is_v1: false,
		title_slot,
	})
}

fn parse_v1_log(file: &Path, text: &str) -> Result<(SessionHeader, Vec<TurnRecord>), String> {
	let mut header = None;
	let mut turns = Vec::new();
	for (index, line) in text.lines().enumerate() {
		if line.trim().is_empty() {
			continue;
		}
		let value: serde_json::Value = serde_json::from_str(line).map_err(|err| {
			format!(
				"invalid session log {} line {}: {}",
				file.display(),
				index + 1,
				format_safe_serde_error(&err)
			)
		})?;
		let kind = value
			.get("type")
			.and_then(serde_json::Value::as_str)
			.unwrap_or_default()
			.to_string();
		match kind.as_str() {
			"header" => {
				let parsed: HeaderLine = serde_json::from_value(value).map_err(|err| {
					format!(
						"invalid session log {} line {}: {}",
						file.display(),
						index + 1,
						format_safe_serde_error(&err)
					)
				})?;
				header = Some(parsed.header);
			},
			"turn" => {
				let parsed: TurnLine = serde_json::from_value(value).map_err(|err| {
					format!(
						"invalid session log {} line {}: {}",
						file.display(),
						index + 1,
						format_safe_serde_error(&err)
					)
				})?;
				turns.push(parsed.turn);
			},
			_ => {},
		}
	}
	let header = header.ok_or_else(|| format!("session log {} has no header", file.display()))?;
	Ok((header, turns))
}

#[derive(Debug, Serialize, Deserialize)]
struct HeaderLine {
	#[serde(rename = "type")]
	kind: String,
	#[serde(flatten)]
	header: SessionHeader,
}

#[derive(Debug, Serialize, Deserialize)]
struct TurnLine {
	#[serde(rename = "type")]
	kind: String,
	#[serde(flatten)]
	turn: TurnRecord,
}

fn find_session_by_id_or_stem(dir: &Path, id_or_stem: &str) -> Result<Option<PathBuf>, String> {
	let legacy_file = dir.join(format!("{id_or_stem}.{LOG_EXTENSION}"));
	if legacy_file.is_file() {
		return Ok(Some(legacy_file));
	}
	let Ok(entries) = fs::read_dir(dir) else {
		return Ok(None);
	};
	let mut matches: Vec<(SystemTime, PathBuf)> = Vec::new();
	for entry in entries.flatten() {
		let path = entry.path();
		if path.extension().and_then(|s| s.to_str()) != Some(LOG_EXTENSION) {
			continue;
		}
		let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
			continue;
		};
		let is_match = stem == id_or_stem
			|| stem
				.split_once('_')
				.map_or(false, |(ts, id)| is_valid_file_safe_timestamp(ts) && id == id_or_stem);
		if is_match {
			let mtime = entry.metadata().and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
			matches.push((mtime, path));
		}
	}
	matches.sort_by(|a, b| b.0.cmp(&a.0));
	Ok(matches.into_iter().next().map(|(_, p)| p))
}

#[derive(Debug, Clone, PartialEq)]
pub struct TerminalBreadcrumb {
	pub cwd: PathBuf,
	pub session_file: PathBuf,
	pub exists: bool,
	pub fresh: bool,
}

/// Sanitize a terminal ID component from the environment into a safe, single
/// file name without path traversal sequences.
pub fn sanitize_terminal_id(id: &str) -> Option<String> {
	let trimmed = id.trim();
	if trimmed.is_empty() || trimmed.contains('\0') || trimmed.contains("..") {
		return None;
	}
	let sanitized: String = trimmed
		.chars()
		.map(|c| if c == '/' || c == '\\' { '-' } else { c })
		.collect();
	let sanitized = sanitized.trim_matches(|c: char| c == '-' || c == '.');
	if sanitized.is_empty() || sanitized.contains("..") {
		return None;
	}
	let path = Path::new(&sanitized);
	let mut components = path.components();
	match (components.next(), components.next()) {
		(Some(std::path::Component::Normal(comp)), None) => {
			let s = comp.to_str()?;
			if s.is_empty() || s == "." || s == ".." {
				None
			} else {
				Some(s.to_string())
			}
		},
		_ => None,
	}
}

pub fn get_terminal_id() -> Option<String> {
	let raw = get_raw_terminal_id()?;
	sanitize_terminal_id(&raw)
}

fn get_raw_terminal_id() -> Option<String> {
	if let Ok(pane) = std::env::var("ZELLIJ_PANE_ID") {
		let session = std::env::var("ZELLIJ_SESSION_NAME").ok()
			.map(|s| s.replace(['/', '\\'], "-"));
		return Some(match session {
			Some(s) => format!("zellij-{s}-{pane}"),
			None => format!("zellij-{pane}"),
		});
	}
	if let Ok(pane) = std::env::var("TMUX_PANE") {
		return Some(format!("tmux-{pane}"));
	}
	if let Ok(surface) = std::env::var("CMUX_SURFACE_ID") {
		return Some(format!("cmux-{surface}"));
	}
	if let Ok(id) = std::env::var("KITTY_WINDOW_ID") {
		return Some(format!("kitty-{id}"));
	}
	if let Ok(pane) = std::env::var("WEZTERM_PANE") {
		return Some(format!("wezterm-{pane}"));
	}
	if let Ok(id) = std::env::var("TERM_SESSION_ID") {
		return Some(format!("apple-{id}"));
	}
	if let Ok(id) = std::env::var("WT_SESSION") {
		return Some(format!("wt-{id}"));
	}
	None
}

pub fn write_terminal_breadcrumb(
	cwd: &Path,
	session_file: &Path,
	fresh: bool,
	override_path: Option<&Path>,
) -> Result<(), String> {
	let breadcrumb_file = match override_path {
		Some(p) => p.to_path_buf(),
		None => {
			let Some(term_id) = get_terminal_id() else {
				return Ok(());
			};
			let dir = dirs::agent_subdir("terminal-sessions", dirs::AgentDirCategory::State);
			let _ = fs::create_dir_all(&dir);
			dir.join(term_id)
		},
	};

	if let Some(parent) = breadcrumb_file.parent() {
		fs::create_dir_all(parent).map_err(|e| e.to_string())?;
	}

	let content = if fresh {
		format!("{}\n{}\nfresh\n", cwd.display(), session_file.display())
	} else {
		format!("{}\n{}\n", cwd.display(), session_file.display())
	};

	fs::write(&breadcrumb_file, content).map_err(|e| e.to_string())
}

pub fn read_terminal_breadcrumb(override_path: Option<&Path>) -> Option<TerminalBreadcrumb> {
	let breadcrumb_file = match override_path {
		Some(p) => p.to_path_buf(),
		None => {
			let term_id = get_terminal_id()?;
			dirs::agent_subdir("terminal-sessions", dirs::AgentDirCategory::State).join(term_id)
		},
	};

	let content = fs::read_to_string(&breadcrumb_file).ok()?;
	let lines: Vec<&str> = content.lines().collect();
	if lines.len() < 2 {
		return None;
	}
	let breadcrumb_cwd = PathBuf::from(lines[0]);
	let session_file = PathBuf::from(lines[1]);
	let fresh = lines.get(2).map_or(false, |l| *l == "fresh");

	let exists = session_file.is_file();
	if exists || fresh {
		Some(TerminalBreadcrumb {
			cwd: breadcrumb_cwd,
			session_file,
			exists,
			fresh,
		})
	} else {
		None
	}
}

/// TS `artifactsDirectoryFor`: session file minus `.jsonl` suffix.
fn artifacts_dir_for(session_file: &Path) -> Option<PathBuf> {
	if session_file.extension().and_then(|ext| ext.to_str()) == Some(LOG_EXTENSION) {
		Some(session_file.with_extension(""))
	} else {
		None
	}
}

/// Lenient header `cwd` peek for discovery checks (never fails the caller).
fn peek_header_cwd(session_file: &Path) -> Option<PathBuf> {
	let content = fs::read_to_string(session_file).ok()?;
	let mut lines = content.lines();
	let first = lines.next()?;
	let header_line = if parse_title_slot(first).is_some() {
		lines.next()?
	} else {
		first
	};
	let value: serde_json::Value = serde_json::from_str(header_line).ok()?;
	let cwd = value.get("cwd").and_then(serde_json::Value::as_str)?;
	Some(PathBuf::from(cwd))
}

/// TS: a "genuine" session of the current project is one whose header `cwd`
/// resolves to the launch cwd.
fn find_session_with_cwd(
	dir: &Path,
	resolved_cwd: &Path,
	exclude: Option<&Path>,
) -> Result<Option<PathBuf>, String> {
	let Ok(entries) = fs::read_dir(dir) else {
		return Ok(None);
	};
	let canonical_exclude =
		exclude.map(|path| fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()));
	let mut matches: Vec<(SystemTime, PathBuf)> = Vec::new();
	for entry in entries.flatten() {
		let path = entry.path();
		if path.extension().and_then(|ext| ext.to_str()) != Some(LOG_EXTENSION) {
			continue;
		}
		if let Some(exclude) = &canonical_exclude
			&& fs::canonicalize(&path).unwrap_or_else(|_| path.clone()) == *exclude
		{
			continue;
		}
		let Some(cwd) = peek_header_cwd(&path) else {
			continue;
		};
		if fs::canonicalize(&cwd).unwrap_or(cwd) == resolved_cwd {
			let modified = entry
				.metadata()
				.and_then(|metadata| metadata.modified())
				.unwrap_or(SystemTime::UNIX_EPOCH);
			matches.push((modified, path));
		}
	}
	matches.sort_by(|a, b| b.0.cmp(&a.0));
	Ok(matches.into_iter().next().map(|(_, path)| path))
}

/// TS `SessionManager.moveTo`: relocate a session log into the session
/// directory for `new_cwd`, re-root its header (`cwd`, `previousSessionFiles`),
/// move the artifacts directory and update the terminal breadcrumb. Refuses to
/// overwrite any existing target (collision keeps both copies intact).
pub fn move_session_to(
	session_file: &Path,
	new_cwd: &Path,
	target_session_dir: Option<&Path>,
	breadcrumb_override: Option<&Path>,
) -> Result<PathBuf, String> {
	let content = fs::read_to_string(session_file)
		.map_err(|err| format!("cannot read session log {}: {err}", session_file.display()))?;
	let parsed = parse_session_content(session_file, &content)?;
	if parsed.is_v1 {
		return Err(format!(
			"cannot move legacy v1 session log {}: read-only; import it first",
			session_file.display()
		));
	}
	let source = fs::canonicalize(session_file).unwrap_or_else(|_| session_file.to_path_buf());
	let new_cwd_abs = fs::canonicalize(new_cwd).unwrap_or_else(|_| new_cwd.to_path_buf());
	let next_dir = target_session_dir
		.map(PathBuf::from)
		.unwrap_or_else(|| compute_default_session_dir(&new_cwd_abs, None));
	fs::create_dir_all(&next_dir)
		.map_err(|err| format!("cannot create {}: {err}", next_dir.display()))?;
	let dest = next_dir.join(source.file_name().unwrap_or_default());

	let mut session_moved = false;
	if dest != source {
		if dest.exists() {
			return Err(format!(
				"cannot move session {}: target {} already exists; refusing to overwrite",
				source.display(),
				dest.display()
			));
		}
		fs::rename(&source, &dest)
			.map_err(|err| format!("cannot move session {}: {err}", source.display()))?;
		session_moved = true;
	}

	let artifacts_source = artifacts_dir_for(&source);
	let artifacts_dest = artifacts_dir_for(&dest);
	if let (Some(old_art), Some(new_art)) = (&artifacts_source, &artifacts_dest)
		&& old_art != new_art
		&& old_art.is_dir()
	{
		if new_art.exists() {
			if session_moved {
				let _ = fs::rename(&dest, &source);
			}
			return Err(format!(
				"cannot move session artifacts: target {} already exists; refusing to overwrite",
				new_art.display()
			));
		}
		if let Err(err) = fs::rename(old_art, new_art) {
			if session_moved {
				let _ = fs::rename(&dest, &source);
			}
			return Err(format!("cannot move session artifacts: {err}"));
		}
	}

	let mut header = parsed.header;
	if session_moved {
		let previous = source.to_string_lossy().to_string();
		if !header.previous_session_files.contains(&previous) {
			header.previous_session_files.push(previous);
		}
	}
	header.cwd = new_cwd_abs.display().to_string();

	let slot = parsed
		.title_slot
		.as_ref()
		.map(|slot| serialize_title_slot(&slot.title, slot.source.as_deref(), &slot.updated_at))
		.unwrap_or_else(|| {
			serialize_title_slot(
				header.title.as_deref().unwrap_or(""),
				header.title_source.as_deref(),
				&header.timestamp,
			)
		});
	let mut rewritten = slot;
	rewritten.push_str(&serde_json::to_string(&header).map_err(|err| err.to_string())?);
	rewritten.push('\n');
	for line in content.lines().skip(if parsed.title_slot.is_some() { 2 } else { 1 }) {
		rewritten.push_str(line);
		rewritten.push('\n');
	}

	let tmp = dest.with_extension("jsonl.tmp-move");
	fs::write(&tmp, &rewritten)
		.map_err(|err| format!("cannot rewrite moved session: {err}"))?;
	fs::rename(&tmp, &dest)
		.map_err(|err| format!("cannot rewrite moved session: {err}"))?;
	let _ = write_terminal_breadcrumb(&new_cwd_abs, &dest, false, breadcrumb_override);
	Ok(dest)
}

/// TS `SessionManager.continueRecent` incl. moved-project re-root: when the
/// breadcrumb cwd is gone and the breadcrumb session is the newest candidate
/// with no genuine session of the current project, the session is relocated
/// (`move_session_to`) instead of resumed with a stale cwd.
pub fn continue_recent(cwd: &Path, session_dir: Option<&Path>) -> Result<Option<PathBuf>, String> {
	continue_recent_with_breadcrumb(cwd, session_dir, None)
}

/// [`continue_recent`] with an explicit terminal-breadcrumb file (multi-terminal
/// routing and isolated tests).
pub fn continue_recent_with_breadcrumb(
	cwd: &Path,
	session_dir: Option<&Path>,
	breadcrumb_override: Option<&Path>,
) -> Result<Option<PathBuf>, String> {
	let dir = session_dir
		.map(PathBuf::from)
		.unwrap_or_else(|| compute_default_session_dir(cwd, None));
	let resolved_cwd = fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());

	if let Some(breadcrumb) = read_terminal_breadcrumb(breadcrumb_override) {
		if breadcrumb.fresh && !breadcrumb.exists {
			return Ok(None);
		}
		let breadcrumb_cwd = fs::canonicalize(&breadcrumb.cwd).unwrap_or_else(|_| breadcrumb.cwd.clone());
		if breadcrumb_cwd == resolved_cwd {
			return Ok(Some(breadcrumb.session_file));
		}

		let mut newest_in_target_dir = find_most_recent_session_in_dir(&dir)?;
		let breadcrumb_file = fs::canonicalize(&breadcrumb.session_file)
			.unwrap_or_else(|_| breadcrumb.session_file.clone());
		let breadcrumb_cwd_missing = !breadcrumb_cwd.exists();
		let newest_is_breadcrumb = newest_in_target_dir.as_ref().map_or(false, |p| {
			fs::canonicalize(p).unwrap_or_else(|_| p.clone()) == breadcrumb_file
		});

		let mut current_project_already_has_session = false;
		if breadcrumb_cwd_missing && newest_is_breadcrumb
			&& let Some(local) = find_session_with_cwd(&dir, &resolved_cwd, Some(&breadcrumb_file))?
		{
			newest_in_target_dir = Some(local);
			current_project_already_has_session = true;
		}

		let looks_like_moved_project = breadcrumb_cwd_missing
			&& (newest_in_target_dir.is_none()
				|| (newest_is_breadcrumb && !current_project_already_has_session));
		if looks_like_moved_project {
			return move_session_to(
			&breadcrumb.session_file,
			&resolved_cwd,
			session_dir,
			breadcrumb_override,
		)
		.map(Some);
		}

		if let Some(newest) = newest_in_target_dir {
			return Ok(Some(newest));
		}
	}

	find_most_recent_session_in_dir(&dir)
}

fn find_most_recent_session_in_dir(dir: &Path) -> Result<Option<PathBuf>, String> {
	let Ok(entries) = fs::read_dir(dir) else {
		return Ok(None);
	};
	let mut newest: Option<(SystemTime, String, PathBuf)> = None;
	for entry in entries.flatten() {
		let path = entry.path();
		if path.extension().and_then(|ext| ext.to_str()) != Some(LOG_EXTENSION) {
			continue;
		}
		let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
			continue;
		};
		let modified = entry
			.metadata()
			.and_then(|metadata| metadata.modified())
			.unwrap_or(SystemTime::UNIX_EPOCH);
		let replace = match &newest {
			None => true,
			Some((time, id, _)) => modified > *time || (modified == *time && stem > id.as_str()),
		};
		if replace {
			newest = Some((modified, stem.to_string(), path));
		}
	}
	Ok(newest.map(|(_, _, path)| path))
}

pub fn session_matches_resume_arg(session_id: &str, file_path: &Path, arg: &str) -> bool {
	let trimmed = arg.trim();
	if trimmed.is_empty() {
		return false;
	}
	let norm_arg = trimmed.to_lowercase();
	let norm_id = session_id.to_lowercase();

	if norm_id.starts_with(&norm_arg) {
		return true;
	}

	let Some(stem) = file_path.file_stem().and_then(|s| s.to_str()) else {
		return false;
	};
	let norm_stem = stem.to_lowercase();
	if norm_stem.starts_with(&norm_arg) {
		return true;
	}

	if let Some(pos) = norm_stem.rfind('_') {
		let suffix = &norm_stem[pos + 1..];
		if suffix.starts_with(&norm_arg) {
			return true;
		}
	}

	false
}

fn scan_dir_for_resume(dir: &Path, arg: &str) -> Result<Option<PathBuf>, String> {
	let Ok(entries) = fs::read_dir(dir) else {
		return Ok(None);
	};
	let mut matches: Vec<(SystemTime, PathBuf)> = Vec::new();
	for entry in entries.flatten() {
		let path = entry.path();
		if path.extension().and_then(|s| s.to_str()) != Some(LOG_EXTENSION) {
			continue;
		}
		let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
		let id = stem.rfind('_').map_or(stem, |pos| &stem[pos + 1..]);
		if session_matches_resume_arg(id, &path, arg) {
			let mtime = entry.metadata().and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
			matches.push((mtime, path));
		}
	}
	matches.sort_by(|a, b| b.0.cmp(&a.0));
	Ok(matches.into_iter().next().map(|(_, path)| path))
}

pub fn resolve_resumable_session(
	arg: &str,
	cwd: &Path,
	session_dir: Option<&Path>,
	allow_global_fallback: bool,
) -> Result<Option<PathBuf>, String> {
	if arg.trim().is_empty() {
		return Ok(None);
	}
	let local_dir = session_dir
		.map(PathBuf::from)
		.unwrap_or_else(|| compute_default_session_dir(cwd, None));

	if let Some(matched) = scan_dir_for_resume(&local_dir, arg)? {
		return Ok(Some(matched));
	}

	if session_dir.is_some() || !allow_global_fallback {
		return Ok(None);
	}

	let root = SessionOptions::default_sessions_root();
	if !root.is_dir() {
		return Ok(None);
	}
	for entry in fs::read_dir(&root).map_err(|e| e.to_string())?.flatten() {
		let path = entry.path();
		if path.is_dir()
			&& let Some(matched) = scan_dir_for_resume(&path, arg)?
		{
			return Ok(Some(matched));
		}
	}

	Ok(None)
}

pub fn fork_session(
	source_path: &Path,
	new_cwd: Option<&Path>,
	target_dir: Option<&Path>,
) -> Result<PathBuf, String> {
	let content = fs::read_to_string(source_path)
		.map_err(|e| format!("cannot read source session {}: {e}", source_path.display()))?;
	let ParsedSession { mut header, is_v1, .. } = parse_session_content(source_path, &content)?;
	if is_v1 {
		return Err("cannot fork legacy v1 session directly; import it first".to_string());
	}

	let original_id = header.id.clone();
	let new_id = output::session_id();
	let now = output::iso8601(SystemTime::now());
	let ts = file_safe_timestamp(&now);

	let target_dir = target_dir
		.map(PathBuf::from)
		.or_else(|| source_path.parent().map(Path::to_path_buf))
		.unwrap_or_else(|| PathBuf::from("."));
	fs::create_dir_all(&target_dir).map_err(|e| e.to_string())?;

	let target_file = target_dir.join(format!("{ts}_{new_id}.{LOG_EXTENSION}"));

	header.id = new_id;
	header.parent_session = Some(original_id.clone());
	// TS fork (`session-manager.ts:1505`): inherit the provider prompt-cache
	// identity, falling back to the parent session id.
	header.provider_prompt_cache_key = header
		.provider_prompt_cache_key
		.take()
		.or(Some(original_id));
	header.timestamp = now.clone();
	if let Some(cwd) = new_cwd {
		header.cwd = cwd.display().to_string();
	}

	let title = header.title.as_deref().unwrap_or("Forked Session");
	let title_slot = serialize_title_slot(title, header.title_source.as_deref(), &now);

	let mut lines = Vec::new();
	lines.push(title_slot);
	lines.push(format!("{}\n", serde_json::to_string(&header).map_err(|e| e.to_string())?));

	for line in content.lines() {
		if line.trim().is_empty() {
			continue;
		}
		if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
			let kind = val.get("type").and_then(serde_json::Value::as_str).unwrap_or_default();
			if kind == "title" || kind == "session" || kind == "header" {
				continue;
			}
			lines.push(format!("{line}\n"));
		}
	}

	let mut file = OpenOptions::new()
		.write(true)
		.create_new(true)
		.open(&target_file)
		.map_err(|e| format!("cannot create fork target {}: {e}", target_file.display()))?;
	for line in lines {
		file.write_all(line.as_bytes()).map_err(|e| e.to_string())?;
	}

	// TS fork copies the session artifacts directory alongside the log.
	if let (Some(old_art), Some(new_art)) =
		(artifacts_dir_for(source_path), artifacts_dir_for(&target_file))
		&& old_art.is_dir()
	{
		copy_dir_recursive(&old_art, &new_art).map_err(|e| {
			let _ = fs::remove_file(&target_file);
			e
		})?;
	}

	Ok(target_file)
}

/// Recursive directory copy (TS `fs.cp(..., { recursive: true })`).
fn copy_dir_recursive(source: &Path, dest: &Path) -> Result<(), String> {
	fs::create_dir_all(dest).map_err(|e| format!("cannot create {}: {e}", dest.display()))?;
	for entry in fs::read_dir(source).map_err(|e| e.to_string())? {
		let entry = entry.map_err(|e| e.to_string())?;
		let from = entry.path();
		let to = dest.join(entry.file_name());
		if from.is_dir() {
			copy_dir_recursive(&from, &to)?;
		} else {
			fs::copy(&from, &to)
				.map_err(|e| format!("cannot copy {}: {e}", from.display()))?;
		}
	}
	Ok(())
}

pub fn import_legacy_v1(
	v1_path: &Path,
	target_dir: &Path,
	fallback_cwd: Option<&Path>,
) -> Result<PathBuf, String> {
	let content = fs::read_to_string(v1_path)
		.map_err(|err| format!("cannot read v1 session {}: {err}", v1_path.display()))?;
	let (v1_header, v1_turns) = parse_v1_log(v1_path, &content)?;

	let cwd = fallback_cwd
		.map(|p| p.display().to_string())
		.unwrap_or_else(|| v1_header.cwd.clone());
	let id = v1_header.id.clone();
	let ts = file_safe_timestamp(&v1_header.timestamp);
	let target_file = target_dir.join(format!("{ts}_{id}.{LOG_EXTENSION}"));
	if target_file.exists() {
		return Err(format!("cannot import legacy v1 session: target file {} already exists; refusing to overwrite", target_file.display()));
	}
	let title = v1_turns.first().map(|t| t.prompt.as_str()).unwrap_or("Imported Session");
	let title_slot = serialize_title_slot(title, Some("auto"), &v1_header.timestamp);

	let v3_header = SessionHeader {
		kind: "session".to_string(),
		version: CURRENT_SESSION_VERSION,
		id: id.clone(),
		timestamp: v1_header.timestamp.clone(),
		cwd,
		model: v1_header.model.clone(),
		provider: v1_header.provider.clone(),
		title: Some(title.to_string()),
		title_source: Some("auto".to_string()),
		additional_directories: Vec::new(),
		parent_session: None,
		previous_session_files: Vec::new(),
		// TS (`session-manager.ts:2817`): cache identity falls back to the source id.
		provider_prompt_cache_key: Some(id.clone()),
	};

	let mut lines = Vec::new();
	lines.push(title_slot);
	lines.push(format!("{}\n", serde_json::to_string(&v3_header).map_err(|e| e.to_string())?));

	let import_custom = serde_json::json!({
		"type": "custom",
		"id": format!("import-{id}"),
		"parentId": null,
		"timestamp": v1_header.timestamp,
		"customType": "foreign_session_import",
		"data": {
			"source": "omp-cli-v1",
			"sourceId": id,
			"sourcePath": v1_path.display().to_string(),
			"sourceCwd": v1_header.cwd,
			"turnCount": v1_turns.len(),
		}
	});
	lines.push(format!("{}\n", serde_json::to_string(&import_custom).map_err(|e| e.to_string())?));

	// Truthful metadata only: v1 logs carry provider/model but no api dialect
	// and no token usage. Unknown fields are omitted, never invented
	// (matches TS foreign import: only source-known data is preserved).
	let import_ms = parse_iso8601_ms(&v1_header.timestamp);
	let provider = v1_header.provider.clone();
	let model = v1_header.model.clone();

	let mut parent_id: Option<String> = None;
	for (turn_idx, turn) in v1_turns.iter().enumerate() {
		let user_id = format!("msg-user-{turn_idx}");
		let user_entry = MessageEntry::new(
			user_id.clone(),
			parent_id.take(),
			v1_header.timestamp.clone(),
			SessionMessage {
				timestamp_ms: import_ms,
				..SessionMessage::user(MessageContent::Text(turn.prompt.clone()), 0)
			},
		);
		lines.push(format!("{}\n", serde_json::to_string(&user_entry).map_err(|e| e.to_string())?));
		parent_id = Some(user_id);

		if !turn.tool_calls.is_empty() {
			let tool_calls_parts: Vec<ContentPart> = turn
				.tool_calls
				.iter()
				.map(|tc| {
					let mut part = ContentPart {
						kind: "toolCall".to_string(),
						text: None,
						data: None,
						mime_type: None,
						extra: serde_json::Map::new(),
					};
					part.extra.insert("id".to_string(), serde_json::json!(tc.id));
					part.extra.insert("name".to_string(), serde_json::json!(tc.name));
					part.extra.insert("arguments".to_string(), tc.arguments.clone());
					part
				})
				.collect();

			let asst_tool_id = format!("msg-asst-call-{turn_idx}");
			let asst_tool_entry = MessageEntry::new(
				asst_tool_id.clone(),
				parent_id.take(),
				v1_header.timestamp.clone(),
				SessionMessage {
					role: "assistant".to_string(),
					content: MessageContent::Parts(tool_calls_parts),
					tool_call_id: None,
					tool_name: None,
					api: None,
					provider: provider.clone(),
					model: model.clone(),
					usage: None,
					stop_reason: Some("toolUse".to_string()),
					error_message: None,
					timestamp_ms: import_ms,
					is_error: None,
					extra: serde_json::Map::new(),
				},
			);
			lines.push(format!("{}\n", serde_json::to_string(&asst_tool_entry).map_err(|e| e.to_string())?));
			parent_id = Some(asst_tool_id);

			for (tc_idx, tc) in turn.tool_calls.iter().enumerate() {
				let res_id = format!("msg-tool-res-{turn_idx}-{tc_idx}");
				let content = if !tc.result_parts.is_empty() {
					MessageContent::Parts(tc.result_parts.clone())
				} else {
					MessageContent::Text(tc.result.clone())
				};
				let mut message = SessionMessage {
					timestamp_ms: import_ms,
					..SessionMessage::tool_result(
						tc.id.clone(),
						tc.name.clone(),
						content,
						tc.is_error,
						0,
					)
				};
				if let Some(details) = &tc.details {
					message.extra.insert("details".to_string(), details.clone());
				}
				let res_entry = MessageEntry::new(
					res_id.clone(),
					parent_id.take(),
					v1_header.timestamp.clone(),
					message,
				);
				lines.push(format!("{}\n", serde_json::to_string(&res_entry).map_err(|e| e.to_string())?));
				parent_id = Some(res_id);
			}
		}

		let asst_id = format!("msg-asst-{turn_idx}");
		let stop_reason =
			if turn.stop_reason.is_empty() { None } else { Some(turn.stop_reason.clone()) };
		let asst_entry = MessageEntry::new(
			asst_id.clone(),
			parent_id.take(),
			v1_header.timestamp.clone(),
			SessionMessage {
				role: "assistant".to_string(),
				content: MessageContent::Text(turn.assistant.clone()),
				tool_call_id: None,
				tool_name: None,
				api: None,
				provider: provider.clone(),
				model: model.clone(),
				usage: None,
				stop_reason,
				error_message: None,
				timestamp_ms: import_ms,
				is_error: None,
				extra: serde_json::Map::new(),
			},
		);
		lines.push(format!("{}\n", serde_json::to_string(&asst_entry).map_err(|e| e.to_string())?));
		parent_id = Some(asst_id);
	}

	fs::create_dir_all(target_dir).map_err(|e| e.to_string())?;
	let mut file = OpenOptions::new()
		.write(true)
		.create_new(true)
		.open(&target_file)
		.map_err(|e| format!("cannot create import target {}: {e}", target_file.display()))?;
	for line in lines {
		file.write_all(line.as_bytes()).map_err(|e| e.to_string())?;
	}

	let validated = fs::read_to_string(&target_file).map_err(|e| e.to_string())?;
	let val = parse_session_content(&target_file, &validated)?;
	if val.is_v1 || val.header.id != id || val.turns.len() != v1_turns.len() {
		let _ = fs::remove_file(&target_file);
		return Err(format!("validation failed for imported session {}", target_file.display()));
	}

	Ok(target_file)
}
/// Newest session in `dir` as `(id, path)` — session listing API for CLI integration.
pub fn latest_session(dir: &Path) -> Result<Option<(String, PathBuf)>, String> {
	let entries = fs::read_dir(dir)
		.map_err(|err| format!("cannot read session directory {}: {err}", dir.display()))?;
	let mut newest: Option<(SystemTime, String, PathBuf)> = None;
	for entry in entries {
		let entry = entry.map_err(|err| format!("cannot read session directory entry: {err}"))?;
		let path = entry.path();
		if path.extension().and_then(|extension| extension.to_str()) != Some(LOG_EXTENSION) {
			continue;
		}
		let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
			continue;
		};
		let modified = entry
			.metadata()
			.and_then(|metadata| metadata.modified())
			.unwrap_or(SystemTime::UNIX_EPOCH);
		let replace = match &newest {
			None => true,
			Some((time, id, _)) => modified > *time || (modified == *time && stem > id.as_str()),
		};
		if replace {
			newest = Some((modified, stem.to_string(), path));
		}
	}
	Ok(newest.map(|(_, id, path)| (id, path)))
}

#[cfg(test)]
mod tests {
	use std::time::{Duration, UNIX_EPOCH};
	use serde_json::json;
	use super::*;

	static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

	fn temp_dir() -> PathBuf {
		let unique = format!(
			"omp-cli-session-{}-{}",
			std::process::id(),
			COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
		);
		let dir = std::env::temp_dir().join(unique);
		fs::create_dir_all(&dir).expect("temp dir");
		dir
	}

	fn options(dir: PathBuf, id: Option<&str>) -> SessionOptions {
		SessionOptions {
			enabled: true,
			cwd: PathBuf::from("/work"),
			model: Some("local/llama3".to_string()),
			provider: Some("local".to_string()),
			dir: Some(dir),
			id: id.map(str::to_string),
			continue_latest: false,
			no_title: false,
		}
	}

	fn turn(prompt: &str, assistant: &str) -> TurnRecord {
		TurnRecord {
			prompt: prompt.to_string(),
			assistant: assistant.to_string(),
			stop_reason: "stop".to_string(),
			tool_calls: vec![ToolCallRecord {
				id: "call-1".to_string(),
				name: "read".to_string(),
				arguments: json!({"path": "a.txt"}),
				result: "contents".to_string(),
				is_error: false,
				result_parts: Vec::new(),
				details: None,
			}],
			parts: Vec::new(),
		}
	}

	#[test]
	fn session_id_whitelist_rejects_path_characters() {
		for bad in [":", "C:", "/", "a/b", "..", ".", "", "a\\b", "id space"] {
			assert!(!valid_session_id(bad), "{bad:?} must be rejected");
		}
		for good in ["s1", "abc-DEF_1.2", "0f.3e-1"] {
			assert!(valid_session_id(good), "{good:?} must be accepted");
		}

		let dir = temp_dir();
		{
			let store = SessionStore::open(&options(dir.clone(), Some("C:")));
			assert!(!store.enabled, "invalid id must disable persistence");
			assert!(store.file.is_none());
			assert!(
				store
					.warnings
					.iter()
					.any(|warning| warning.contains("invalid session id"))
			);
		}
		assert_eq!(fs::read_dir(&dir).expect("list").count(), 0, "nothing written");
		fs::remove_dir_all(&dir).ok();
	}

	#[test]
	fn latest_session_tie_breaks_on_id_not_readdir_order() {
		let dir = temp_dir();
		let stamp = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
		for id in ["z-old", "a-new"] {
			let path = dir.join(format!("{id}.{LOG_EXTENSION}"));
			fs::write(&path, "").expect("seed log");
			let file = fs::File::options()
				.write(true)
				.open(&path)
				.expect("open log");
			file
				.set_times(fs::FileTimes::new().set_modified(stamp))
				.expect("same mtime");
		}
		let (id, path) = latest_session(&dir).expect("scan").expect("a session");
		assert_eq!(id, "z-old");
		assert_eq!(path, dir.join(format!("z-old.{LOG_EXTENSION}")));
		fs::remove_dir_all(&dir).ok();
	}

	#[test]
	fn latest_session_prefers_the_newer_mtime() {
		let dir = temp_dir();
		let old = dir.join(format!("a.{LOG_EXTENSION}"));
		let new = dir.join(format!("z.{LOG_EXTENSION}"));
		fs::write(&old, "").expect("old");
		fs::write(&new, "").expect("new");
		let file = fs::File::options().write(true).open(&old).expect("open");
		file
			.set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(10)))
			.expect("stamp");
		let file = fs::File::options().write(true).open(&new).expect("open");
		file
			.set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(20)))
			.expect("stamp");
		let (id, _) = latest_session(&dir).expect("scan").expect("a session");
		assert_eq!(id, "z");
		fs::remove_dir_all(&dir).ok();
	}

	#[test]
	fn jsonl_round_trip_preserves_header_and_turns() {
		let dir = temp_dir();
		let first = turn("fix", "done");
		{
			let mut store = SessionStore::open(&options(dir.clone(), Some("s1")));
			assert!(store.enabled());
			assert_eq!(store.header().expect("header").id, "s1");
			store.append_turn(&first).expect("append");
			assert!(store.take_warnings().is_empty());
		}
		let mut reopened = SessionStore::open(&options(dir.clone(), Some("s1")));
		assert!(reopened.enabled());
		assert_eq!(reopened.history().to_vec(), vec![first.clone()]);
		let second = TurnRecord {
			prompt: "again".to_string(),
			assistant: "ok".to_string(),
			stop_reason: "stop".to_string(),
			tool_calls: Vec::new(),
			parts: Vec::new(),
		};
		reopened.append_turn(&second).expect("append");
		drop(reopened);

		let resumed = SessionStore::open(&options(dir.clone(), Some("s1")));
		assert_eq!(resumed.history().to_vec(), vec![first, second]);
		assert_eq!(resumed.header().expect("header").cwd, "/work");
		fs::remove_dir_all(dir).ok();
	}

	#[test]
	fn continue_picks_the_newest_log_by_mtime() {
		let dir = temp_dir();
		let mut older = SessionStore::open(&options(dir.clone(), Some("older")));
		older
			.append_turn(&turn("old", "old answer"))
			.expect("append");
		let older_file = older.file().expect("older file").to_path_buf();
		drop(older);
		let mut newer = SessionStore::open(&options(dir.clone(), Some("newer")));
		newer
			.append_turn(&turn("new", "new answer"))
			.expect("append");
		let newer_file = newer.file().expect("newer file").to_path_buf();
		drop(newer);
		let base = UNIX_EPOCH + Duration::from_secs(1_000);
		fs::File::options()
			.write(true)
			.open(&older_file)
			.expect("open older")
			.set_modified(base)
			.expect("mtime older");
		fs::File::options()
			.write(true)
			.open(&newer_file)
			.expect("open newer")
			.set_modified(base + Duration::from_secs(10))
			.expect("mtime newer");

		let mut resumed = SessionStore::open(&SessionOptions {
			continue_latest: true,
			..options(dir.clone(), None)
		});
		assert!(resumed.enabled());
		assert_eq!(resumed.id(), "newer");
		assert_eq!(resumed.history().to_vec(), vec![turn("new", "new answer")]);
		assert!(resumed.take_warnings().is_empty());
		fs::remove_dir_all(dir).ok();
	}

	#[test]
	fn no_session_writes_nothing() {
		let dir = temp_dir().join("never-created");
		let mut store =
			SessionStore::open(&SessionOptions { enabled: false, ..options(dir.clone(), Some("s1")) });
		assert!(!store.enabled());
		store
			.append_turn(&turn("hi", "there"))
			.expect("append is a no-op");
		assert!(!dir.exists());
		fs::remove_dir_all(dir.parent().expect("parent")).ok();
	}

	#[test]
	fn unreadable_directory_is_a_warning_not_a_failure() {
		let dir = temp_dir();
		let blocker = dir.join("blocker");
		fs::write(&blocker, "not a directory").expect("write blocker");
		let mut store = SessionStore::open(&SessionOptions {
			dir: Some(blocker.clone()),
			..options(dir.clone(), Some("s1"))
		});
		assert!(!store.enabled());
		assert!(!store.take_warnings().is_empty());
		store
			.append_turn(&turn("hi", "there"))
			.expect("append is a no-op");
		fs::remove_dir_all(dir).ok();
	}

	#[test]
	fn builder_captures_tools_and_final_text() {
		let mut builder = TurnBuilder::new("fix the build");
		builder.observe(&Event::MessageStart { role: "assistant".to_string() });
		builder.observe(&Event::ToolStart {
			id: "call-1".to_string(),
			tool: "read".to_string(),
			args: json!({"path": "a.txt"}),
		});
		builder.observe(&Event::ToolEnd {
			id: "call-1".to_string(),
			tool: "read".to_string(),
			result: "contents".to_string(),
			error: None,
		});
		builder.observe(&Event::message_end(pi_agent::Message::assistant("done")));
		builder.observe(&Event::turn_end(
			pi_agent::Message::assistant("done"),
			Vec::new(),
			"stop",
		));
		builder.observe(&Event::agent_end(vec![pi_agent::Message::assistant("done")]));
		let record = builder.finish();
		assert_eq!(record.prompt, "fix the build");
		assert_eq!(record.assistant, "done");
		assert_eq!(record.stop_reason, "stop");
		assert_eq!(record.tool_calls.len(), 1);
		assert_eq!(record.tool_calls[0].result, "contents");
		assert!(!record.tool_calls[0].is_error);
	}

	#[test]
	fn builder_distinguishes_empty_tool_result_from_pending_interrupted() {
		let mut builder = TurnBuilder::new("test");
		// Call 1: finishes with a legitimate empty result.
		builder.observe(&Event::ToolStart {
			id: "call-1".to_string(),
			tool: "write".to_string(),
			args: json!({"path": "a.txt"}),
		});
		builder.observe(&Event::ToolEnd {
			id: "call-1".to_string(),
			tool: "write".to_string(),
			result: String::new(),
			error: None,
		});
		// Call 2: started but never received ToolEnd (interrupted).
		builder.observe(&Event::ToolStart {
			id: "call-2".to_string(),
			tool: "bash".to_string(),
			args: json!({"command": "sleep 10"}),
		});
		builder.observe(&Event::turn_end(
			pi_agent::Message::assistant(""),
			Vec::new(),
			"aborted",
		));
		let record = builder.finish();
		assert_eq!(record.tool_calls.len(), 2);
		// Completed call keeps its legitimate empty result.
		assert_eq!(record.tool_calls[0].result, "");
		assert!(!record.tool_calls[0].is_error);
		// Pending call keeps a paired interrupted error result (step1 invariant).
		assert_eq!(record.tool_calls[1].result, "[Tool execution interrupted by user]");
		assert!(record.tool_calls[1].is_error);
	}
	#[test]
	fn append_lines_rollback_on_write_failure_preserves_original_bytes_and_memory() {
		let dir = temp_dir();
		let mut store = SessionStore::open(&options(dir.clone(), Some("rollback-w")));
		assert!(store.enabled());
		let file = store.file().expect("file").to_path_buf();

		store.append_turn(&turn("turn1", "answer1")).expect("turn1 ok");
		let bytes_before = fs::read(&file).expect("read before");
		let history_before = store.history().to_vec();
		let messages_before = store.messages().to_vec();

		set_fault_injection(Some(FaultInjection::FailAppendAfterLines(1)));
		let res = store.append_turn(&turn("turn2", "answer2"));
		set_fault_injection(None);

		assert!(res.is_err());
		let bytes_after = fs::read(&file).expect("read after");
		assert_eq!(bytes_after, bytes_before, "disk bytes must match pre-call bytes bit-for-bit");
		assert_eq!(store.history(), history_before, "in-memory history must not mutate on failure");
		assert_eq!(store.messages(), messages_before, "in-memory messages must not mutate on failure");
		assert_eq!(store.history().len(), 1, "history length must stay at 1");
		assert_eq!(store.history()[0].prompt, "turn1");
		assert_eq!(store.history()[0].assistant, "answer1");

		let reopened = SessionStore::open(&options(dir.clone(), Some("rollback-w")));
		assert!(reopened.enabled());
		assert_eq!(reopened.history().to_vec(), history_before);
		fs::remove_dir_all(dir).ok();
	}

	#[test]
	fn append_lines_rollback_on_flush_failure_preserves_original_bytes_and_memory() {
		let dir = temp_dir();
		let mut store = SessionStore::open(&options(dir.clone(), Some("rollback-f")));
		assert!(store.enabled());
		let file = store.file().expect("file").to_path_buf();

		store.append_turn(&turn("turn1", "answer1")).expect("turn1 ok");
		let bytes_before = fs::read(&file).expect("read before");
		let history_before = store.history().to_vec();
		let messages_before = store.messages().to_vec();

		set_fault_injection(Some(FaultInjection::FailAppendFlush));
		let res = store.append_turn(&turn("turn2", "answer2"));
		set_fault_injection(None);

		assert!(res.is_err());
		let bytes_after = fs::read(&file).expect("read after");
		assert_eq!(bytes_after, bytes_before, "disk bytes must match pre-call bytes bit-for-bit");
		assert_eq!(store.history(), history_before, "in-memory history must not mutate on failure");
		assert_eq!(store.messages(), messages_before, "in-memory messages must not mutate on failure");
		assert_eq!(store.history().len(), 1, "history length must stay at 1");
		assert_eq!(store.history()[0].prompt, "turn1");
		assert_eq!(store.history()[0].assistant, "answer1");

		let reopened = SessionStore::open(&options(dir.clone(), Some("rollback-f")));
		assert!(reopened.enabled());
		assert_eq!(reopened.history().to_vec(), history_before);
		fs::remove_dir_all(dir).ok();
	}

	#[test]
	fn append_turn_rollback_on_title_slot_flush_failure_preserves_original_bytes_and_memory() {
		let dir = temp_dir();
		let mut store = SessionStore::open(&options(dir.clone(), Some("title-flush-fail")));
		assert!(store.enabled());
		let file = store.file().expect("file").to_path_buf();
		let bytes_before = fs::read(&file).expect("read before");
		let history_before = store.history().to_vec();
		let messages_before = store.messages().to_vec();

		set_fault_injection(Some(FaultInjection::FailTitleSlotFlush));
		let res = store.append_turn(&turn("first prompt auto titles", "answer"));
		set_fault_injection(None);

		assert!(res.is_err(), "append_turn must fail when title slot flush fails");
		let bytes_after = fs::read(&file).expect("read after");
		assert_eq!(bytes_after, bytes_before, "disk bytes must match pre-call bytes bit-for-bit (no committed entries!)");
		assert_eq!(store.history(), history_before, "in-memory history must remain empty");
		assert_eq!(store.messages(), messages_before, "in-memory messages must remain empty");
		assert!(store.header().expect("header").title.is_none(), "header title must remain None");

		let reopened = SessionStore::open(&options(dir.clone(), Some("title-flush-fail")));
		assert!(reopened.enabled());
		assert!(reopened.history().is_empty(), "reopened store must have zero committed turns");
		assert!(reopened.header().expect("header").title.is_none());
		fs::remove_dir_all(dir).ok();
	}

	#[test]
	fn overwrite_title_slot_rollback_preserves_exact_256_bytes_on_short_write() {
		let dir = temp_dir();
		let file = dir.join("test.jsonl");
		let initial_slot = serialize_title_slot("Initial Title", Some("auto"), "2026-10-26T12:00:00.000Z");
		let header_line = r#"{"type":"session","version":3,"id":"s1","timestamp":"2026-10-26T12:00:00.000Z","cwd":"/work"}"#;
		let mut init = Vec::new();
		init.extend_from_slice(initial_slot.as_bytes());
		init.extend_from_slice(format!("{header_line}\n").as_bytes());
		fs::write(&file, &init).expect("write init");

		let new_slot = serialize_title_slot("Short Write Title", Some("user"), "2026-10-26T13:00:00.000Z");
		set_fault_injection(Some(FaultInjection::FailTitleSlotWriteAfterBytes(50)));
		let res = overwrite_title_slot_rolled_back(&file, &new_slot);
		set_fault_injection(None);

		assert!(res.is_err());
		let bytes_after = fs::read(&file).expect("read after");
		assert_eq!(bytes_after, init, "exact 256 bytes must be restored on short write failure");
		let content = fs::read_to_string(&file).expect("read str");
		let parsed = parse_title_slot(content.lines().next().unwrap()).expect("parse");
		assert_eq!(parsed.title, "Initial Title");
		fs::remove_dir_all(dir).ok();
	}

	#[test]
	fn overwrite_title_slot_rollback_preserves_exact_256_bytes_on_flush_failure() {
		let dir = temp_dir();
		let file = dir.join("test.jsonl");
		let initial_slot = serialize_title_slot("Initial Title", Some("auto"), "2026-10-26T12:00:00.000Z");
		let header_line = r#"{"type":"session","version":3,"id":"s1","timestamp":"2026-10-26T12:00:00.000Z","cwd":"/work"}"#;
		let mut init = Vec::new();
		init.extend_from_slice(initial_slot.as_bytes());
		init.extend_from_slice(format!("{header_line}\n").as_bytes());
		fs::write(&file, &init).expect("write init");

		let new_slot = serialize_title_slot("Flush Fail Title", Some("user"), "2026-10-26T13:00:00.000Z");
		set_fault_injection(Some(FaultInjection::FailTitleSlotFlush));
		let res = overwrite_title_slot_rolled_back(&file, &new_slot);
		set_fault_injection(None);

		assert!(res.is_err());
		let bytes_after = fs::read(&file).expect("read after");
		assert_eq!(bytes_after, init, "exact 256 bytes must be restored on flush failure");
		let content = fs::read_to_string(&file).expect("read str");
		let parsed = parse_title_slot(content.lines().next().unwrap()).expect("parse");
		assert_eq!(parsed.title, "Initial Title");
		fs::remove_dir_all(dir).ok();
	}

	#[test]
	fn legacy_stem_negative_collision_and_exact_match() {
		let dir = temp_dir();
		let legacy_file = dir.join("team_alpha_beta.jsonl");
		fs::write(&legacy_file, "").expect("create legacy file");

		// 1. Searching for "alpha_beta" must NOT match "team_alpha_beta.jsonl" (no split_once collision)
		let no_match = find_session_by_id_or_stem(&dir, "alpha_beta").expect("scan");
		assert!(no_match.is_none(), "legacy team_alpha_beta.jsonl must NOT match alpha_beta");

		// 2. Exact match for "team_alpha_beta" MUST find it
		let exact = find_session_by_id_or_stem(&dir, "team_alpha_beta").expect("scan");
		assert_eq!(exact, Some(legacy_file));

		// 3. Direct timestamp validation unit assertions
		assert!(is_valid_file_safe_timestamp("2026-10-27T17-50-31-850Z"));
		assert!(is_valid_file_safe_timestamp("2026-10-27T17-50-31Z"));
		assert!(!is_valid_file_safe_timestamp("team"));
		assert!(!is_valid_file_safe_timestamp("team_alpha_beta"));
		assert!(!is_valid_file_safe_timestamp("2026-10-27T25-00-00Z"));
		assert!(!is_valid_file_safe_timestamp("2026-10-27T12-60-00Z"));
		assert!(!is_valid_file_safe_timestamp("2026-10-27T12-00-60Z"));
		assert!(!is_valid_file_safe_timestamp("2026-10-27-01T12-00-00Z"));
		assert!(!is_valid_file_safe_timestamp("2026-02-31T12-00-00Z"));
		assert!(!is_valid_file_safe_timestamp("2026-02-29T12-00-00Z"));
		assert!(is_valid_file_safe_timestamp("2024-02-29T12-00-00Z"));
		assert!(!is_valid_file_safe_timestamp("2026-04-31T12-00-00Z"));

		// 4. Exact calendar validation in parse_iso8601_ms
		assert!(parse_iso8601_ms("2026-02-31T12:00:00Z").is_none());
		assert!(parse_iso8601_ms("2026-02-29T12:00:00Z").is_none());
		assert!(parse_iso8601_ms("2024-02-29T12:00:00Z").is_some());
		assert!(parse_iso8601_ms("2026-04-31T12:00:00Z").is_none());
		assert!(parse_iso8601_ms("2026-01-31T12:00:00Z").is_some());
		fs::remove_dir_all(dir).ok();
	}

	#[test]
	fn modern_session_with_underscore_id_resolves_cleanly() {
		let dir = temp_dir();
		let file = dir.join("2026-10-27T17-50-31-850Z_alpha_beta.jsonl");
		fs::write(&file, "").expect("create session file");

		let found = find_session_by_id_or_stem(&dir, "alpha_beta").expect("scan");
		assert_eq!(found, Some(file));

		fs::remove_dir_all(dir).ok();
	}

	#[test]
	fn session_with_image_parts_persists_and_resumes_without_loss() {
		let dir = temp_dir();
		let mut store = SessionStore::open(&options(dir.clone(), Some("img-session")));
		assert!(store.enabled());

		let image_data = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";
		let turn1 = TurnBuilder::new_with_images(
			"look at this chart",
			vec![pi_agent::ImagePart::new("image/png", image_data)],
		)
		.record;
		let turn1 = TurnRecord {
			assistant: "I see a trend line".to_string(),
			stop_reason: "stop".to_string(),
			..turn1
		};

		store.append_turn(&turn1).expect("append turn with image");
		drop(store);

		// Reopen / resume session
		let mut reopened = SessionStore::open(&options(dir.clone(), Some("img-session")));
		assert!(reopened.enabled());
		assert_eq!(reopened.history().len(), 1);
		let resumed_turn = &reopened.history()[0];
		assert_eq!(resumed_turn.prompt, "look at this chart");
		assert_eq!(resumed_turn.assistant, "I see a trend line");
		assert_eq!(resumed_turn.images().len(), 1);
		assert_eq!(resumed_turn.images()[0].mime_type, "image/png");
		assert_eq!(resumed_turn.images()[0].data_base64, image_data);

		// Append a second turn with another image
		let turn2 = TurnBuilder::new_with_images(
			"and this one",
			vec![pi_agent::ImagePart::new("image/jpeg", "ZGF0YTI=")],
		)
		.record;
		let turn2 = TurnRecord {
			assistant: "all good".to_string(),
			stop_reason: "stop".to_string(),
			..turn2
		};
		reopened.append_turn(&turn2).expect("append turn 2");
		drop(reopened);

		// Reopen again and check both turns
		let second_resumed = SessionStore::open(&options(dir.clone(), Some("img-session")));
		assert_eq!(second_resumed.history().len(), 2);
		assert_eq!(second_resumed.history()[0].images().len(), 1);
		assert_eq!(second_resumed.history()[0].images()[0].mime_type, "image/png");
		assert_eq!(second_resumed.history()[1].images().len(), 1);
		assert_eq!(second_resumed.history()[1].images()[0].mime_type, "image/jpeg");
		assert_eq!(second_resumed.history()[1].images()[0].data_base64, "ZGF0YTI=");

		fs::remove_dir_all(dir).ok();
	}

	#[test]
	fn text_only_session_preserves_plain_text_wire_format() {
		let dir = temp_dir();
		let mut store = SessionStore::open(&options(dir.clone(), Some("text-only-session")));
		assert!(store.enabled());

		let turn1 = TurnRecord {
			prompt: "hello plain text".to_string(),
			assistant: "hello human".to_string(),
			stop_reason: "stop".to_string(),
			tool_calls: vec![],
			parts: vec![],
		};
		store.append_turn(&turn1).expect("append turn");
		let file = store.file().expect("file").to_path_buf();
		drop(store);

		let content = fs::read_to_string(&file).expect("read file");
		// Ensure user message content is string, not array
		let user_line = content
			.lines()
			.find(|l| l.contains(r#""role":"user""#))
			.expect("user line");
		let user_json: serde_json::Value = serde_json::from_str(user_line).expect("parse user json");
		assert_eq!(
			user_json["message"]["content"],
			"hello plain text",
			"plain text prompt must be serialized as string content, not array"
		);

		// Reopen and check history
		let reopened = SessionStore::open(&options(dir.clone(), Some("text-only-session")));
		assert_eq!(reopened.history().len(), 1);
		assert_eq!(reopened.history()[0].prompt, "hello plain text");
		assert!(reopened.history()[0].parts.is_empty());
		assert!(reopened.history()[0].images().is_empty());

		fs::remove_dir_all(dir).ok();
	}

	#[test]
	fn session_preserves_ordered_parts_and_data_uri_roundtrip_to_openai() {
		let dir = temp_dir();
		let mut store = SessionStore::open(&options(dir.clone(), Some("ordered-session")));
		assert!(store.enabled());

		// Input with raw data URI directly in ContentPart (not through constructor), text, and raw base64
		let parts = vec![
			ContentPart {
				kind: "image".to_string(),
				text: None,
				data: Some("data:image/png;base64,aW1nMQ==".to_string()),
				mime_type: Some("image/png".to_string()),
				extra: serde_json::Map::new(),
			},
			ContentPart::text("middle description"),
			ContentPart::image("aW1nMg==", "image/jpeg"),
		];
		let turn = TurnRecord {
			prompt: "unused-when-parts-present".to_string(),
			assistant: "done".to_string(),
			stop_reason: "stop".to_string(),
			tool_calls: vec![],
			parts,
		};

		store.append_turn(&turn).expect("append ordered turn");
		let file = store.file().expect("file").to_path_buf();
		drop(store);

		// 1. Verify JSONL on disk: data field must contain raw base64, NEVER full data: URI
		let content = fs::read_to_string(&file).expect("read file");
		let user_line = content
			.lines()
			.find(|l| l.contains(r#""role":"user""#))
			.expect("user line");
		let user_json: serde_json::Value = serde_json::from_str(user_line).expect("parse user line");
		let disk_parts = user_json["message"]["content"].as_array().expect("parts array");
		assert_eq!(disk_parts.len(), 3);
		assert_eq!(disk_parts[0]["type"], "image");
		assert_eq!(
			disk_parts[0]["data"],
			"aW1nMQ==",
			"TS session log must store raw base64, NOT data: URI"
		);
		assert_eq!(disk_parts[0]["mimeType"], "image/png");
		assert_eq!(disk_parts[1]["type"], "text");
		assert_eq!(disk_parts[1]["text"], "middle description");
		assert_eq!(disk_parts[2]["type"], "image");
		assert_eq!(disk_parts[2]["data"], "aW1nMg==");
		assert_eq!(disk_parts[2]["mimeType"], "image/jpeg");

		// 2. Reopen / resume session and verify in-memory ordering
		let reopened = SessionStore::open(&options(dir.clone(), Some("ordered-session")));
		let hist = reopened.history();
		assert_eq!(hist.len(), 1);
		assert_eq!(hist[0].parts.len(), 3);
		assert_eq!(hist[0].parts[0].data.as_deref(), Some("aW1nMQ=="));
		assert_eq!(hist[0].parts[1].text.as_deref(), Some("middle description"));
		assert_eq!(hist[0].parts[2].data.as_deref(), Some("aW1nMg=="));

		// 3. Reconstruct pi_agent message history via REAL runtime::history_messages
		let pi_messages = crate::runtime::history_messages(&reopened.history()).expect("history messages");
		assert_eq!(pi_messages.len(), 2);
		assert_eq!(pi_messages[0].parts.len(), 3);
		let openai_msgs = pi_agent::to_openai_messages(&pi_messages).expect("to openai messages");
		let wire_val = serde_json::to_value(&openai_msgs[0]).expect("serialize wire message");
		let wire_parts = wire_val["content"].as_array().expect("wire content array");
		assert_eq!(wire_parts.len(), 3);
		assert_eq!(wire_parts[0]["type"], "image_url");
		assert_eq!(wire_parts[0]["image_url"]["url"], "data:image/png;base64,aW1nMQ==");
		assert_eq!(wire_parts[1]["type"], "text");
		assert_eq!(wire_parts[1]["text"], "middle description");
		assert_eq!(wire_parts[2]["type"], "image_url");
		assert_eq!(wire_parts[2]["image_url"]["url"], "data:image/jpeg;base64,aW1nMg==");

		fs::remove_dir_all(dir).ok();
	}

	#[test]
	fn append_turn_rejects_malformed_image_and_preserves_session_unmodified() {
		let dir = temp_dir();
		let mut store = SessionStore::open(&options(dir.clone(), Some("safe-reject-session")));
		assert!(store.enabled());

		let initial_turn = TurnRecord {
			prompt: "first prompt".to_string(),
			assistant: "first reply".to_string(),
			stop_reason: "stop".to_string(),
			tool_calls: vec![],
			parts: vec![],
		};
		store.append_turn(&initial_turn).expect("append initial");
		let file = store.file().expect("file").to_path_buf();
		let size_before = fs::metadata(&file).expect("metadata").len();

		// Attempt to append turn with an external URL image
		let bad_turn = TurnRecord {
			prompt: "bad".to_string(),
			assistant: "no".to_string(),
			stop_reason: "stop".to_string(),
			tool_calls: vec![],
			parts: vec![ContentPart {
				kind: "image".to_string(),
				text: None,
				data: Some("http://attacker.com/leak.png".to_string()),
				mime_type: Some("image/png".to_string()),
				extra: serde_json::Map::new(),
			}],
		};
		let res = store.append_turn(&bad_turn);
		assert!(res.is_err(), "append_turn must fail before mutation on invalid image");
		let size_after = fs::metadata(&file).expect("metadata").len();
		assert_eq!(size_before, size_after, "session file must remain unmodified on rejected turn");
		assert_eq!(store.history().len(), 1, "in-memory history must not mutate on failure");

		fs::remove_dir_all(dir).ok();
	}

	#[test]
	fn append_turn_rejects_unsupported_part_kind_and_preserves_session_unmodified() {
		let dir = temp_dir();
		let mut store = SessionStore::open(&options(dir.clone(), Some("safe-reject-kind-session")));
		assert!(store.enabled());

		let bad_turn = TurnRecord {
			prompt: "bad".to_string(),
			assistant: "no".to_string(),
			stop_reason: "stop".to_string(),
			tool_calls: vec![],
			parts: vec![ContentPart {
				kind: "audio".to_string(),
				text: None,
				data: Some("audio-stream".to_string()),
				mime_type: Some("audio/mp3".to_string()),
				extra: serde_json::Map::new(),
			}],
		};
		let res = store.append_turn(&bad_turn);
		let err = res.unwrap_err();
		assert!(err.contains("unsupported content part kind at index 0"), "got: {err}");
		assert_eq!(store.history().len(), 0, "session must remain empty on rejected turn");

		fs::remove_dir_all(dir).ok();
	}

	#[test]
	fn unknown_content_shape_in_session_log_disables_persistence_without_data_loss_or_write() {
		let dir = temp_dir();
		let file = dir.join("2026-10-27T10-00-00-000Z_test-unknown-shape.jsonl");
		let initial_log = concat!(
			r#"{"type":"session","version":3,"id":"test-unknown-shape","timestamp":"2026-10-27T10:00:00.000Z","cwd":"/","model":"m","provider":"p"}"#,
			"\n",
			r#"{"type":"message","id":"u1","parentId":null,"timestamp":"2026-10-27T10:00:01.000Z","message":{"role":"user","content":{"unsupported_custom_block":{"nested":"value"}}}}"#,
			"\n"
		);
		fs::write(&file, initial_log).expect("write initial log");
		let size_before = fs::metadata(&file).expect("metadata").len();

		let mut store = SessionStore::open(&options(dir.clone(), Some("test-unknown-shape")));
		assert!(!store.enabled(), "store must be disabled when unknown content shape is present");
		let reason = store.corrupt_reason().expect("corrupt_reason must be populated");
		assert!(
			reason.contains("unsupported or malformed raw message content shape"),
			"corrupt reason must report unsupported content shape, got: {reason}"
		);

		let dummy_turn = TurnRecord {
			prompt: "hello".to_string(),
			assistant: "world".to_string(),
			stop_reason: "stop".to_string(),
			tool_calls: vec![],
			parts: vec![],
		};
		let res = store.append_turn(&dummy_turn);
		assert!(res.is_err(), "append_turn must fail on disabled/corrupted session");

		let file_content_after = fs::read_to_string(&file).expect("read file after");
		assert_eq!(file_content_after, initial_log, "file content must remain 100% intact with no loss or write");
		let size_after = fs::metadata(&file).expect("metadata").len();
		assert_eq!(size_before, size_after, "file size must remain unchanged");

		fs::remove_dir_all(dir).ok();
	}

	#[test]
	fn direct_append_message_with_unknown_part_aborts_without_file_or_memory_mutation() {
		let dir = temp_dir();
		let mut store = SessionStore::open(&options(dir.clone(), Some("preflight-test-session")));
		assert!(store.enabled());

		// Append a valid initial turn to establish baseline state
		let initial_turn = TurnRecord {
			prompt: "hello".to_string(),
			assistant: "world".to_string(),
			stop_reason: "stop".to_string(),
			tool_calls: vec![],
			parts: vec![],
		};
		store.append_turn(&initial_turn).expect("append initial turn");
		let file = store.file().expect("file").to_path_buf();

		let file_content_before = fs::read_to_string(&file).expect("read file");
		let file_size_before = fs::metadata(&file).expect("metadata").len();
		let messages_len_before = store.messages.len();
		let entries_len_before = store.entries.len();
		let history_len_before = store.history().len();

		// Construct direct MessageEntry with an unknown part kind
		let bad_entry = MessageEntry::new(
			"msg-bad".to_string(),
			store.messages.last().map(|m| m.id.clone()),
			output::iso8601(SystemTime::now()),
			SessionMessage::user(
				MessageContent::Parts(vec![ContentPart {
					kind: "unknown_custom_extension".to_string(),
					text: None,
					data: None,
					mime_type: None,
					extra: serde_json::Map::new(),
				}]),
				12345,
			),
		);

		let res = store.append_message(&bad_entry);
		assert!(res.is_err(), "append_message must fail before mutation on unknown part");

		// Verify exact equality of file and in-memory state
		let file_content_after = fs::read_to_string(&file).expect("read file after");
		assert_eq!(file_content_before, file_content_after, "session file must remain 100% identical");
		let file_size_after = fs::metadata(&file).expect("metadata after").len();
		assert_eq!(file_size_before, file_size_after, "session file size must not change");
		assert_eq!(store.messages.len(), messages_len_before, "messages must not mutate");
		assert_eq!(store.entries.len(), entries_len_before, "entries must not mutate");
		assert_eq!(store.history().len(), history_len_before, "history must not mutate");

		fs::remove_dir_all(dir).ok();
	}

	#[test]
	fn session_preserves_tool_result_image_parts_and_roundtrips() {
		let dir = temp_dir();
		let mut store = SessionStore::open(&options(dir.clone(), Some("tool-image-roundtrip")));
		assert!(store.enabled());

		let turn_record = TurnRecord {
			prompt: "take picture".to_string(),
			assistant: "calling camera".to_string(),
			stop_reason: "stop".to_string(),
			tool_calls: vec![ToolCallRecord {
				id: "call-cam-1".to_string(),
				name: "camera".to_string(),
				arguments: json!({}),
				result: "snapshot".to_string(),
				is_error: false,
				result_parts: vec![
					ContentPart::text("snapshot"),
					ContentPart::image("aW1nMQ==", "image/png"),
				],
				details: None,
			}],
			parts: vec![],
		};
		store.append_turn(&turn_record).expect("append turn with tool image");

		// 1. Verify JSONL content on disk
		let file = store.file().expect("file").to_path_buf();
		let file_content = fs::read_to_string(&file).expect("read file");
		assert!(file_content.contains(r#""role":"toolResult""#));
		assert!(file_content.contains(r#""mimeType":"image/png""#));
		assert!(file_content.contains(r#""data":"aW1nMQ==""#));

		// 2. Reopen session and check history
		let reopened = SessionStore::open(&options(dir.clone(), Some("tool-image-roundtrip")));
		assert!(reopened.enabled());
		let history = reopened.history();
		assert_eq!(history.len(), 1);
		assert_eq!(history[0].tool_calls.len(), 1);
		assert_eq!(history[0].tool_calls[0].result_parts.len(), 2);
		assert_eq!(history[0].tool_calls[0].result, "snapshot");
		assert_eq!(history[0].tool_calls[0].result_parts[0].text.as_deref(), Some("snapshot"));
		assert_eq!(history[0].tool_calls[0].result_parts[1].data.as_deref(), Some("aW1nMQ=="));

		// 3. Replay through history_messages and to_openai_messages
		let pi_messages = crate::runtime::history_messages(&history).expect("history messages");
		assert_eq!(pi_messages.len(), 3);
		assert_eq!(pi_messages[2].role, pi_agent::Role::Tool);
		assert_eq!(pi_messages[2].parts.len(), 2);

		let wire = pi_agent::to_openai_messages(&pi_messages).expect("to wire");
		assert_eq!(wire.len(), 4);
		assert_eq!(wire[2].role, "tool");
		assert_eq!(wire[2].content, Some(pi_agent::OpenAiContent::Text("snapshot".to_string())));

		assert_eq!(wire[3].role, "user");
		if let Some(pi_agent::OpenAiContent::Parts(parts)) = &wire[3].content {
			assert_eq!(parts.len(), 2);
			assert_eq!(parts[0], pi_agent::OpenAiContentPart::Text {
				text: "Attached image(s) from tool result:".to_string(),
			});
			assert_eq!(parts[1], pi_agent::OpenAiContentPart::ImageUrl {
				image_url: pi_agent::OpenAiImageUrl {
					url: "data:image/png;base64,aW1nMQ==".to_string(),
				},
			});
		} else {
			panic!("expected OpenAiContent::Parts");
		}

		fs::remove_dir_all(dir).ok();
	}

	#[test]
	fn append_turn_rejects_malformed_tool_result_image_and_preserves_session() {
		let dir = temp_dir();
		let mut store = SessionStore::open(&options(dir.clone(), Some("tool-image-bad")));
		assert!(store.enabled());

		let bad_turn = TurnRecord {
			prompt: "take picture".to_string(),
			assistant: "calling camera".to_string(),
			stop_reason: "stop".to_string(),
			tool_calls: vec![ToolCallRecord {
				id: "call-cam-1".to_string(),
				name: "camera".to_string(),
				arguments: json!({}),
				result: "snapshot".to_string(),
				is_error: false,
				result_parts: vec![
					ContentPart {
						kind: "image".to_string(),
						text: None,
						data: Some("http://attacker.com/evil.png".to_string()),
						mime_type: Some("image/png".to_string()),
						extra: serde_json::Map::new(),
					},
				],
				details: None,
			}],
			parts: vec![],
		};
		let res = store.append_turn(&bad_turn);
		assert!(res.is_err(), "external URL image in tool result must be rejected");
		assert_eq!(store.history().len(), 0);

		fs::remove_dir_all(dir).ok();
	}

	#[test]
	fn tool_details_survive_turn_builder_append_and_resume() {
		let details = json!({
			"op": "init",
			"phases": [{"name": "Plan", "tasks": [{"content": "write code", "status": "pending"}]}],
			"storage": "session",
		});
		let mut builder = TurnBuilder::new("build");
		builder.observe(&Event::ToolStart {
			id: "call-todo".to_string(),
			tool: "todo".to_string(),
			args: json!({"op": "init"}),
		});
		let mut asst = pi_agent::Message::assistant("");
		asst.tool_calls.push(pi_agent::ToolCall {
			id: "call-todo".to_string(),
			name: "todo".to_string(),
			arguments: json!({"op": "init"}),
		});
		builder.observe(&Event::ToolEnd {
			id: "call-todo".to_string(),
			tool: "todo".to_string(),
			result: "plan written".to_string(),
			error: None,
		});
		builder.observe(&Event::turn_end(
			asst,
			vec![pi_agent::ToolResult {
				tool_call_id: "call-todo".to_string(),
				content: "plan written".to_string(),
				is_error: false,
				terminate: false,
				details: Some(details.clone()),
			}],
			"toolUse",
		));
		let record = builder.finish();
		assert_eq!(record.tool_calls[0].details.as_ref(), Some(&details));

		let dir = temp_dir();
		let mut store = SessionStore::open(&options(dir.clone(), Some("todo-details")));
		store.append_turn(&record).expect("append turn");
		let file = store.file().expect("session file").to_path_buf();

		let reopened = SessionStore::open(&options(dir.clone(), Some("todo-details")));
		assert!(reopened.enabled());
		assert_eq!(reopened.history().len(), 1);
		assert_eq!(reopened.history()[0].tool_calls[0].details.as_ref(), Some(&details));

		let raw = fs::read_to_string(&file).expect("read session log");
		assert!(
			raw.contains(r#""details":{"op":"init""#),
			"tool details must be written into the toolResult session entry: {raw}"
		);

		fs::remove_dir_all(dir).ok();
	}

	#[test]
	fn latest_todo_phases_honors_custom_entries_in_log_order() {
		fn todo_turn(prompt: &str, phases: serde_json::Value, is_error: bool) -> TurnRecord {
			TurnRecord {
				prompt: prompt.to_string(),
				assistant: "updated todos".to_string(),
				stop_reason: "toolUse".to_string(),
				tool_calls: vec![ToolCallRecord {
					id: "call-todo".to_string(),
					name: "todo".to_string(),
					arguments: json!({"op": "init"}),
					result: "todos updated".to_string(),
					is_error,
					result_parts: Vec::new(),
					details: Some(json!({"op": "init", "phases": phases, "storage": "session"})),
				}],
				parts: Vec::new(),
			}
		}
		fn custom_entry(data: serde_json::Value) -> SessionEntry {
			SessionEntry {
				kind: "custom".to_string(),
				id: format!("msg-{}", output::session_id()),
				parent_id: None,
				timestamp: output::iso8601(SystemTime::now()),
				extra: json!({"customType": USER_TODO_EDIT_CUSTOM_TYPE, "data": data}),
			}
		}

		let dir = temp_dir();
		let mut store = SessionStore::open(&options(dir.clone(), Some("todo-custom")));
		store
			.append_turn(&todo_turn(
				"seed",
				json!([{"name": "Plan", "tasks": [{"content": "from tool", "status": "in_progress"}]}]),
				false,
			))
			.expect("append seed turn");
		assert_eq!(store.latest_todo_phases()[0].tasks[0].content, "from tool");

		// A later user edit wins by log order; an empty list is a real clear.
		store
			.append_entry(custom_entry(json!({"phases": []})))
			.expect("append clear edit");
		assert!(store.latest_todo_phases().is_empty());

		// A malformed custom payload is skipped, not treated as a clear.
		store.append_entry(custom_entry(json!({}))).expect("append malformed edit");
		assert!(store.latest_todo_phases().is_empty());

		// A tool result appended after the edit takes over again.
		store
			.append_turn(&todo_turn(
				"later",
				json!([{"name": "Plan", "tasks": [{"content": "from later tool", "status": "pending"}]}]),
				false,
			))
			.expect("append later turn");
		assert_eq!(store.latest_todo_phases()[0].tasks[0].content, "from later tool");

		// A failed tool result echoes the pre-call state; it never rolls the
		// list back or forward.
		store
			.append_turn(&todo_turn(
				"failed",
				json!([{"name": "Plan", "tasks": [{"content": "stale echo", "status": "pending"}]}]),
				true,
			))
			.expect("append failed turn");
		assert_eq!(store.latest_todo_phases()[0].tasks[0].content, "from later tool");

		let file = store.file().expect("session file").to_path_buf();
		let resumed = SessionStore::open(&options(dir.clone(), Some("todo-custom")));
		assert_eq!(resumed.latest_todo_phases()[0].tasks[0].content, "from later tool");

		// Fork copies the branch verbatim, so the restored todo state matches.
		let fork_dir = temp_dir();
		let fork_file = fork_session(&file, None, Some(&fork_dir)).expect("fork session");
		let fork_stem = fork_file
			.file_stem()
			.and_then(std::ffi::OsStr::to_str)
			.expect("fork stem")
			.to_string();
		let forked = SessionStore::open(&options(fork_dir.clone(), Some(&fork_stem)));
		assert!(forked.enabled(), "forked session must open: {:?}", forked.corrupt_reason());
		assert_eq!(forked.latest_todo_phases()[0].tasks[0].content, "from later tool");

		fs::remove_dir_all(dir).ok();
		fs::remove_dir_all(fork_dir).ok();
	}
}
