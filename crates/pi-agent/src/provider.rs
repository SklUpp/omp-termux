use std::{
	collections::{BTreeMap, VecDeque},
	pin::Pin,
	time::Duration,
};

use async_trait::async_trait;
use futures::{Stream, StreamExt};
use serde::{Deserialize, Serialize};

use crate::state::{Message, Role, ToolCall};

/// TCP/TLS connect budget for a provider endpoint.
///
/// Deliberately no total-request timeout: a streamed completion legitimately
/// stays open for minutes, so the caller (CLI, agent loop) owns cancellation.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum characters of an error response body kept for diagnostics.
const ERROR_BODY_CHARS: usize = 2048;

/// Error returned by a model provider or stream processing.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
	#[error("Network error: {0}")]
	Network(String),
	#[error("HTTP status {0}: {1}")]
	HttpStatus(u16, String),
	#[error("Serialization error: {0}")]
	Serialization(#[from] serde_json::Error),
	#[error("Stream protocol error: {0}")]
	Protocol(String),
	#[error("Invalid base URL: {0}")]
	InvalidBaseUrl(String),
	#[error("Unsupported image: {0}")]
	UnsupportedImage(String),
}

/// Delta chunk emitted by streaming completions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamChunk {
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub content: Option<String>,
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub tool_call_deltas: Vec<ToolCallDelta>,
	pub is_done: bool,
}

/// Incremental delta for a tool call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallDelta {
	pub index: usize,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub id: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub name: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub arguments_delta: Option<String>,
}

/// Pinned async stream of chunks.
pub type ChunkStream = Pin<Box<dyn Stream<Item = Result<StreamChunk, ProviderError>> + Send>>;

/// Model provider interface supporting both a whole-turn completion and an SSE stream.
///
/// Implementations own their transport. The agent loop only needs messages and
/// tool schemas, so anything from a raw HTTP client to a host-side callback can
/// satisfy this trait.
#[async_trait]
pub trait ModelProvider: Send + Sync {
	/// Complete a dialog turn with a single (non-streaming) request.
	async fn complete(
		&self,
		messages: &[Message],
		tools: &[serde_json::Value],
	) -> Result<Message, ProviderError>;

	/// Stream a dialog turn as OpenAI-style server-sent events.
	async fn stream(
		&self,
		messages: &[Message],
		tools: &[serde_json::Value],
	) -> Result<ChunkStream, ProviderError>;

	/// Whether this provider supports multimodal image inputs.
	fn supports_images(&self) -> bool {
		false
	}

	/// Validate that message contents are supported by this provider.
	fn check_support(&self, messages: &[Message]) -> Result<(), ProviderError> {
		if !self.supports_images() && messages.iter().any(Message::has_image) {
			return Err(ProviderError::UnsupportedImage(
				"provider does not support image content".to_string(),
			));
		}
		for m in messages {
			for img in m.image_parts() {
				if let Err(err) = img.validate() {
					return Err(ProviderError::UnsupportedImage(format!(
						"invalid image payload: {err}"
					)));
				}
			}
		}
		Ok(())
	}
}

/// Chat-completions provider for any OpenAI-compatible HTTP endpoint.
///
/// Base URL, API key and model are supplied by the caller (CLI/config): nothing
/// about a specific vendor, model id or prompt is compiled in.
pub struct OpenAiCompatibleProvider {
	http: reqwest::Client,
	base_url: String,
	api_key: String,
	model: String,
	effort: Option<String>,
	supports_images: bool,
}

impl OpenAiCompatibleProvider {
	/// Build a provider with a rustls HTTP client.
	pub fn new(
		base_url: impl Into<String>,
		api_key: impl Into<String>,
		model: impl Into<String>,
	) -> Result<Self, ProviderError> {
		let base_url = base_url.into();
		let parsed = reqwest::Url::parse(&base_url)
			.map_err(|err| ProviderError::InvalidBaseUrl(err.to_string()))?;
		if parsed.scheme() != "http" && parsed.scheme() != "https" {
			return Err(ProviderError::InvalidBaseUrl(format!(
				"unsupported scheme `{}`",
				parsed.scheme()
			)));
		}
		let http = reqwest::Client::builder()
			.connect_timeout(CONNECT_TIMEOUT)
			.build()
			.map_err(network_error)?;
		Ok(Self::with_client(http, base_url, api_key, model))
	}

	/// Build a provider around a caller-configured HTTP client.
	pub fn with_client(
		http: reqwest::Client,
		base_url: impl Into<String>,
		api_key: impl Into<String>,
		model: impl Into<String>,
	) -> Self {
		Self {
			http,
			base_url: base_url.into(),
			api_key: api_key.into(),
			model: model.into(),
			effort: None,
			supports_images: true,
		}
	}

	/// Enable or disable multimodal image support.
	pub fn with_images(mut self, enabled: bool) -> Self {
		self.supports_images = enabled;
		self
	}

	/// Whether multimodal image inputs are accepted.
	pub fn supports_images(&self) -> bool {
		self.supports_images
	}

	/// Set the reasoning effort sent as `reasoning_effort`.
	///
	/// `None` and blank strings both mean "let the endpoint decide", so the field
	/// is omitted from the request instead of being sent empty.
	pub fn with_effort(mut self, effort: Option<String>) -> Self {
		self.effort = effort.filter(|value| !value.trim().is_empty());
		self
	}

	/// Reasoning effort currently configured for this provider.
	pub fn effort(&self) -> Option<&str> {
		self.effort.as_deref()
	}

	/// `{base_url}/chat/completions`, tolerating a trailing slash in `base_url`.
	pub fn endpoint(&self) -> String {
		format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
	}

	fn request(
		&self,
		messages: &[Message],
		tools: &[serde_json::Value],
		stream: bool,
	) -> Result<OpenAiChatRequest, ProviderError> {
		Ok(OpenAiChatRequest {
			model: self.model.clone(),
			messages: to_openai_messages(messages)?,
			tools: tools.to_vec(),
			stream,
			reasoning_effort: self.effort.clone(),
		})
	}
}

#[async_trait]
impl ModelProvider for OpenAiCompatibleProvider {
	fn supports_images(&self) -> bool {
		self.supports_images
	}

	async fn complete(
		&self,
		messages: &[Message],
		tools: &[serde_json::Value],
	) -> Result<Message, ProviderError> {
		self.check_support(messages)?;
		let body = self.request(messages, tools, false)?;
		let response = self
			.http
			.post(self.endpoint())
			.bearer_auth(&self.api_key)
			.json(&body)
			.send()
			.await
			.map_err(network_error)?;

		let status = response.status();
		let text = response.text().await.map_err(network_error)?;
		if !status.is_success() {
			return Err(ProviderError::HttpStatus(status.as_u16(), snippet(&text)));
		}

		let parsed: OpenAiChatResponse = serde_json::from_str(&text).map_err(|err| {
			ProviderError::Protocol(format!(
				"invalid chat completion body: {err}; body: {}",
				snippet(&text)
			))
		})?;
		let choice = parsed.choices.into_iter().next().ok_or_else(|| {
			ProviderError::Protocol("chat completion contained no choices".to_string())
		})?;
		Ok(from_openai_response(choice.message))
	}

	async fn stream(
		&self,
		messages: &[Message],
		tools: &[serde_json::Value],
	) -> Result<ChunkStream, ProviderError> {
		self.check_support(messages)?;
		let body = self.request(messages, tools, true)?;
		let response = self
			.http
			.post(self.endpoint())
			.bearer_auth(&self.api_key)
			.header(reqwest::header::ACCEPT, "text/event-stream")
			.json(&body)
			.send()
			.await
			.map_err(network_error)?;

		let status = response.status();
		if !status.is_success() {
			// The error body is diagnostics only; a failed read must still surface
			// the status instead of looking like a transport error.
			let body = match response.text().await {
				Ok(text) => snippet(&text),
				Err(err) => format!("<failed to read error body: {err}>"),
			};
			return Err(ProviderError::HttpStatus(status.as_u16(), body));
		}

		let body = response
			.bytes_stream()
			.map(|item| item.map_err(|err| err.to_string()));
		Ok(sse_stream(Box::pin(body)))
	}
}

/// Accumulates streaming deltas into the final assistant message.
///
/// Text deltas are concatenated in arrival order; tool-call deltas are joined by
/// their `index`, so a call whose name arrives in one chunk and whose arguments
/// arrive in later chunks still becomes one [`ToolCall`].
#[derive(Debug, Default)]
pub struct StreamAssembler {
	content: String,
	calls: BTreeMap<usize, PendingCall>,
}

#[derive(Debug, Default)]
struct PendingCall {
	id: Option<String>,
	name: Option<String>,
	arguments: String,
}

impl StreamAssembler {
	pub fn new() -> Self {
		Self::default()
	}

	/// Fold one streamed chunk into the accumulated turn.
	pub fn push(&mut self, chunk: &StreamChunk) {
		if let Some(content) = &chunk.content {
			self.content.push_str(content);
		}
		for delta in &chunk.tool_call_deltas {
			let pending = self.calls.entry(delta.index).or_default();
			if let Some(id) = &delta.id {
				pending.id = Some(id.clone());
			}
			if let Some(name) = &delta.name {
				pending.name = Some(name.clone());
			}
			if let Some(arguments) = &delta.arguments_delta {
				pending.arguments.push_str(arguments);
			}
		}
	}

	/// Finish the turn, producing an assistant message with reassembled tool calls.
	///
	/// Returns a protocol error if any completed tool call lacks a name.
	pub fn finish(self) -> Result<Message, ProviderError> {
		let mut tool_calls = Vec::with_capacity(self.calls.len());
		for (index, pending) in self.calls {
			let Some(name) = pending.name.filter(|n| !n.trim().is_empty()) else {
				return Err(ProviderError::Protocol(format!(
					"streamed tool call at index {index} is missing or has empty tool name"
				)));
			};
			let arguments = if pending.arguments.trim().is_empty() {
				serde_json::Value::Object(serde_json::Map::new())
			} else {
				serde_json::from_str(&pending.arguments)
					.unwrap_or(serde_json::Value::String(pending.arguments))
			};
			tool_calls.push(ToolCall {
				id: pending.id.unwrap_or_else(|| format!("call_{index}")),
				name,
				arguments,
			});
		}
		Ok(Message::assistant_with_tools(self.content, tool_calls))
	}

	/// Produce a partial assistant message (content only, no tool calls) for aborted or failed turns.
	pub(crate) fn finish_partial(&self) -> Message {
		Message::assistant(&self.content)
	}
}

/// Request schema for the OpenAI Chat Completions REST API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiChatRequest {
	pub model: String,
	pub messages: Vec<OpenAiMessage>,
	#[serde(skip_serializing_if = "Vec::is_empty")]
	pub tools: Vec<serde_json::Value>,
	#[serde(default)]
	pub stream: bool,
	/// OpenAI-compatible `reasoning_effort`; omitted when unset.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub reasoning_effort: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenAiImageUrl {
	pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum OpenAiContentPart {
	#[serde(rename = "text")]
	Text { text: String },
	#[serde(rename = "image_url")]
	ImageUrl { image_url: OpenAiImageUrl },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OpenAiContent {
	Text(String),
	Parts(Vec<OpenAiContentPart>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiMessage {
	pub role: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub name: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub content: Option<OpenAiContent>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub tool_calls: Option<Vec<OpenAiToolCall>>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiToolCall {
	pub id: String,
	#[serde(rename = "type")]
	pub call_type: String,
	pub function: OpenAiFunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiFunctionCall {
	pub name: String,
	pub arguments: String,
}

/// Response schema for a non-streaming OpenAI chat completion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiChatResponse {
	#[serde(default)]
	pub choices: Vec<OpenAiChoice>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiChoice {
	pub message: OpenAiResponseMessage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiResponseMessage {
	#[serde(default)]
	pub content: Option<String>,
	#[serde(default)]
	pub tool_calls: Option<Vec<OpenAiToolCall>>,
}

/// SSE chunk schema for a streamed chat completion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiStreamChunk {
	#[serde(default)]
	pub choices: Vec<OpenAiStreamChoice>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiStreamChoice {
	pub delta: OpenAiDelta,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiDelta {
	#[serde(default)]
	pub content: Option<String>,
	#[serde(default)]
	pub tool_calls: Option<Vec<OpenAiStreamToolCall>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiStreamToolCall {
	pub index: usize,
	#[serde(default)]
	pub id: Option<String>,
	#[serde(default)]
	pub function: Option<OpenAiStreamFunctionCall>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiStreamFunctionCall {
	#[serde(default)]
	pub name: Option<String>,
	#[serde(default)]
	pub arguments: Option<String>,
}

/// Convert pi-agent domain messages into OpenAI API messages.
pub fn to_openai_messages(messages: &[Message]) -> Result<Vec<OpenAiMessage>, ProviderError> {
	let mut out = Vec::new();
	let mut i = 0;
	while i < messages.len() {
		let m = &messages[i];
		if m.role == Role::Tool {
			let mut j = i;
			let mut image_parts = Vec::new();
			while j < messages.len() && messages[j].role == Role::Tool {
				let tm = &messages[j];
				let mut this_tool_images = Vec::new();
				let mut text_parts = Vec::new();
				for part in &tm.parts {
					match part {
						crate::state::ContentPart::Text { text } => {
							if !text.is_empty() {
								text_parts.push(text.clone());
							}
						}
						crate::state::ContentPart::Image { data, mime_type } => {
							let (clean_b64, mime) = crate::state::normalize_image_payload(data, mime_type)
								.map_err(|err| ProviderError::UnsupportedImage(format!("invalid image payload: {err}")))?;
							this_tool_images.push(OpenAiContentPart::ImageUrl {
								image_url: OpenAiImageUrl {
									url: format!("data:{mime};base64,{clean_b64}"),
								},
							});
						}
					}
				}
				let has_images = !this_tool_images.is_empty();
				let text_result = if !tm.parts.is_empty() {
					text_parts.join("\n")
				} else {
					tm.content.clone()
				};
				let content_str = if !text_result.is_empty() {
					text_result
				} else if has_images {
					"(see attached image)".to_string()
				} else {
					tm.content.clone()
				};

				out.push(OpenAiMessage {
					role: "tool".to_string(),
					name: tm.name.clone(),
					content: Some(OpenAiContent::Text(content_str)),
					tool_calls: None,
					tool_call_id: tm.tool_call_id.clone(),
				});
				image_parts.extend(this_tool_images);
				j += 1;
			}

			if !image_parts.is_empty() {
				let mut user_parts = Vec::with_capacity(1 + image_parts.len());
				user_parts.push(OpenAiContentPart::Text {
					text: "Attached image(s) from tool result:".to_string(),
				});
				user_parts.extend(image_parts);
				out.push(OpenAiMessage {
					role: "user".to_string(),
					name: None,
					content: Some(OpenAiContent::Parts(user_parts)),
					tool_calls: None,
					tool_call_id: None,
				});
			}

			i = j;
			continue;
		}

		let role = match m.role {
			Role::System => "system",
			Role::User => "user",
			Role::Assistant => "assistant",
			Role::Tool => unreachable!(),
		};

		let tool_calls = if m.tool_calls.is_empty() {
			None
		} else {
			Some(
				m.tool_calls
					.iter()
					.map(|tc| OpenAiToolCall {
						id: tc.id.clone(),
						call_type: "function".to_string(),
						function: OpenAiFunctionCall {
							name: tc.name.clone(),
							arguments: tc.arguments.to_string(),
						},
					})
					.collect(),
			)
		};

		let content = if m.has_image() || !m.parts.is_empty() {
			let mut parts = Vec::new();
			for part in &m.parts {
				match part {
					crate::state::ContentPart::Text { text } => {
						if !text.is_empty() {
							parts.push(OpenAiContentPart::Text {
								text: text.clone(),
							});
						}
					}
					crate::state::ContentPart::Image { data, mime_type } => {
						let (clean_b64, mime) = crate::state::normalize_image_payload(data, mime_type)
							.map_err(|err| ProviderError::UnsupportedImage(format!("invalid image payload: {err}")))?;
						parts.push(OpenAiContentPart::ImageUrl {
							image_url: OpenAiImageUrl {
								url: format!("data:{mime};base64,{clean_b64}"),
							},
						});
					}
				}
			}
			if !m.content.is_empty()
				&& !parts.iter().any(|p| matches!(p, OpenAiContentPart::Text { .. }))
			{
				parts.insert(
					0,
					OpenAiContentPart::Text {
						text: m.content.clone(),
					},
				);
			}
			Some(OpenAiContent::Parts(parts))
		} else if m.content.is_empty() && tool_calls.is_some() {
			None
		} else {
			Some(OpenAiContent::Text(m.content.clone()))
		};

		out.push(OpenAiMessage {
			role: role.to_string(),
			name: m.name.clone(),
			content,
			tool_calls,
			tool_call_id: m.tool_call_id.clone(),
		});
		i += 1;
	}
	Ok(out)
}

/// Convert an OpenAI response message into a domain assistant [`Message`].
///
/// Tool arguments that are not valid JSON are preserved verbatim as a string so
/// the tool layer reports the malformed call instead of the model turn silently
/// losing it.
pub fn from_openai_response(resp_msg: OpenAiResponseMessage) -> Message {
	let mut tool_calls = Vec::new();
	if let Some(calls) = resp_msg.tool_calls {
		for call in calls {
			let arguments = serde_json::from_str(&call.function.arguments)
				.unwrap_or(serde_json::Value::String(call.function.arguments));
			tool_calls.push(ToolCall { id: call.id, name: call.function.name, arguments });
		}
	}

	Message::assistant_with_tools(resp_msg.content.unwrap_or_default(), tool_calls)
}

/// Parse one SSE line (`data: {...}` / `data: [DONE]`).
///
/// Returns `None` for lines that carry no payload (blank lines, `:` comments,
/// non-`data:` fields).
pub fn parse_sse_line(line: &str) -> Option<Result<StreamChunk, ProviderError>> {
	let trimmed = line.trim();
	if trimmed.is_empty() || trimmed.starts_with(':') {
		return None;
	}

	let data = trimmed.strip_prefix("data:")?.trim();
	if data == "[DONE]" {
		return Some(Ok(StreamChunk { content: None, tool_call_deltas: Vec::new(), is_done: true }));
	}

	match serde_json::from_str::<OpenAiStreamChunk>(data) {
		Ok(chunk) => {
			let mut content = None;
			let mut tool_call_deltas = Vec::new();

			for choice in chunk.choices {
				if let Some(delta) = choice.delta.content {
					content = Some(delta);
				}
				for tc in choice.delta.tool_calls.unwrap_or_default() {
					let (name, arguments_delta) = match tc.function {
						Some(func) => (func.name, func.arguments),
						None => (None, None),
					};
					tool_call_deltas.push(ToolCallDelta {
						index: tc.index,
						id: tc.id,
						name,
						arguments_delta,
					});
				}
			}

			Some(Ok(StreamChunk { content, tool_call_deltas, is_done: false }))
		},
		Err(err) => Some(Err(ProviderError::Serialization(err))),
	}
}

/// Turn a raw byte stream of SSE records into parsed chunks.
///
/// Transport-agnostic: any `Stream` of byte-ish `Result` items works, which
/// keeps the framing logic testable without a network.
///
/// Records may be split across transport chunks and the stream may end without a
/// trailing newline; both are handled by buffering until `\n` and flushing the
/// tail on EOF. `[DONE]` terminates the stream.
pub fn sse_stream<S, B, E>(body: S) -> ChunkStream
where
	S: Stream<Item = Result<B, E>> + Send + Unpin + 'static,
	B: AsRef<[u8]> + Send + 'static,
	E: std::fmt::Display + Send + 'static,
{
	struct State<S> {
		body: S,
		buf: Vec<u8>,
		pending: VecDeque<StreamChunk>,
		done: bool,
	}

	futures::stream::unfold(
		State { body, buf: Vec::new(), pending: VecDeque::new(), done: false },
		|mut st| async move {
			loop {
				if let Some(chunk) = st.pending.pop_front() {
					if chunk.is_done {
						st.done = true;
					}
					return Some((Ok(chunk), st));
				}
				if st.done {
					return None;
				}

				if let Some(pos) = st.buf.iter().position(|byte| *byte == b'\n') {
					let line: Vec<u8> = st.buf.drain(..=pos).collect();
					match parse_sse_line(&String::from_utf8_lossy(&line)) {
						None => {},
						Some(Ok(chunk)) => st.pending.push_back(chunk),
						Some(Err(err)) => {
							st.done = true;
							return Some((Err(err), st));
						},
					}
					continue;
				}

				match st.body.next().await {
					Some(Ok(bytes)) => st.buf.extend_from_slice(bytes.as_ref()),
					Some(Err(err)) => {
						st.done = true;
						return Some((Err(ProviderError::Network(err.to_string())), st));
					},
					None => {
						st.done = true;
						if st.buf.is_empty() {
							return None;
						}
						let line: Vec<u8> = std::mem::take(&mut st.buf);
						return parse_sse_line(&String::from_utf8_lossy(&line)).map(|res| (res, st));
					},
				}
			}
		},
	)
	.boxed()
}

fn network_error(err: reqwest::Error) -> ProviderError {
	ProviderError::Network(err.to_string())
}

fn snippet(text: &str) -> String {
	text.chars().take(ERROR_BODY_CHARS).collect()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn finish_rejects_missing_tool_name() {
		let mut assembler = StreamAssembler::new();
		assembler.push(&StreamChunk {
			content: Some("text".into()),
			tool_call_deltas: vec![ToolCallDelta {
				index: 0,
				id: Some("call_1".into()),
				name: None,
				arguments_delta: Some("{}".into()),
			}],
			is_done: false,
		});
		let err = assembler.finish().unwrap_err();
		assert!(matches!(err, ProviderError::Protocol(_)));
	}

	#[test]
	fn finish_rejects_empty_or_whitespace_tool_name() {
		let mut assembler = StreamAssembler::new();
		assembler.push(&StreamChunk {
			content: Some("text".into()),
			tool_call_deltas: vec![ToolCallDelta {
				index: 0,
				id: Some("call_1".into()),
				name: Some("   ".into()),
				arguments_delta: Some("{}".into()),
			}],
			is_done: false,
		});
		let err = assembler.finish().unwrap_err();
		assert!(matches!(err, ProviderError::Protocol(_)));
	}

	#[test]
	fn finish_accepts_valid_tool_call() {
		let mut assembler = StreamAssembler::new();
		assembler.push(&StreamChunk {
			content: Some("text".into()),
			tool_call_deltas: vec![ToolCallDelta {
				index: 0,
				id: Some("call_1".into()),
				name: Some("read".into()),
				arguments_delta: Some("{}".into()),
			}],
			is_done: false,
		});
		let msg = assembler.finish().unwrap();
		assert_eq!(msg.content, "text");
		assert_eq!(msg.tool_calls.len(), 1);
		assert_eq!(msg.tool_calls[0].name, "read");
	}

	#[test]
	fn to_openai_messages_groups_tool_results_and_hoists_images() {
		let t1 = Message::tool_with_status_and_parts(
			"c1",
			"tool1",
			"text 1",
			vec![
				crate::state::ContentPart::text("text 1"),
				crate::state::ContentPart::image("aW1hZ2Ux", "image/png").unwrap(),
			],
			false,
		);
		let t2 = Message::tool_with_status_and_parts(
			"c2",
			"tool2",
			"text 2",
			vec![crate::state::ContentPart::text("text 2")],
			false,
		);
		let wire = to_openai_messages(&[t1, t2]).expect("to wire");
		assert_eq!(wire.len(), 3);
		assert_eq!(wire[0].role, "tool");
		assert_eq!(wire[0].tool_call_id.as_deref(), Some("c1"));
		assert_eq!(wire[0].content, Some(OpenAiContent::Text("text 1".to_string())));

		assert_eq!(wire[1].role, "tool");
		assert_eq!(wire[1].tool_call_id.as_deref(), Some("c2"));
		assert_eq!(wire[1].content, Some(OpenAiContent::Text("text 2".to_string())));

		assert_eq!(wire[2].role, "user");
		assert_eq!(wire[2].tool_call_id, None);
		if let Some(OpenAiContent::Parts(parts)) = &wire[2].content {
			assert_eq!(parts.len(), 2);
			assert_eq!(parts[0], OpenAiContentPart::Text {
				text: "Attached image(s) from tool result:".to_string(),
			});
			assert_eq!(parts[1], OpenAiContentPart::ImageUrl {
				image_url: OpenAiImageUrl {
					url: "data:image/png;base64,aW1hZ2Ux".to_string(),
				},
			});
		} else {
			panic!("expected OpenAiContent::Parts for user image message");
		}
	}

	#[test]
	fn to_openai_messages_image_only_tool_result_placeholder() {
		let t1 = Message::tool_with_status_and_parts(
			"c1",
			"tool1",
			"",
			vec![crate::state::ContentPart::image("aW1hZ2Ux", "image/png").unwrap()],
			false,
		);
		let wire = to_openai_messages(&[t1]).expect("to wire");
		assert_eq!(wire.len(), 2);
		assert_eq!(wire[0].role, "tool");
		assert_eq!(wire[0].content, Some(OpenAiContent::Text("(see attached image)".to_string())));
		assert_eq!(wire[1].role, "user");
	}

	#[test]
	fn to_openai_messages_tool_result_fails_closed_on_invalid_image() {
		let t1 = Message {
			role: Role::Tool,
			content: String::new(),
			parts: vec![crate::state::ContentPart::Image {
				data: "not-base64!".to_string(),
				mime_type: "image/png".to_string(),
			}],
			name: Some("tool".to_string()),
			tool_calls: Vec::new(),
			tool_call_id: Some("c1".to_string()),
			is_error: false,
		};
		let err = to_openai_messages(&[t1]).unwrap_err();
		assert!(matches!(err, ProviderError::UnsupportedImage(_)));
	}
}
