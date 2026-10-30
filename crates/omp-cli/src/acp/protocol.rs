//! ACP protocol surface: JSON-RPC 2.0 envelopes, error codes and capabilities.
//!
//! Ground truth is `packages/utils/src/acp/transport.ts` (envelopes, error
//! codes, messages) and `packages/utils/src/acp/protocol.ts` (capability
//! shapes). Only what the transport and the ACP session driver share lives
//! here: session state, method dispatch and speech catalogues belong to the
//! modules built on top.

use std::{fmt, hash::{Hash, Hasher}};

use serde::{Deserialize, Serialize};
use serde_json::{Number, Value, json};

/// ACP protocol version announced by this build.
pub const PROTOCOL_VERSION: u32 = 1;
/// `agentInfo.name` advertised by `initialize`.
pub const AGENT_NAME: &str = "oh-my-pi";
/// `agentInfo.title` advertised by `initialize`.
pub const AGENT_TITLE: &str = "Oh My Pi";
/// Transport-level cancellation request. Upstream `dispatchAgent` returns
/// without failing, so the caller sees success.
pub const CANCEL_REQUEST_METHOD: &str = "$/cancel_request";

/// JSON parse failure.
pub const PARSE_ERROR: i64 = -32700;
/// Envelope that is not a JSON-RPC 2.0 message.
pub const INVALID_REQUEST: i64 = -32600;
/// Method this peer does not serve.
pub const METHOD_NOT_FOUND: i64 = -32601;
/// Method known, parameters unusable.
pub const INVALID_PARAMS: i64 = -32602;
/// Handler fault; never used to hide a missing backend.
pub const INTERNAL_ERROR: i64 = -32603;
/// The request was cancelled with `$/cancel_request` or a session cancel.
pub const REQUEST_CANCELLED: i64 = -32800;
/// Provider credentials are missing for the requested operation.
pub const AUTH_REQUIRED: i64 = -32000;
/// A `fs/*`-style URI does not resolve.
pub const RESOURCE_NOT_FOUND: i64 = -32002;
/// The session is already processing; the call can be retried once idle.
pub const SESSION_BUSY: i64 = -32003;

/// JSON-RPC request identifier.
///
/// Upstream types it `string | number | null` (`transport.ts:5`) and keys its
/// pending table by the parsed value (`transport.ts:114`). JavaScript numbers
/// are IEEE-754 doubles, so a numeric id is kept as the parsed
/// [`serde_json::Number`] — integers, fractions and magnitudes survive — and
/// compared by double value, so `1` and `1.0` are the same id exactly like a
/// JS `Map` key. Numeric ids are never renumbered; the transport's own counter
/// starts at `0` like upstream.
#[derive(Debug, Clone)]
pub enum JsonRpcId {
	Number(Number),
	String(String),
	Null,
}

impl JsonRpcId {
	/// Numeric id, used by the transport's own counter.
	pub fn number(value: i64) -> Self {
		Self::Number(Number::from(value))
	}

	/// Read an id field. `None` means "present but not a legal id", which the
	/// caller must answer with [`INVALID_REQUEST`].
	pub fn from_json(value: &Value) -> Option<Self> {
		match value {
			Value::Null => Some(Self::Null),
			Value::Number(number) => Some(Self::Number(number.clone())),
			Value::String(text) => Some(Self::String(text.clone())),
			_ => None,
		}
	}

	/// Wire form of this id, echoed as parsed.
	pub fn as_json(&self) -> Value {
		match self {
			Self::Number(number) => Value::Number(number.clone()),
			Self::String(text) => Value::from(text.clone()),
			Self::Null => Value::Null,
		}
	}
}

/// Double value of a numeric id with `-0` folded into `0`.
fn numeric_key(number: &Number) -> Option<u64> {
	number
		.as_f64()
		.map(|value| if value == 0.0 { 0.0f64 } else { value })
		.map(f64::to_bits)
}

impl PartialEq for JsonRpcId {
	fn eq(&self, other: &Self) -> bool {
		match (self, other) {
			(Self::Number(left), Self::Number(right)) => {
				match (numeric_key(left), numeric_key(right)) {
					(Some(left), Some(right)) => left == right,
					_ => false,
				}
			},
			(Self::String(left), Self::String(right)) => left == right,
			(Self::Null, Self::Null) => true,
			_ => false,
		}
	}
}

impl Eq for JsonRpcId {}

impl Hash for JsonRpcId {
	fn hash<H: Hasher>(&self, state: &mut H) {
		match self {
			Self::Number(number) => {
				0u8.hash(state);
				numeric_key(number).hash(state);
			},
			Self::String(text) => {
				1u8.hash(state);
				text.hash(state);
			},
			Self::Null => 2u8.hash(state),
		}
	}
}

impl fmt::Display for JsonRpcId {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Number(number) => write!(f, "{number}"),
			Self::String(text) => f.write_str(text),
			Self::Null => f.write_str("null"),
		}
	}
}

/// Error returned by a peer or by a handler.
///
/// Messages mirror upstream: standard code + text, an optional `: detail`
/// suffix, and optional `data`. Details never carry request bodies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestError {
	pub code:    i64,
	pub message: String,
	pub data:    Option<Value>,
}

impl RequestError {
	/// Error with a caller-chosen code and message.
	pub fn new(code: i64, message: impl Into<String>) -> Self {
		Self { code, message: message.into(), data: None }
	}

	/// Attach the `error.data` payload.
	pub fn with_data(mut self, data: Value) -> Self {
		self.data = Some(data);
		self
	}

	/// Append `: detail` to the message, matching upstream
	/// `createStandardError` (`transport.ts:82`).
	pub fn with_detail(mut self, detail: impl AsRef<str>) -> Self {
		self.message = format!("{}: {}", self.message, detail.as_ref());
		self
	}

	/// `-32700`: the line was not JSON.
	pub fn parse_error() -> Self {
		Self::new(PARSE_ERROR, "Parse error")
	}

	/// `-32600`: valid JSON, not a JSON-RPC 2.0 message.
	pub fn invalid_request() -> Self {
		Self::new(INVALID_REQUEST, "Invalid request")
	}

	/// `-32601`: upstream message is `"Method not found": {method}`.
	pub fn method_not_found(method: &str) -> Self {
		Self::new(METHOD_NOT_FOUND, format!("\"Method not found\": {method}"))
			.with_data(json!({ "method": method }))
	}

	/// `-32602`: method known, parameters unusable.
	pub fn invalid_params() -> Self {
		Self::new(INVALID_PARAMS, "Invalid params")
	}

	/// `-32603`: handler fault.
	pub fn internal_error() -> Self {
		Self::new(INTERNAL_ERROR, "Internal error")
	}

	/// `-32800`: the call was cancelled.
	pub fn request_cancelled() -> Self {
		Self::new(REQUEST_CANCELLED, "Request cancelled")
	}

	/// `-32000`: credentials are required for this operation.
	pub fn auth_required() -> Self {
		Self::new(AUTH_REQUIRED, "Authentication required")
	}

	/// `-32002`: a resource URI does not resolve.
	pub fn resource_not_found(uri: Option<&str>) -> Self {
		match uri {
			Some(uri) => {
				Self::new(RESOURCE_NOT_FOUND, format!("Resource not found: {uri}"))
					.with_data(json!({ "uri": uri }))
			},
			None => Self::new(RESOURCE_NOT_FOUND, "Resource not found"),
		}
	}

	/// `-32003`: the session is busy; `data` should keep the stable
	/// `{"reason":"session_busy"}` discriminator (`transport.ts:66-74`).
	pub fn session_busy(message: impl Into<String>, data: Option<Value>) -> Self {
		let mut error = Self::new(SESSION_BUSY, message);
		error.data = data;
		error
	}

	/// The `error` member of a JSON-RPC response.
	pub fn to_value(&self) -> Value {
		let mut value = json!({ "code": self.code, "message": self.message });
		if let Some(data) = &self.data {
			value["data"] = data.clone();
		}
		value
	}
}

impl fmt::Display for RequestError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(&self.message)
	}
}

impl std::error::Error for RequestError {}

/// One decoded inbound frame.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
	/// Has an id: the handler result must be written back.
	Request {
		id:     JsonRpcId,
		method: String,
		params: Option<Value>,
	},
	/// No id: handler errors are dropped, no frame is written
	/// (`transport.ts:203-211`).
	Notification { method: String, params: Option<Value> },
	/// Answer to one of our reverse calls.
	Response { id: JsonRpcId, outcome: Result<Value, RequestError> },
}

/// A line that could not be decoded, plus the id to answer under (if any).
#[derive(Debug, Clone, PartialEq)]
pub struct LineError {
	/// Id echoed in the error response; `None` becomes `null`.
	pub id:    Option<JsonRpcId>,
	pub error: RequestError,
	/// True when the frame has no `method` member, so it is a malformed
	/// *answer* to one of our requests, not a malformed peer request. The
	/// transport fails a matching pending call instead of echoing `-32600`.
	pub response_shaped: bool,
}

/// Decode one newline-delimited frame.
///
/// `Ok(None)` is an envelope upstream ignores: no `method` and no `id`
/// (`transport.ts:200`). `Err` must be answered and the connection kept alive;
/// upstream's `ndJsonStream` errors the whole stream instead, which would let
/// one stray byte kill a live session.
///
/// A response must carry `result` or `error`; upstream would resolve such a
/// frame with `undefined`, and this crate never fabricates a result.
pub fn parse_line(line: &str) -> Result<Option<Incoming>, LineError> {
	let value: Value = serde_json::from_str(line).map_err(|error| LineError {
		id:              None,
		error:           RequestError::parse_error().with_detail(error.to_string()),
		response_shaped: false,
	})?;
	let Some(object) = value.as_object() else {
		return Err(LineError {
			id:              None,
			error:           RequestError::invalid_request(),
			response_shaped: false,
		});
	};
	let response_shaped = !object.contains_key("method");
	let id = match object.get("id") {
		Some(raw) => Some(JsonRpcId::from_json(raw).ok_or_else(|| LineError {
			id:              None,
			error:           RequestError::invalid_request()
				.with_detail("id must be a string, number or null"),
			response_shaped,
		})?),
		None => None,
	};
	if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
		return Err(LineError {
			id,
			error: RequestError::invalid_request().with_detail("jsonrpc must be \"2.0\""),
			response_shaped,
		});
	}
	let Some(method) = object.get("method") else {
		let Some(id) = id else {
			return Ok(None);
		};
		if let Some(error) = object.get("error") {
			let error = parse_error_object(error).ok_or_else(|| LineError {
				id:              Some(id.clone()),
				error:           RequestError::invalid_request()
					.with_detail("malformed error member"),
				response_shaped,
			})?;
			return Ok(Some(Incoming::Response { id, outcome: Err(error) }));
		}
		return match object.get("result") {
			Some(result) => Ok(Some(Incoming::Response { id, outcome: Ok(result.clone()) })),
			None => Err(LineError {
				id:              Some(id),
				error:           RequestError::invalid_request()
					.with_detail("response without result or error"),
				response_shaped,
			}),
		};
	};
	let Some(method) = method.as_str() else {
		return Err(LineError {
			id,
			error: RequestError::invalid_request().with_detail("method must be a string"),
			response_shaped,
		});
	};
	let method = method.to_string();
	let params = object.get("params").cloned();
	match id {
		Some(id) => Ok(Some(Incoming::Request { id, method, params })),
		None => Ok(Some(Incoming::Notification { method, params })),
	}
}

fn parse_error_object(value: &Value) -> Option<RequestError> {
	let object = value.as_object()?;
	let code = object.get("code")?.as_i64()?;
	let message = object.get("message")?.as_str()?.to_string();
	Some(RequestError { code, message, data: object.get("data").cloned() })
}

/// Frame for a correlated request.
pub fn request_frame(id: &JsonRpcId, method: &str, params: Option<&Value>) -> Value {
	let mut frame = json!({ "jsonrpc": "2.0", "id": id.as_json(), "method": method });
	if let Some(params) = params {
		frame["params"] = params.clone();
	}
	frame
}

/// Frame for a notification.
pub fn notification_frame(method: &str, params: Option<&Value>) -> Value {
	let mut frame = json!({ "jsonrpc": "2.0", "method": method });
	if let Some(params) = params {
		frame["params"] = params.clone();
	}
	frame
}

/// Successful response. Upstream writes `result: result ?? {}`
/// (`transport.ts:213`), so a `null` handler result is written as `{}`.
pub fn result_frame(id: &JsonRpcId, result: Value) -> Value {
	let result = if result.is_null() { json!({}) } else { result };
	json!({ "jsonrpc": "2.0", "id": id.as_json(), "result": result })
}

/// Error response; `None` becomes a `null` id.
pub fn error_frame(id: Option<&JsonRpcId>, error: &RequestError) -> Value {
	let id = id.map_or(Value::Null, JsonRpcId::as_json);
	json!({ "jsonrpc": "2.0", "id": id, "error": error.to_value() })
}

/// `agentInfo` advertised by `initialize`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentInfo {
	pub name:    String,
	pub title:   String,
	pub version: String,
}

impl Default for AgentInfo {
	fn default() -> Self {
		Self {
			name:    AGENT_NAME.to_string(),
			title:   AGENT_TITLE.to_string(),
			version: env!("CARGO_PKG_VERSION").to_string(),
		}
	}
}

/// Client file-system capability gates (`fs/*` are called only when true).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct FileSystemCapabilities {
	pub read_text_file:  bool,
	pub write_text_file: bool,
}

/// Client authentication capability gates.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AuthCapabilities {
	pub terminal: bool,
}

/// Client elicitation capabilities. `form` must be present for
/// `elicitation/create` to be callable.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ElicitationCapabilities {
	pub form: Option<Value>,
}

/// `initialize.params.clientCapabilities`, as far as this agent reads it.
///
/// Unknown members are ignored; a missing member keeps its `false`/`None`
/// default so an absent capability means "not offered".
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ClientCapabilities {
	pub fs:          FileSystemCapabilities,
	pub terminal:    bool,
	pub auth:        AuthCapabilities,
	pub elicitation: Option<ElicitationCapabilities>,
}

impl ClientCapabilities {
	/// True when the client accepts `elicitation/create`.
	pub fn elicitation_form(&self) -> bool {
		self.elicitation.as_ref().is_some_and(|elicitation| elicitation.form.is_some())
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn classifies_requests_notifications_and_responses() {
		let request = parse_line(r#"{"jsonrpc":"2.0","id":"s-1","method":"ping","params":{"n":1}}"#)
			.expect("parse")
			.expect("frame");
		assert_eq!(
			request,
			Incoming::Request {
				id:     JsonRpcId::String("s-1".to_string()),
				method: "ping".to_string(),
				params: Some(json!({ "n": 1 })),
			}
		);
		assert_eq!(
			parse_line(r#"{"jsonrpc":"2.0","method":"session/update"}"#).expect("parse"),
			Some(Incoming::Notification {
				method: "session/update".to_string(),
				params: None,
			})
		);
		assert_eq!(
			parse_line(r#"{"jsonrpc":"2.0","id":7,"result":{"ok":true}}"#).expect("parse"),
			Some(Incoming::Response {
				id:      JsonRpcId::number(7),
				outcome: Ok(json!({ "ok": true })),
			})
		);
	}

	#[test]
	fn numeric_ids_keep_their_value_and_compare_like_javascript() {
		let fractional = parse_line(r#"{"jsonrpc":"2.0","id":1.5,"result":null}"#)
			.expect("parse")
			.expect("frame");
		let Incoming::Response { id, .. } = fractional else {
			panic!("expected response");
		};
		assert_eq!(id.as_json(), json!(1.5));
		assert_eq!(JsonRpcId::from_json(&json!(1.0)), Some(JsonRpcId::number(1)));
		assert_ne!(JsonRpcId::number(1), JsonRpcId::number(2));
		assert_eq!(JsonRpcId::from_json(&json!(1e30)).expect("id").as_json(), json!(1e30));
	}

	#[test]
	fn error_responses_keep_code_message_and_data() {
		let response = parse_line(
			r#"{"jsonrpc":"2.0","id":3,"error":{"code":-32601,"message":"nope","data":{"method":"x"}}}"#,
		)
		.expect("parse")
		.expect("frame");
		let Incoming::Response { outcome: Err(error), .. } = response else {
			panic!("expected error response");
		};
		assert_eq!(error.code, METHOD_NOT_FOUND);
		assert_eq!(error.message, "nope");
		assert_eq!(error.data, Some(json!({ "method": "x" })));
	}

	#[test]
	fn parse_and_envelope_failures_name_the_id_to_answer() {
		assert_eq!(parse_line("not json").expect_err("parse").id, None);
		assert!(!parse_line("not json").expect_err("parse").response_shaped);
		let error = parse_line(r#"{"jsonrpc":"1.0","id":4,"method":"ping"}"#).expect_err("envelope");
		assert_eq!(error.id, Some(JsonRpcId::number(4)));
		assert_eq!(error.error.code, INVALID_REQUEST);
		assert!(!error.response_shaped, "a malformed request is not an answer");
		let error = parse_line(r#"{"jsonrpc":"1.0","id":4}"#).expect_err("envelope");
		assert_eq!(error.error.code, INVALID_REQUEST);
		assert!(error.response_shaped, "no method means a malformed answer");
		let error = parse_line(r#"{"jsonrpc":"2.0","id":{},"method":"ping"}"#).expect_err("id");
		assert_eq!(error.error.code, INVALID_REQUEST);
		assert_eq!(error.id, None);
		let error = parse_line(r#"{"jsonrpc":"2.0","id":8}"#).expect_err("response without result");
		assert_eq!(error.id, Some(JsonRpcId::number(8)));
		assert!(error.response_shaped);
		// Upstream ignores envelopes with neither method nor id.
		assert_eq!(parse_line(r#"{"jsonrpc":"2.0"}"#).expect("parse"), None);
	}

	#[test]
	fn frames_omit_absent_params_and_normalise_null_results() {
		let id = JsonRpcId::number(2);
		assert_eq!(
			request_frame(&id, "ping", None),
			json!({ "jsonrpc": "2.0", "id": 2, "method": "ping" })
		);
		assert_eq!(
			notification_frame("session/update", Some(&json!({ "x": 1 }))),
			json!({ "jsonrpc": "2.0", "method": "session/update", "params": { "x": 1 } })
		);
		assert_eq!(
			result_frame(&id, Value::Null),
			json!({ "jsonrpc": "2.0", "id": 2, "result": {} })
		);
		assert_eq!(
			error_frame(None, &RequestError::parse_error()),
			json!({
				"jsonrpc": "2.0",
				"id": Value::Null,
				"error": { "code": -32700, "message": "Parse error" }
			})
		);
	}

	#[test]
	fn client_capabilities_default_to_not_offered() {
		let absent: ClientCapabilities = serde_json::from_value(json!({})).expect("caps");
		assert!(!absent.fs.read_text_file);
		assert!(!absent.terminal);
		assert!(!absent.auth.terminal);
		assert!(!absent.elicitation_form());
		let present: ClientCapabilities = serde_json::from_value(json!({
			"fs": { "readTextFile": true, "writeTextFile": false },
			"terminal": true,
			"auth": { "terminal": true },
			"elicitation": { "form": {} }
		}))
		.expect("caps");
		assert!(present.fs.read_text_file);
		assert!(!present.fs.write_text_file);
		assert!(present.terminal);
		assert!(present.auth.terminal);
		assert!(present.elicitation_form());
	}
}
