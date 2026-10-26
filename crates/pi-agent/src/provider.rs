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
}

impl OpenAiCompatibleProvider {
	/// Build a provider with a rustls HTTP client.
	pub fn new(
		base_url: impl Into<String>,
		api_key: impl Into<String>,
		model: impl Into<String>,
	) -> Result<Self, ProviderError> {
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
		}
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
	) -> OpenAiChatRequest {
		OpenAiChatRequest {
			model: self.model.clone(),
			messages: to_openai_messages(messages),
			tools: tools.to_vec(),
			stream,
			reasoning_effort: self.effort.clone(),
		}
	}
}

#[async_trait]
impl ModelProvider for OpenAiCompatibleProvider {
	async fn complete(
		&self,
		messages: &[Message],
		tools: &[serde_json::Value],
	) -> Result<Message, ProviderError> {
		let body = self.request(messages, tools, false);
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
		let body = self.request(messages, tools, true);
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
	pub fn finish(self) -> Message {
		let mut tool_calls = Vec::with_capacity(self.calls.len());
		for (index, pending) in self.calls {
			let Some(name) = pending.name else { continue }; // arguments without a name are unusable
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
		Message::assistant_with_tools(self.content, tool_calls)
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiMessage {
	pub role: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub name: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub content: Option<String>,
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
pub fn to_openai_messages(messages: &[Message]) -> Vec<OpenAiMessage> {
	messages
		.iter()
		.map(|m| {
			let role = match m.role {
				Role::System => "system",
				Role::User => "user",
				Role::Assistant => "assistant",
				Role::Tool => "tool",
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

			OpenAiMessage {
				role: role.to_string(),
				name: m.name.clone(),
				content: if m.content.is_empty() && tool_calls.is_some() {
					None
				} else {
					Some(m.content.clone())
				},
				tool_calls,
				tool_call_id: m.tool_call_id.clone(),
			}
		})
		.collect()
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
