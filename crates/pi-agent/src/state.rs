use std::{collections::HashMap, path::PathBuf, sync::Arc};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
/// Role of a message in context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
	System,
	User,
	Assistant,
	Tool,
}

/// A tool invocation request by model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
	pub id: String,
	pub name: String,
	pub arguments: serde_json::Value,
}

/// A tool execution response.
///
/// Tools fill `content`/`is_error` and leave `tool_call_id` empty; the scheduler
/// stamps the id of the call it ran, so a tool cannot mislabel its own result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResult {
	pub tool_call_id: String,
	pub content: String,
	#[serde(default)]
	pub is_error: bool,
	#[serde(default)]
	pub terminate: bool,
	/// Structured result payload (TS `details`). Persisted with the session so
	/// tools such as `todo` can restore their state; never sent to the model.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub details: Option<serde_json::Value>,
}

/// Completion status of an assistant turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
	EndTurn,
	ToolUse,
	Aborted,
	Error(String),
}

/// Validate supported image MIME formats without external dependencies.
pub fn is_valid_image_mime(mime: &str) -> bool {
	matches!(
		mime.to_ascii_lowercase().as_str(),
		"image/png" | "image/jpeg" | "image/jpg" | "image/webp" | "image/gif"
	)
}

#[inline]
fn b64_val(b: u8) -> Option<u8> {
	match b {
		b'A'..=b'Z' => Some(b - b'A'),
		b'a'..=b'z' => Some(b - b'a' + 26),
		b'0'..=b'9' => Some(b - b'0' + 52),
		b'+' => Some(62),
		b'/' => Some(63),
		_ => None,
	}
}

/// Strict validation of base64 payload without external dependencies.
/// Validates standard alphabet, length, padding structure, and canonical zero pad bits (RFC 4648).
pub fn is_valid_base64_payload(payload: &str) -> bool {
	let bytes = payload.as_bytes();
	if bytes.is_empty() {
		return false;
	}

	let pad_count = if bytes.ends_with(b"==") {
		2
	} else if bytes.ends_with(b"=") {
		1
	} else {
		0
	};

	if pad_count > 0 {
		if bytes.len() % 4 != 0 {
			return false;
		}
		let data_part = &bytes[..bytes.len() - pad_count];
		if data_part.is_empty() {
			return false;
		}
		for &b in data_part {
			if b64_val(b).is_none() {
				return false;
			}
		}
		let last_val = b64_val(*data_part.last().unwrap()).unwrap();
		if pad_count == 1 && (last_val & 0x03 != 0) {
			return false;
		}
		if pad_count == 2 && (last_val & 0x0F != 0) {
			return false;
		}
		true
	} else {
		let rem = bytes.len() % 4;
		if rem == 1 {
			return false;
		}
		for &b in bytes {
			if b64_val(b).is_none() {
				return false;
			}
		}
		if rem == 2 {
			let last_val = b64_val(*bytes.last().unwrap()).unwrap();
			if last_val & 0x0F != 0 {
				return false;
			}
		} else if rem == 3 {
			let last_val = b64_val(*bytes.last().unwrap()).unwrap();
			if last_val & 0x03 != 0 {
				return false;
			}
		}
		true
	}
}

/// Normalized validation and extraction for image payloads.
/// Validates MIME, data URI structure, base64 payload, and prevents external URLs.
/// Returns (clean_base64, canonical_mime).
pub fn normalize_image_payload(data: &str, declared_mime: &str) -> Result<(String, String), String> {
	let data = data.trim();
	let declared_mime = declared_mime.trim();

	if data.starts_with("http://")
		|| data.starts_with("https://")
		|| data.starts_with("//")
		|| data.contains("://")
	{
		return Err("external URLs are not permitted; provide local base64 or data URI".to_string());
	}

	if let Some(rest) = data.strip_prefix("data:") {
		let Some((meta, payload)) = rest.split_once(',') else {
			return Err("malformed data URI: missing comma separator".to_string());
		};
		let Some((uri_mime, encoding)) = meta.split_once(';') else {
			return Err("malformed data URI: missing ;base64 metadata".to_string());
		};
		let uri_mime = uri_mime.trim();
		if !is_valid_image_mime(uri_mime) {
			return Err("invalid image MIME in data URI".to_string());
		}
		if !encoding.trim().eq_ignore_ascii_case("base64") {
			return Err("unsupported data URI encoding; expected base64".to_string());
		}
		if !declared_mime.is_empty() && !declared_mime.eq_ignore_ascii_case(uri_mime) {
			return Err("MIME type mismatch: declared MIME does not match data URI".to_string());
		}
		let payload = payload.trim();
		if !is_valid_base64_payload(payload) {
			return Err("invalid base64 payload in data URI".to_string());
		}
		Ok((payload.to_string(), uri_mime.to_string()))
	} else {
		if !is_valid_image_mime(declared_mime) {
			return Err("invalid image MIME type".to_string());
		}
		if !is_valid_base64_payload(data) {
			return Err("invalid base64 payload".to_string());
		}
		Ok((data.to_string(), declared_mime.to_string()))
	}
}

/// Typed image payload for multimodal model messages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImagePart {
	#[serde(rename = "mime_type", alias = "mimeType")]
	pub mime_type: String,
	#[serde(rename = "data_base64", alias = "data")]
	pub data_base64: String,
}

impl ImagePart {
	pub fn new(mime_type: impl Into<String>, data_base64: impl Into<String>) -> Self {
		let mime = mime_type.into();
		let data = data_base64.into();
		if let Ok((clean_b64, norm_mime)) = normalize_image_payload(&data, &mime) {
			Self {
				mime_type: norm_mime,
				data_base64: clean_b64,
			}
		} else {
			Self {
				mime_type: mime,
				data_base64: data,
			}
		}
	}

	pub fn validate(&self) -> Result<(), String> {
		normalize_image_payload(&self.data_base64, &self.mime_type).map(|_| ())
	}

	pub fn to_data_url(&self) -> Result<String, String> {
		let (clean_b64, mime) = normalize_image_payload(&self.data_base64, &self.mime_type)?;
		Ok(format!("data:{mime};base64,{clean_b64}"))
	}
}

/// Lossless, ordered typed content part for agent messages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContentPart {
	#[serde(rename = "text")]
	Text { text: String },
	#[serde(rename = "image")]
	Image {
		#[serde(rename = "data", alias = "data_base64")]
		data: String,
		#[serde(rename = "mimeType", alias = "mime_type")]
		mime_type: String,
	},
}

impl ContentPart {
	pub fn text(text: impl Into<String>) -> Self {
		Self::Text { text: text.into() }
	}

	pub fn image(data: impl Into<String>, mime_type: impl Into<String>) -> Result<Self, String> {
		let (clean_b64, mime) = normalize_image_payload(&data.into(), &mime_type.into())?;
		Ok(Self::Image {
			data: clean_b64,
			mime_type: mime,
		})
	}

	pub fn is_image(&self) -> bool {
		matches!(self, Self::Image { .. })
	}

	pub fn as_image(&self) -> Option<ImagePart> {
		match self {
			Self::Image { data, mime_type } => Some(ImagePart {
				mime_type: mime_type.clone(),
				data_base64: data.clone(),
			}),
			_ => None,
		}
	}
}

fn deserialize_parts<'de, D>(deserializer: D) -> Result<Vec<ContentPart>, D::Error>
where
	D: serde::Deserializer<'de>,
{
	let values = Vec::<serde_json::Value>::deserialize(deserializer)?;
	let mut parts = Vec::with_capacity(values.len());
	for (index, v) in values.into_iter().enumerate() {
		if v.get("type").is_some() {
			if let Ok(part) = serde_json::from_value::<ContentPart>(v) {
				parts.push(part);
			} else {
				return Err(serde::de::Error::custom(format!(
					"unsupported message content part at index {index}"
				)));
			}
		} else if let Ok(img) = serde_json::from_value::<ImagePart>(v) {
			parts.push(ContentPart::Image {
				data: img.data_base64,
				mime_type: img.mime_type,
			});
		} else {
			return Err(serde::de::Error::custom(format!(
				"unsupported message content part at index {index}"
			)));
		}
	}
	Ok(parts)
}

/// A single message in the agent dialog history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Message {
	pub role: Role,
	pub content: String,
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub parts: Vec<ContentPart>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub name: Option<String>,
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub tool_calls: Vec<ToolCall>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub tool_call_id: Option<String>,
	#[serde(default, skip_serializing_if = "std::ops::Not::not")]
	pub is_error: bool,
}

#[derive(Deserialize)]
struct RawMessage {
	role: Role,
	#[serde(default)]
	content: String,
	#[serde(
		default,
		deserialize_with = "deserialize_parts",
		alias = "images",
		alias = "image_parts"
	)]
	parts: Vec<ContentPart>,
	#[serde(default)]
	name: Option<String>,
	#[serde(default)]
	tool_calls: Vec<ToolCall>,
	#[serde(default)]
	tool_call_id: Option<String>,
	#[serde(default)]
	is_error: bool,
}

impl<'de> Deserialize<'de> for Message {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		let raw = RawMessage::deserialize(deserializer)?;
		let mut parts = raw.parts;
		let content = if raw.content.is_empty() {
			parts
				.iter()
				.filter_map(|p| match p {
					ContentPart::Text { text } => Some(text.as_str()),
					_ => None,
				})
				.collect::<Vec<_>>()
				.join("\n")
		} else {
			raw.content
		};

		// Normalize legacy payloads (content string + image parts only) into ordered parts
		let has_text_part = parts.iter().any(|p| matches!(p, ContentPart::Text { .. }));
		if !has_text_part && !content.is_empty() && parts.iter().any(|p| p.is_image()) {
			parts.insert(0, ContentPart::Text { text: content.clone() });
		}

		Ok(Message {
			role: raw.role,
			content,
			parts,
			name: raw.name,
			tool_calls: raw.tool_calls,
			tool_call_id: raw.tool_call_id,
			is_error: raw.is_error,
		})
	}
}
impl Message {
	pub fn system(content: impl Into<String>) -> Self {
		Self {
			role: Role::System,
			content: content.into(),
			parts: Vec::new(),
			name: None,
			tool_calls: Vec::new(),
			tool_call_id: None,
			is_error: false,
		}
	}

	pub fn user(content: impl Into<String>) -> Self {
		Self {
			role: Role::User,
			content: content.into(),
			parts: Vec::new(),
			name: None,
			tool_calls: Vec::new(),
			tool_call_id: None,
			is_error: false,
		}
	}

	pub fn user_with_parts(parts: Vec<ContentPart>) -> Self {
		let text = parts
			.iter()
			.filter_map(|p| match p {
				ContentPart::Text { text } => Some(text.as_str()),
				_ => None,
			})
			.collect::<Vec<_>>()
			.join("\n");
		Self {
			role: Role::User,
			content: text,
			parts,
			name: None,
			tool_calls: Vec::new(),
			tool_call_id: None,
			is_error: false,
		}
	}

	pub fn user_with_images(content: impl Into<String>, images: Vec<ImagePart>) -> Self {
		let prompt = content.into();
		let mut parts = Vec::new();
		if !prompt.is_empty() {
			parts.push(ContentPart::Text { text: prompt.clone() });
		}
		for img in images {
			parts.push(ContentPart::Image {
				data: img.data_base64,
				mime_type: img.mime_type,
			});
		}
		Self {
			role: Role::User,
			content: prompt,
			parts,
			name: None,
			tool_calls: Vec::new(),
			tool_call_id: None,
			is_error: false,
		}
	}

	pub fn has_image(&self) -> bool {
		self.parts.iter().any(|p| p.is_image())
	}

	pub fn image_parts(&self) -> Vec<ImagePart> {
		self.parts.iter().filter_map(|p| p.as_image()).collect()
	}

	pub fn assistant(content: impl Into<String>) -> Self {
		Self {
			role: Role::Assistant,
			content: content.into(),
			parts: Vec::new(),
			name: None,
			tool_calls: Vec::new(),
			tool_call_id: None,
			is_error: false,
		}
	}

	pub fn assistant_with_tools(content: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
		Self {
			role: Role::Assistant,
			content: content.into(),
			parts: Vec::new(),
			name: None,
			tool_calls,
			tool_call_id: None,
			is_error: false,
		}
	}

	pub fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
		Self {
			role: Role::Tool,
			content: content.into(),
			parts: Vec::new(),
			name: None,
			tool_calls: Vec::new(),
			tool_call_id: Some(tool_call_id.into()),
			is_error: false,
		}
	}

	pub fn tool_with_status(
		tool_call_id: impl Into<String>,
		name: impl Into<String>,
		content: impl Into<String>,
		is_error: bool,
	) -> Self {
		Self {
			role: Role::Tool,
			content: content.into(),
			parts: Vec::new(),
			name: Some(name.into()),
			tool_calls: Vec::new(),
			tool_call_id: Some(tool_call_id.into()),
			is_error,
		}
	}
	pub fn tool_with_status_and_parts(
		tool_call_id: impl Into<String>,
		name: impl Into<String>,
		content: impl Into<String>,
		parts: Vec<ContentPart>,
		is_error: bool,
	) -> Self {
		Self {
			role: Role::Tool,
			content: content.into(),
			parts,
			name: Some(name.into()),
			tool_calls: Vec::new(),
			tool_call_id: Some(tool_call_id.into()),
			is_error,
		}
	}
}

/// Agent configuration parameters.
///
/// Everything frequently edited (system prompt, model id, limits) is supplied by
/// the caller from config/data files, never baked into this crate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
	pub max_turns: usize,
	pub context_limit: usize,
	pub system_prompt: Option<String>,
	pub cwd: PathBuf,
}

/// Shared context accessible to tools and execution loop.
/// Tool approval gate checked prior to executing side-effecting operations.
#[async_trait]
pub trait ToolApprover: Send + Sync {
	async fn approve(&self, name: &str, args: &serde_json::Value) -> Result<(), String>;
}

/// Shared context accessible to tools and execution loop.
#[derive(Clone)]
pub struct AgentContext {
	pub cwd: PathBuf,
	pub env: HashMap<String, String>,
	pub cancellation_token: CancellationToken,
	pub approver: Option<Arc<dyn ToolApprover>>,
}

impl std::fmt::Debug for AgentContext {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("AgentContext")
			.field("cwd", &self.cwd)
			.field("env", &self.env)
			.field("cancellation_token", &self.cancellation_token)
			.field("has_approver", &self.approver.is_some())
			.finish()
	}
}

impl AgentContext {
	pub fn new(cwd: PathBuf) -> Self {
		Self {
			cwd,
			env: std::env::vars().collect(),
			cancellation_token: CancellationToken::new(),
			approver: None,
		}
	}

	pub fn with_cancellation(cwd: PathBuf, cancellation_token: CancellationToken) -> Self {
		Self {
			cwd,
			env: std::env::vars().collect(),
			cancellation_token,
			approver: None,
		}
	}

	pub fn with_approver(
		cwd: PathBuf,
		cancellation_token: CancellationToken,
		approver: Option<Arc<dyn ToolApprover>>,
	) -> Self {
		Self {
			cwd,
			env: std::env::vars().collect(),
			cancellation_token,
			approver,
		}
	}
}

/// Persistent state of an agent session.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentState {
	pub messages: Vec<Message>,
	pub total_turns: usize,
}

/// Result of a single turn execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnResult {
	pub message: Message,
	pub tool_results: Vec<ToolResult>,
	pub is_completed: bool,
}
