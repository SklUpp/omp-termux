//! Debug Adapter Protocol payload shapes.
//!
//! Wire responses are typed only where the tool reads them; per-request
//! argument bodies are built as JSON because adapters accept adapter-specific
//! extension fields that must pass through untouched.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Lifecycle of one debug session, matching the TypeScript union.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionStatus {
	Launching,
	Configuring,
	Stopped,
	Running,
	Terminated,
}

impl SessionStatus {
	pub fn as_str(self) -> &'static str {
		match self {
			Self::Launching => "launching",
			Self::Configuring => "configuring",
			Self::Stopped => "stopped",
			Self::Running => "running",
			Self::Terminated => "terminated",
		}
	}
}

/// Adapter capabilities. `initialize` returns an open-ended object; unknown
/// keys stay available for `custom_request` consumers and summaries.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Capabilities(pub Value);

impl Capabilities {
	pub fn supports(&self, name: &str) -> bool {
		self.0.get(name).and_then(Value::as_bool) == Some(true)
	}

	pub fn as_value(&self) -> &Value {
		&self.0
	}
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Source {
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub name: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub path: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub source_reference: Option<i64>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub presentation_hint: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub origin: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Breakpoint {
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub id: Option<i64>,
	#[serde(default)]
	pub verified: bool,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub message: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub source: Option<Source>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub line: Option<i64>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub column: Option<i64>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub instruction_reference: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub offset: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StackFrame {
	pub id: i64,
	#[serde(default)]
	pub name: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub source: Option<Source>,
	#[serde(default)]
	pub line: i64,
	#[serde(default)]
	pub column: i64,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub instruction_pointer_reference: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Thread {
	pub id: i64,
	#[serde(default)]
	pub name: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Scope {
	#[serde(default)]
	pub name: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub presentation_hint: Option<String>,
	#[serde(default)]
	pub variables_reference: i64,
	#[serde(default)]
	pub expensive: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Variable {
	#[serde(default)]
	pub name: String,
	#[serde(default)]
	pub value: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub r#type: Option<String>,
	#[serde(default)]
	pub variables_reference: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DisassembledInstruction {
	#[serde(default)]
	pub address: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub instruction_bytes: Option<String>,
	#[serde(default)]
	pub instruction: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub symbol: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub location: Option<Source>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub line: Option<i64>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub column: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Module {
	pub id: Value,
	#[serde(default)]
	pub name: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub path: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub symbol_status: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub address_range: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvaluateResponse {
	#[serde(default)]
	pub result: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub r#type: Option<String>,
	#[serde(default)]
	pub variables_reference: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DataBreakpointInfoResponse {
	#[serde(default)]
	pub data_id: Option<String>,
	#[serde(default)]
	pub description: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub access_types: Option<Vec<String>>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub can_persist: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadMemoryResponse {
	#[serde(default)]
	pub address: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub data: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub unreadable_bytes: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteMemoryResponse {
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub offset: Option<i64>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub bytes_written: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BreakpointsResponse {
	#[serde(default)]
	pub breakpoints: Vec<Breakpoint>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StackTraceResponse {
	#[serde(default, rename = "stackFrames")]
	pub stack_frames: Vec<StackFrame>,
	#[serde(default, rename = "totalFrames")]
	pub total_frames: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScopesResponse {
	#[serde(default)]
	pub scopes: Vec<Scope>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VariablesResponse {
	#[serde(default)]
	pub variables: Vec<Variable>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ThreadsResponse {
	#[serde(default)]
	pub threads: Vec<Thread>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DisassembleResponse {
	#[serde(default)]
	pub instructions: Vec<DisassembledInstruction>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModulesResponse {
	#[serde(default)]
	pub modules: Vec<Module>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SourcesResponse {
	#[serde(default)]
	pub sources: Vec<Source>,
}

/// Desired source breakpoint kept in manager state.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BreakpointRecord {
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub id: Option<i64>,
	#[serde(default)]
	pub verified: bool,
	pub line: i64,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub condition: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub message: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FunctionBreakpointRecord {
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub id: Option<i64>,
	#[serde(default)]
	pub verified: bool,
	pub name: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub condition: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub message: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstructionBreakpointRecord {
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub id: Option<i64>,
	#[serde(default)]
	pub verified: bool,
	pub instruction_reference: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub offset: Option<i64>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub condition: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub hit_condition: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub message: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DataBreakpointRecord {
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub id: Option<i64>,
	#[serde(default)]
	pub verified: bool,
	pub data_id: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub access_type: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub condition: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub hit_condition: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub message: Option<String>,
}

/// Cached stop location for the active frame.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StopLocation {
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub thread_id: Option<i64>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub frame_id: Option<i64>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub reason: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub description: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub text: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub frame_name: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub instruction_pointer_reference: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub source: Option<Source>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub line: Option<i64>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub column: Option<i64>,
}

/// Tool-facing session summary (`details.snapshot` / `details.sessions`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
	pub id: String,
	pub adapter: String,
	pub cwd: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub program: Option<String>,
	pub status: SessionStatus,
	pub launched_at: String,
	pub last_used_at: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub thread_id: Option<i64>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub frame_id: Option<i64>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub stop_reason: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub stop_description: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub frame_name: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub instruction_pointer_reference: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub source: Option<Source>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub line: Option<i64>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub column: Option<i64>,
	pub breakpoint_files: usize,
	pub breakpoint_count: usize,
	pub function_breakpoint_count: usize,
	pub output_bytes: u64,
	pub output_truncated: bool,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub exit_code: Option<i64>,
	pub needs_configuration_done: bool,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub parent_session_id: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub child_session_ids: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContinueState {
	Running,
	Stopped,
	Terminated,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContinueOutcome {
	pub snapshot: SessionSummary,
	pub state: ContinueState,
	pub timed_out: bool,
}

/// Parsed `stopped` event body.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoppedEventBody {
	#[serde(default)]
	pub reason: String,
	#[serde(default)]
	pub description: Option<String>,
	#[serde(default)]
	pub thread_id: Option<i64>,
	#[serde(default)]
	pub text: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutputEventBody {
	#[serde(default)]
	pub output: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExitedEventBody {
	#[serde(default)]
	pub exit_code: Option<i64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContinuedEventBody {
	#[serde(default)]
	pub thread_id: Option<i64>,
}

/// Status for `runInTerminal` requests; the debuggee is detached.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunInTerminalArguments {
	#[serde(default)]
	pub kind: Option<String>,
	#[serde(default)]
	pub title: Option<String>,
	#[serde(default)]
	pub cwd: Option<String>,
	#[serde(default)]
	pub args: Vec<String>,
	#[serde(default)]
	pub env: std::collections::HashMap<String, Option<String>>,
}

/// Reverse `startDebugging` payload.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct StartDebuggingArguments {
	#[serde(default)]
	pub request: Option<String>,
	#[serde(default)]
	pub configuration: Value,
}
