//! Wire and transcript types for the native eval tool.
//!
//! Field names mirror the TypeScript contract (`camelCase`) so persisted
//! sessions and hosts that already consume `EvalToolDetails` keep working.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Runtime backend selected by one eval cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EvalLanguage {
	#[serde(rename = "py")]
	Py,
	#[serde(rename = "js")]
	Js,
	#[serde(rename = "rb")]
	Rb,
	#[serde(rename = "jl")]
	Jl,
}

impl EvalLanguage {
	/// Stable display order used by the schema and discovery corpus.
	pub const ALL: [Self; 4] = [Self::Py, Self::Js, Self::Rb, Self::Jl];

	pub fn token(self) -> &'static str {
		match self {
			Self::Py => "py",
			Self::Js => "js",
			Self::Rb => "rb",
			Self::Jl => "jl",
		}
	}

	/// Long name used by transcript renderers and approval text.
	pub fn runtime_name(self) -> &'static str {
		match self {
			Self::Py => "python",
			Self::Js => "js",
			Self::Rb => "ruby",
			Self::Jl => "julia",
		}
	}

	pub fn display_name(self) -> &'static str {
		match self {
			Self::Py => "Python",
			Self::Js => "JavaScript",
			Self::Rb => "Ruby",
			Self::Jl => "Julia",
		}
	}

	pub fn parse(token: &str) -> Option<Self> {
		Self::ALL.into_iter().find(|language| language.token() == token)
	}
}

/// Status event emitted by a prelude helper or a tool-bridge call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalStatusEvent {
	pub op: String,
	#[serde(flatten)]
	pub fields: Map<String, Value>,
}

impl EvalStatusEvent {
	pub fn new(op: impl Into<String>) -> Self {
		Self { op: op.into(), fields: Map::new() }
	}

	/// Build from the `application/x-omp-status` bundle value.
	pub fn from_bundle(value: &Value) -> Option<Self> {
		let object = value.as_object()?;
		let op = object.get("op")?.as_str()?.to_string();
		let mut fields = object.clone();
		fields.remove("op");
		Some(Self { op, fields })
	}

	/// Upsert by `op`, merging fields (TypeScript `upsertStatusEvent`).
	pub fn upsert(events: &mut Vec<Self>, incoming: Self) {
		if let Some(existing) = events.iter_mut().find(|event| event.op == incoming.op) {
			existing.fields.extend(incoming.fields);
			return;
		}
		events.push(incoming);
	}
}

/// Rich output captured from a kernel MIME bundle.
#[derive(Debug, Clone, PartialEq)]
pub enum EvalDisplayOutput {
	Json(Value),
	Image { data: String, mime_type: String },
	Markdown { text: Option<String> },
	Status(EvalStatusEvent),
}

/// Typed image payload surfaced under `details.images`.
///
/// The host owns the model-visible image path; this payload is the exact
/// OpenAI-style block it needs. `ToolResult` itself carries text only, so the
/// parent wires `details.images` into `ContentPart::Image`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalImage {
	#[serde(rename = "type")]
	pub kind: ImageKind,
	pub data: String,
	#[serde(rename = "mimeType")]
	pub mime_type: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImageKind {
	#[serde(rename = "image")]
	Image,
}

impl EvalImage {
	pub fn new(data: impl Into<String>, mime_type: impl Into<String>) -> Self {
		Self {
			kind: ImageKind::Image,
			data: data.into(),
			mime_type: mime_type.into(),
		}
	}

	/// OpenAI content-part value for the parent's model-image wiring.
	pub fn to_content_value(&self) -> Value {
		serde_json::json!({
			"type": "image",
			"data": self.data,
			"mimeType": self.mime_type,
		})
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CellStatus {
	Pending,
	Running,
	Complete,
	Error,
}

/// One executed cell (always exactly one in the current schema).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvalCellResult {
	pub index: usize,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub title: Option<String>,
	pub code: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub language: Option<EvalLanguage>,
	pub output: String,
	pub status: CellStatus,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub duration_ms: Option<u64>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub exit_code: Option<i32>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub status_events: Option<Vec<EvalStatusEvent>>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub has_markdown: Option<bool>,
}

/// Structured result payload (TypeScript `EvalToolDetails`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvalToolDetails {
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub cells: Option<Vec<EvalCellResult>>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub json_outputs: Option<Vec<Value>>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub images: Option<Vec<EvalImage>>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub status_events: Option<Vec<EvalStatusEvent>>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub is_error: Option<bool>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub meta: Option<OutputMeta>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub language: Option<EvalLanguage>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub languages: Option<Vec<EvalLanguage>>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub notice: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub r#async: Option<AsyncJob>,
}

/// Output truncation/artifact metadata (TypeScript `OutputMeta`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutputMeta {
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub artifact_id: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub artifact_path: Option<String>,
	pub total_bytes: u64,
	pub total_lines: u64,
	pub output_bytes: usize,
	pub output_lines: usize,
	pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AsyncJob {
	pub state: AsyncState,
	pub job_id: String,
	#[serde(rename = "type")]
	pub kind: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AsyncState {
	Running,
	Completed,
	Failed,
}

/// Frame emitted by every subprocess runner (NDJSON).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KernelFrame {
	#[serde(rename = "type")]
	pub kind: String,
	#[serde(default)]
	pub id: Option<String>,
	#[serde(default)]
	pub data: Option<String>,
	#[serde(default)]
	pub bundle: Option<Map<String, Value>>,
	#[serde(default)]
	pub ename: Option<String>,
	#[serde(default)]
	pub evalue: Option<String>,
	#[serde(default)]
	pub traceback: Option<Vec<String>>,
	#[serde(default)]
	pub status: Option<String>,
	#[serde(default)]
	pub execution_count: Option<i64>,
	#[serde(default)]
	pub cancelled: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KernelStatus {
	Ok,
	Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelError {
	pub name: String,
	pub value: String,
	pub traceback: Vec<String>,
}

/// One settled kernel execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelExecuteResult {
	pub status: KernelStatus,
	pub execution_count: Option<i64>,
	pub error: Option<KernelError>,
	pub cancelled: bool,
	pub timed_out: bool,
	pub stdin_requested: bool,
	pub kernel_killed: bool,
}

/// Callbacks for a single execution.
pub struct KernelExecuteOptions<'a> {
	/// Caller-chosen id; the bridge registers `(session, run)` before writing.
	pub run_id: String,
	/// Working directory applied before this request.
	pub cwd: &'a std::path::Path,
	/// Managed runtime environment (`PI_*`) applied per request.
	pub env: &'a std::collections::HashMap<String, String>,
	pub silent: bool,
	pub store_history: bool,
	pub cancellation: tokio_util::sync::CancellationToken,
	/// Streamed plain output (stdout + stderr + rich text).
	pub on_chunk: Option<Box<dyn FnMut(&str) + Send + Sync + 'a>>,
	/// Rich display items in arrival order.
	pub on_display: Option<Box<dyn FnMut(EvalDisplayOutput) + Send + Sync + 'a>>,
}

/// Why an eval call failed before or instead of producing a cell.
#[derive(Debug, thiserror::Error)]
pub enum EvalError {
	#[error("Unknown eval language `{0}`")]
	UnknownLanguage(String),
	#[error("Eval backend unavailable: {0}")]
	Unavailable(String),
	#[error("Eval kernel failed: {0}")]
	Kernel(String),
	#[error("Eval bridge: {0}")]
	Bridge(String),
	#[error("Invalid eval arguments: {0}")]
	InvalidArguments(String),
	#[error("Eval operation aborted: {0}")]
	Aborted(String),
	#[error("Eval I/O: {0}")]
	Io(#[from] std::io::Error),
}
