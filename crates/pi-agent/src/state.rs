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
	/// Host-injected guidance (TS `developer`): plan reminders and similar
	/// agent-authored context that is not a user turn.
	Developer,
}

/// Token usage for exactly one model request, as its provider reported it.
///
/// Mirrors the TS `Usage` token counters (`packages/catalog/src/types.ts`,
/// camelCase on the wire) after provider normalization: `input` already
/// excludes cache reads and cache writes, so the counters are disjoint and
/// `total_tokens == input + output + cache_read + cache_write`.
///
/// Absence of a report is `None` on the carrying [`Message`] — never a
/// zero-filled stand-in for "the provider said nothing".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
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
	/// Provider-reported reasoning/thinking tokens, a subset of `output`
	/// (OpenAI `completion_tokens_details.reasoning_tokens`). Absent when the
	/// provider reports none — `None` means unknown, not zero (TS
	/// `calculateOpenAIUsageAccounting` adds the key only when positive).
	#[serde(default, rename = "reasoningTokens", skip_serializing_if = "Option::is_none")]
	pub reasoning_tokens: Option<u64>,
}

impl TokenUsage {
	/// Sum one reported snapshot into this aggregate, saturating so a
	/// pathological provider report can never wrap. `reasoning_tokens` stays
	/// `None` only while no report carried the counter: unknown is not zero.
	pub fn merge(&mut self, usage: TokenUsage) {
		self.input = self.input.saturating_add(usage.input);
		self.output = self.output.saturating_add(usage.output);
		self.cache_read = self.cache_read.saturating_add(usage.cache_read);
		self.cache_write = self.cache_write.saturating_add(usage.cache_write);
		self.total_tokens = self.total_tokens.saturating_add(usage.total_tokens);
		self.reasoning_tokens = match (self.reasoning_tokens, usage.reasoning_tokens) {
			(Some(total), Some(reported)) => Some(total.saturating_add(reported)),
			(Some(total), None) => Some(total),
			(None, Some(reported)) => Some(reported),
			(None, None) => None,
		};
	}
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
	/// Provider-supplied public reasoning block (TS `ThinkingContent`). Never
	/// replayed outbound and never mixed into the answer text.
	#[serde(rename = "thinking")]
	Thinking { thinking: String },
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

	pub fn thinking(thinking: impl Into<String>) -> Self {
		Self::Thinking { thinking: thinking.into() }
	}

	pub fn is_thinking(&self) -> bool {
		matches!(self, Self::Thinking { .. })
	}

	pub fn as_thinking(&self) -> Option<&str> {
		match self {
			Self::Thinking { thinking } => Some(thinking),
			_ => None,
		}
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
	/// Provider-reported usage for the request that produced this assistant
	/// message. `None` on non-assistant roles and when the provider reported
	/// nothing at all.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub usage: Option<TokenUsage>,
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
	#[serde(default)]
	usage: Option<TokenUsage>,
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
			usage: raw.usage,
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
			usage: None,
		}
	}

	/// Host-injected developer message (TS `role:"developer"`).
	pub fn developer(content: impl Into<String>) -> Self {
		Self {
			role: Role::Developer,
			content: content.into(),
			parts: Vec::new(),
			name: None,
			tool_calls: Vec::new(),
			tool_call_id: None,
			is_error: false,
			usage: None,
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
			usage: None,
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
			usage: None,
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
			usage: None,
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
			usage: None,
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
			usage: None,
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
			usage: None,
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
			usage: None,
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
			usage: None,
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

/// A non-forcing requirement to call `tool_name` before the loop runs other
/// tools or yields (upstream `SoftToolRequirement`).
///
/// Hosts return one while a staged preview awaits `write xd://resolve|reject`;
/// the loop injects `reminder` once per `(id, generation)` head, keeps
/// `tool_choice` on auto, and only escalates to a one-request forced call when
/// the model ignores it. The generation makes a cleared queue invalidate the
/// head even when ids wrap.
#[derive(Debug, Clone)]
pub struct SoftToolRequirement {
	pub id:         u64,
	pub generation: u64,
	pub tool_name:  String,
	pub reminder:   Message,
}

/// Host-owned policy consulted once per provider request.
///
/// The loop owns the reminder/escalation lifecycle for one dialog run; the host
/// only exposes the current queue head and the exact invocation shape that
/// satisfies it, so no force state can leak between prompts or child agents.
pub trait ToolPolicy: Send + Sync {
	/// Current soft requirement, or `None` when nothing is staged.
	fn requirement(&self) -> Option<SoftToolRequirement>;

	/// Whether one assistant tool call satisfies `requirement`. A host demands
	/// the precise invocation shape here (e.g. `write` to a resolve device), not
	/// merely the tool name.
	fn satisfies(&self, requirement: &SoftToolRequirement, call: &ToolCall) -> bool;

	/// Tool choice for the single escalated request after a non-compliant turn.
	/// Defaults to forcing the requirement's named tool; a host whose upstream
	/// gate only requires *some* tool overrides this with
	/// [`ToolChoice::Required`](crate::provider::ToolChoice::Required).
	fn choice(&self, requirement: &SoftToolRequirement) -> crate::provider::ToolChoice {
		crate::provider::ToolChoice::Tool { name: requirement.tool_name.clone() }
	}

	/// Outcome once the generic escalation budget is spent. Default keeps the
	/// aborting behavior callers relied on before a policy could opt out.
	fn exhaustion(&self, _requirement: &SoftToolRequirement) -> Exhaustion {
		Exhaustion::Error
	}

	/// Escalation budget for this head. Default keeps the historical generic
	/// budget; a policy mirroring a one-shot upstream queue overrides it.
	fn escalation_limit(&self, _requirement: &SoftToolRequirement) -> u32 {
		3
	}
}

/// Session-owned staged-preview policy (upstream `nextToolChoiceDirective`
/// soft branch plus `isPreviewResolutionToolCall`).
///
/// Peeking is pure: the queue head and generation stay in the
/// [`crate::tools::pending::PendingStore`], so rebuilding the tool registry for
/// every prompt cannot forget a staged head.
pub struct StagedPreviewPolicy {
	pending: Arc<crate::tools::pending::PendingStore>,
}

impl StagedPreviewPolicy {
	pub fn new(pending: Arc<crate::tools::pending::PendingStore>) -> Self {
		Self { pending }
	}
}

/// Upstream `resolve-reminder`: the single non-display reminder injected when a
/// staged preview becomes the current head.
pub fn resolve_reminder_message(source_tool: &str) -> Message {
	Message::user(format!(
		"`{source_tool}` result above: PREVIEW — no files changed. Finalize now with `write`: \
		 write a one-sentence plain-text reason to `xd://resolve` to APPLY, or `xd://reject` to DISCARD."
	))
}

impl ToolPolicy for StagedPreviewPolicy {
	fn requirement(&self) -> Option<SoftToolRequirement> {
		let head = self.pending.peek_head()?;
		Some(SoftToolRequirement {
			id:         head.id,
			generation: head.generation,
			tool_name:  "write".to_string(),
			reminder:   resolve_reminder_message(&head.source_tool),
		})
	}

	fn satisfies(&self, requirement: &SoftToolRequirement, call: &ToolCall) -> bool {
		// Only the exact `write xd://resolve|reject` shape resolves the preview;
		// a plain write elsewhere is a detour, and unknown `xd://` targets never
		// count (the tool itself fails closed).
		call.name == requirement.tool_name
			&& call
				.arguments
				.get("path")
				.and_then(serde_json::Value::as_str)
				.is_some_and(crate::tools::pending::is_resolution_device_path)
	}
}

/// What the loop does when a soft requirement survives its generic escalation
/// budget. Staged previews abort (a silently dropped preview would be data
/// loss); policies that mirror a converging upstream gate ask for a normal
/// end-turn hand-back instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exhaustion {
	Error,
	EndTurn,
}

/// Operation one plan-mode target describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanWriteOp {
	Create,
	Update,
	Delete,
	Rename,
	/// A previously staged mutation about to be applied by `xd://resolve`.
	DeferredApply,
}

/// One parsed, effective mutation target.
///
/// Tools classify what they will actually touch (parsed sections, patch hunks,
/// resolved paths), never an unrelated top-level argument.
#[derive(Debug, Clone, Copy)]
pub struct PlanWriteTarget<'a> {
	/// Authored or resolved path; empty only for a staged action that declares
	/// no targets, which fails closed while plan mode is active.
	pub path:      &'a str,
	pub op:        PlanWriteOp,
	pub rename_to: Option<&'a str>,
}

impl<'a> PlanWriteTarget<'a> {
	#[must_use]
	pub fn new(path: &'a str, op: PlanWriteOp) -> Self {
		Self { path, op, rename_to: None }
	}

	#[must_use]
	pub fn rename(path: &'a str, to: &'a str) -> Self {
		Self { path, op: PlanWriteOp::Rename, rename_to: Some(to) }
	}
}

/// Host-owned plan-mode write guard mounted on the mutating tools.
///
/// Plan mode keeps the working tree read-only. The host owns the mode state and
/// the exact user-facing classification; tools hand it every parsed target
/// before any resolution. `bash` is never consulted (upstream gap, retained).
pub trait PlanModeGuard: Send + Sync {
	/// `Some(message)` blocks the call; `None` allows it.
	fn write_error(&self, tool_name: &str, targets: &[PlanWriteTarget<'_>]) -> Option<String>;

	/// Sandbox root a local-artifact mutation may use. `Some` only while plan
	/// mode is active: tools route parsed `local:` targets through this root and
	/// refuse the mutation when it is `None`.
	fn artifact_root(&self) -> Option<std::path::PathBuf> {
		None
	}
}

/// `xd://propose` handler mounted on `write` while plan mode is active.
///
/// `body` is the raw `write` content — JSON proposal args
/// (`{"title": …, "content" | "slug": …}`) or a plain plan title. Without a
/// mounted handler `write xd://propose` fails with the upstream wording.
#[async_trait]
pub trait ProposeHandler: Send + Sync {
	async fn propose(&self, body: &str, ctx: &AgentContext) -> Result<ToolResult, crate::tools::ToolError>;
}

/// Two-policy stack: the staged-preview requirement wins while a preview is
/// queued, and the plan-mode reminder applies only when nothing is staged.
///
/// Ordering is load-bearing: `xd://resolve|reject` must settle the staged
/// action before the loop can ask for a plan decision.
pub struct CompositeToolPolicy {
	staged: Arc<dyn ToolPolicy>,
	plan:   Option<Arc<dyn ToolPolicy>>,
}

impl CompositeToolPolicy {
	pub fn new(staged: Arc<dyn ToolPolicy>, plan: Option<Arc<dyn ToolPolicy>>) -> Self {
		Self { staged, plan }
	}

	/// Policy that produced `requirement`: the head is re-peeked (both policies
	/// expose a pure requirement), so a stale staged requirement can never
	/// authorize a plan one or vice versa.
	fn owner(&self, requirement: &SoftToolRequirement) -> Option<&Arc<dyn ToolPolicy>> {
		let staged_owns = self.staged.requirement().is_some_and(|head| {
			head.id == requirement.id
				&& head.generation == requirement.generation
				&& head.tool_name == requirement.tool_name
		});
		if staged_owns {
			Some(&self.staged)
		} else {
			self.plan.as_ref()
		}
	}
}

impl ToolPolicy for CompositeToolPolicy {
	fn requirement(&self) -> Option<SoftToolRequirement> {
		self
			.staged
			.requirement()
			.or_else(|| self.plan.as_ref()?.requirement())
	}

	fn satisfies(&self, requirement: &SoftToolRequirement, call: &ToolCall) -> bool {
		self
			.owner(requirement)
			.is_some_and(|policy| policy.satisfies(requirement, call))
	}

	fn choice(&self, requirement: &SoftToolRequirement) -> crate::provider::ToolChoice {
		self.owner(requirement).map_or(
			crate::provider::ToolChoice::Auto,
			|policy| policy.choice(requirement),
		)
	}

	fn exhaustion(&self, requirement: &SoftToolRequirement) -> Exhaustion {
		self
			.owner(requirement)
			.map_or(Exhaustion::Error, |policy| policy.exhaustion(requirement))
	}

	fn escalation_limit(&self, requirement: &SoftToolRequirement) -> u32 {
		self.owner(requirement).map_or(3, |policy| policy.escalation_limit(requirement))
	}
}

/// Outcome of one ACP reverse call.
///
/// Tools fall back to their native implementation only for [`Self::Unsupported`]
/// (the client did not advertise the capability). A remote failure may have
/// produced a side effect — a write or a started command — so retrying it
/// natively could duplicate it; [`Self::Cancelled`] must never retry either.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcpClientError {
	/// The client did not advertise this method, or refused to serve it.
	Unsupported,
	/// The request was sent and failed.
	Remote(String),
	/// The turn was cancelled before a result arrived.
	Cancelled,
}

impl std::fmt::Display for AcpClientError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::Unsupported => f.write_str("ACP client did not advertise this capability"),
			Self::Remote(message) => write!(f, "ACP client call failed: {message}"),
			Self::Cancelled => f.write_str("ACP client call cancelled"),
		}
	}
}

impl std::error::Error for AcpClientError {}

/// Reverse calls the ACP client advertised for one session.
///
/// Mounted on the native `read`/`write`/`bash` tool instances by the host
/// composition (root registry plus the supervisor's child registries). Methods
/// are async because the transport answers on another thread; implementations
/// MUST NOT block a Tokio worker while waiting (use `spawn_blocking`).
#[async_trait]
pub trait AcpClientBridge: Send + Sync {
	/// `clientCapabilities.fs.readTextFile`.
	fn supports_fs_read(&self) -> bool {
		false
	}

	/// `clientCapabilities.fs.writeTextFile`.
	fn supports_fs_write(&self) -> bool {
		false
	}

	/// `clientCapabilities.terminal`.
	fn supports_terminal(&self) -> bool {
		false
	}

	async fn read_text_file(
		&self,
		path: &str,
		line: Option<u64>,
		limit: Option<u64>,
		cancel: &CancellationToken,
	) -> Result<String, AcpClientError>;

	async fn write_text_file(
		&self,
		path: &str,
		content: &str,
		cancel: &CancellationToken,
	) -> Result<(), AcpClientError>;

	async fn terminal_create(
		&self,
		command: &str,
		args: Vec<String>,
		env: Vec<(String, String)>,
		cwd: Option<&str>,
		output_byte_limit: Option<u64>,
		cancel: &CancellationToken,
	) -> Result<String, AcpClientError>;

	async fn terminal_output(
		&self,
		terminal_id: &str,
		cancel: &CancellationToken,
	) -> Result<serde_json::Value, AcpClientError>;

	async fn terminal_wait_for_exit(
		&self,
		terminal_id: &str,
		cancel: &CancellationToken,
	) -> Result<serde_json::Value, AcpClientError>;

	async fn terminal_kill(
		&self,
		terminal_id: &str,
		cancel: &CancellationToken,
	) -> Result<serde_json::Value, AcpClientError>;

	async fn terminal_release(
		&self,
		terminal_id: &str,
		cancel: &CancellationToken,
	) -> Result<serde_json::Value, AcpClientError>;
}

/// Tool approval gate checked prior to executing side-effecting operations.
#[async_trait]
pub trait ToolApprover: Send + Sync {
	async fn approve(&self, name: &str, args: &serde_json::Value) -> Result<(), String>;

	/// Interactive frontend exposed to the `ask` tool, when this approver is
	/// backed by one. Children receive a stripped approver, so delegated runs
	/// stay headless.
	fn ask_channel(&self) -> Option<Arc<dyn crate::tools::ask::AskChannel>> {
		None
	}
}

/// Shared context accessible to tools and execution loop.
#[derive(Clone)]
pub struct AgentContext {
	pub cwd: PathBuf,
	pub env: HashMap<String, String>,
	pub cancellation_token: CancellationToken,
	pub approver: Option<Arc<dyn ToolApprover>>,
	/// Interactive `ask` surface; `None` in headless runs and child agents.
	pub ask: Option<Arc<dyn crate::tools::ask::AskChannel>>,
	/// Live agent-event sink. A tool reports progress by enqueueing a
	/// `ToolUpdate` on this same channel the loop uses for `ToolStart`/`ToolEnd`,
	/// so delivery is ordered and backpressured by one bounded queue.
	pub progress: Option<tokio::sync::mpsc::Sender<crate::AgentEvent>>,
	/// Session plan-mode guard for this run. Per context, never on a shared
	/// tool instance: concurrent sessions and child agents each build their own.
	pub plan: Option<Arc<dyn PlanModeGuard>>,
}

impl std::fmt::Debug for AgentContext {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("AgentContext")
			.field("cwd", &self.cwd)
			.field("env", &self.env)
			.field("cancellation_token", &self.cancellation_token)
			.field("has_approver", &self.approver.is_some())
			.field("has_ask", &self.ask.is_some())
			.field("has_progress", &self.progress.is_some())
			.field("has_plan", &self.plan.is_some())
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
			ask: None,
			progress: None,
			plan: None,
		}
	}

	pub fn with_cancellation(cwd: PathBuf, cancellation_token: CancellationToken) -> Self {
		Self {
			cwd,
			env: std::env::vars().collect(),
			cancellation_token,
			approver: None,
			ask: None,
			progress: None,
			plan: None,
		}
	}

	pub fn with_approver(
		cwd: PathBuf,
		cancellation_token: CancellationToken,
		approver: Option<Arc<dyn ToolApprover>>,
	) -> Self {
		let ask = approver.as_ref().and_then(|approver| approver.ask_channel());
		Self {
			cwd,
			env: std::env::vars().collect(),
			cancellation_token,
			approver,
			ask,
			progress: None,
			plan: None,
		}
	}

	/// Attach the session plan-mode guard for this run.
	#[must_use]
	pub fn with_plan(mut self, plan: Option<Arc<dyn PlanModeGuard>>) -> Self {
		self.plan = plan;
		self
	}

	/// Attach an interactive `ask` surface (tests and embedders).
	#[must_use]
	pub fn with_ask(mut self, ask: Option<Arc<dyn crate::tools::ask::AskChannel>>) -> Self {
		self.ask = ask;
		self
	}

	/// Attach the live agent-event sink a tool reports progress through.
	#[must_use]
	pub fn with_progress(
		mut self,
		progress: Option<tokio::sync::mpsc::Sender<crate::AgentEvent>>,
	) -> Self {
		self.progress = progress;
		self
	}

	/// Report partial tool output as a `ToolUpdate` on the loop's own event
	/// channel.
	///
	/// The send awaits capacity: no update is dropped, and because the same
	/// bounded queue carries `ToolStart`/`ToolEnd`, a tool that reports before
	/// returning is ordered before its `ToolEnd`. The wait races cancellation so
	/// a stopping run never blocks on a host that stopped reading.
	pub async fn report_progress(
		&self,
		tool_call_id: &str,
		name: &str,
		args: &serde_json::Value,
		partial: serde_json::Value,
	) {
		let Some(sink) = &self.progress else {
			return;
		};
		let event = crate::AgentEvent::ToolUpdate {
			id: tool_call_id.to_string(),
			name: name.to_string(),
			args: args.clone(),
			partial,
		};
		tokio::select! {
			_ = self.cancellation_token.cancelled() => {},
			_ = sink.send(event) => {},
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

#[cfg(test)]
mod composite_policy_tests {
	use super::*;

	/// Deterministic policy: the requirement and the satisfaction verdict are
	/// fixed, so ordering is the only variable under test.
	struct FixedPolicy {
		requirement: Option<SoftToolRequirement>,
		satisfied:   bool,
	}

	impl ToolPolicy for FixedPolicy {
		fn requirement(&self) -> Option<SoftToolRequirement> {
			self.requirement.clone()
		}

		fn satisfies(&self, _requirement: &SoftToolRequirement, _call: &ToolCall) -> bool {
			self.satisfied
		}
	}

	fn head(tool_name: &str, id: u64) -> SoftToolRequirement {
		SoftToolRequirement {
			id,
			generation: 1,
			tool_name: tool_name.to_string(),
			reminder: Message::user("reminder"),
		}
	}

	fn call(name: &str, path: &str) -> ToolCall {
		ToolCall {
			id: "call-1".to_string(),
			name: name.to_string(),
			arguments: serde_json::json!({ "path": path }),
		}
	}

	struct NamedChoice;

	impl ToolPolicy for NamedChoice {
		fn requirement(&self) -> Option<SoftToolRequirement> {
			Some(head("ask", 9))
		}

		fn satisfies(&self, _requirement: &SoftToolRequirement, _call: &ToolCall) -> bool {
			true
		}

		fn choice(&self, _requirement: &SoftToolRequirement) -> crate::provider::ToolChoice {
			crate::provider::ToolChoice::Required
		}
	}

	#[test]
	fn staged_preview_policy_keeps_first_priority() {
		let staged = FixedPolicy { requirement: Some(head("write", 7)), satisfied: true };
		let plan = Arc::new(NamedChoice);
		let policy = CompositeToolPolicy::new(Arc::new(staged), Some(plan));

		let requirement = policy.requirement().expect("staged head");
		assert_eq!(requirement.tool_name, "write");
		assert!(policy.satisfies(&requirement, &call("write", "xd://resolve")));
		assert_eq!(
			policy.choice(&requirement),
			crate::provider::ToolChoice::Tool { name: "write".to_string() },
			"staged preview keeps the named-write force"
		);
	}

	#[test]
	fn plan_reminder_applies_only_when_nothing_is_staged() {
		let staged = Arc::new(FixedPolicy { requirement: None, satisfied: false });
		let plan = Arc::new(NamedChoice);
		let policy = CompositeToolPolicy::new(staged, Some(plan));

		let requirement = policy.requirement().expect("plan head");
		assert_eq!(requirement.tool_name, "ask");
		assert!(policy.satisfies(&requirement, &call("ask", "ignored")));
		assert_eq!(policy.choice(&requirement), crate::provider::ToolChoice::Required);
	}

	#[test]
	fn exhaustion_belongs_to_the_owning_policy() {
		struct Yielding;

		impl ToolPolicy for Yielding {
			fn requirement(&self) -> Option<SoftToolRequirement> {
				Some(head("ask", 11))
			}

			fn satisfies(&self, _requirement: &SoftToolRequirement, _call: &ToolCall) -> bool {
				true
			}

			fn exhaustion(&self, _requirement: &SoftToolRequirement) -> Exhaustion {
				Exhaustion::EndTurn
			}
		}

		let staged = Arc::new(FixedPolicy { requirement: Some(head("write", 11)), satisfied: true });
		let policy = CompositeToolPolicy::new(staged, Some(Arc::new(Yielding)));
		let requirement = policy.requirement().expect("staged head");
		assert_eq!(policy.exhaustion(&requirement), Exhaustion::Error);

		let policy = CompositeToolPolicy::new(
			Arc::new(FixedPolicy { requirement: None, satisfied: false }),
			Some(Arc::new(Yielding)),
		);
		let requirement = policy.requirement().expect("plan head");
		assert_eq!(policy.exhaustion(&requirement), Exhaustion::EndTurn);
	}

	#[test]
	fn without_a_plan_policy_the_stack_is_the_staged_policy() {
		let policy = CompositeToolPolicy::new(
			Arc::new(FixedPolicy { requirement: None, satisfied: true }),
			None,
		);
		assert!(policy.requirement().is_none());
	}
}
