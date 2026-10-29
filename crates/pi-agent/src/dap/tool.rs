//! `debug` tool: one DAP session tree driven by the upstream 28-action switch.
//!
//! Port of `packages/coding-agent/src/tools/debug.ts`. Every action sends a
//! real DAP request through [`DapSessionManager`]; nothing here is simulated.
//! Read-only actions are listed in [`DEBUG_READONLY_ACTIONS`] so the parent
//! registry can classify approval as `read` versus `exec`.

use std::{
	path::{Path, PathBuf},
	sync::Arc,
	time::{Duration, Instant},
};

use async_trait::async_trait;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use super::{
	client::DapError,
	config::{
		LaunchAdapterSelection, LaunchProgramKind, adapter_unavailable_message, classify_program,
		configured_adapter_names, get_adapter_configs, resolve_launch_overrides, select_attach_adapter,
		select_launch_adapter,
	},
	session::DapSessionManager,
	types::{
		BreakpointRecord, ContinueOutcome, ContinueState, DataBreakpointInfoResponse,
		DataBreakpointRecord, DisassembledInstruction, EvaluateResponse, FunctionBreakpointRecord,
		InstructionBreakpointRecord, Module, Scope, SessionSummary, Source, StackFrame, Thread, Variable,
	},
};
use crate::state::{AgentContext, ToolResult};
use crate::tools::{Tool, ToolError};

/// Actions that only read program state. Everything else mutates the debuggee
/// or the session tree and therefore needs exec approval.
pub const DEBUG_READONLY_ACTIONS: [&str; 10] = [
	"output",
	"threads",
	"stack_trace",
	"scopes",
	"variables",
	"disassemble",
	"read_memory",
	"loaded_sources",
	"modules",
	"sessions",
];

/// Approval tier for a `debug` action (unknown actions count as exec).
pub fn is_readonly_action(action: &str) -> bool {
	DEBUG_READONLY_ACTIONS.contains(&action.to_ascii_lowercase().as_str())
}

const DESCRIPTION: &str = "Debugger access. Prefer over bash for program state, breakpoints, stepping, or thread inspection.\nOnly one active session at a time. `program` is a target path, not a shell command.\nDirectories need a directory-capable adapter (e.g. `dlv`).";

const ACTIONS: [&str; 28] = [
	"launch",
	"attach",
	"set_breakpoint",
	"remove_breakpoint",
	"set_instruction_breakpoint",
	"remove_instruction_breakpoint",
	"data_breakpoint_info",
	"set_data_breakpoint",
	"remove_data_breakpoint",
	"continue",
	"step_over",
	"step_in",
	"step_out",
	"pause",
	"evaluate",
	"stack_trace",
	"threads",
	"scopes",
	"variables",
	"disassemble",
	"read_memory",
	"write_memory",
	"modules",
	"loaded_sources",
	"custom_request",
	"output",
	"terminate",
	"sessions",
];

/// Default/min/max timeout seconds, matching `TOOL_TIMEOUTS.debug`.
const DEFAULT_TIMEOUT_SECS: f64 = 30.0;
const MIN_TIMEOUT_SECS: f64 = 5.0;
const MAX_TIMEOUT_SECS: f64 = 300.0;

pub struct DapTool {
	/// Root session tree. The runner template owns one `Arc` and hands it to
	/// every per-prompt tool instance so debug sessions survive turns.
	/// Subagents deliberately get a fresh manager (`DapTool::new`) and cannot
	/// see or terminate the parent's debuggee.
	manager: Arc<DapSessionManager>,
	/// `tools.maxTimeout` global ceiling, when the host exposes one.
	max_timeout: Option<u64>,
}

impl DapTool {
	/// Fresh, isolated session tree (child agents and standalone hosts).
	pub fn new() -> Self {
		Self { manager: Arc::new(DapSessionManager::new()), max_timeout: None }
	}

	/// Share the root session tree owned by `RunnerSession`.
	pub fn shared(manager: Arc<DapSessionManager>) -> Self {
		Self { manager, max_timeout: None }
	}

	/// Apply the host's `tools.maxTimeout` cap. `None`/`0` disables the cap.
	#[must_use]
	pub fn with_max_timeout(mut self, seconds: Option<u64>) -> Self {
		self.max_timeout = seconds.filter(|seconds| *seconds > 0);
		self
	}

	pub fn manager(&self) -> &DapSessionManager {
		self.manager.as_ref()
	}
}

impl Default for DapTool {
	fn default() -> Self {
		Self::new()
	}
}

#[async_trait]
impl Tool for DapTool {
	fn name(&self) -> &str {
		"debug"
	}

	fn description(&self) -> &str {
		DESCRIPTION
	}

	fn schema(&self) -> Value {
		json!({
			"type": "object",
			"properties": {
				"action": {
					"type": "string",
					"enum": ACTIONS,
					"description": "debug operation to perform"
				},
				"program": { "type": "string", "description": "debug target path; Delve accepts Go package directories" },
				"args": { "type": "array", "items": { "type": "string" }, "description": "program arguments" },
				"adapter": { "type": "string", "description": "configured adapter id (gdb, lldb-dap, debugpy, dlv, rdbg, or dap.json entry)" },
				"cwd": { "type": "string" },
				"file": { "type": "string", "description": "source file" },
				"line": { "type": "number", "description": "source line" },
				"function": { "type": "string", "description": "function name" },
				"name": { "type": "string", "description": "variable or data name" },
				"condition": { "type": "string", "description": "breakpoint condition" },
				"hit_condition": { "type": "string" },
				"expression": { "type": "string", "description": "expression to evaluate" },
				"context": { "type": "string", "description": "evaluate context: watch | repl | hover | variables | clipboard" },
				"frame_id": { "type": "number" },
				"scope_id": { "type": "number", "description": "scope variables reference" },
				"variable_ref": { "type": "number", "description": "variable reference" },
				"pid": { "type": "number", "description": "process id for attach" },
				"port": { "type": "number", "description": "remote attach port" },
				"host": { "type": "string", "description": "remote attach host" },
				"levels": { "type": "number", "description": "max stack frames" },
				"memory_reference": { "type": "string", "description": "memory reference or address" },
				"instruction_reference": { "type": "string" },
				"instruction_count": { "type": "number" },
				"instruction_offset": { "type": "number" },
				"count": { "type": "number", "description": "bytes to read" },
				"data": { "type": "string", "description": "base64 memory payload" },
				"data_id": { "type": "string", "description": "data breakpoint id" },
				"access_type": { "type": "string", "enum": ["read", "write", "readWrite"] },
				"command": { "type": "string", "description": "custom dap request command" },
				"arguments": { "type": "object", "description": "custom request arguments" },
				"offset": { "type": "number" },
				"resolve_symbols": { "type": "boolean" },
				"allow_partial": { "type": "boolean" },
				"start_module": { "type": "number" },
				"module_count": { "type": "number" },
				"timeout": { "type": "number", "description": "per-request timeout seconds" }
			},
			"required": ["action"],
			"additionalProperties": false
		})
	}

	fn is_exclusive(&self) -> bool {
		true
	}

	async fn execute(&self, args: &Value, ctx: &AgentContext) -> Result<ToolResult, ToolError> {
		let params = args
			.as_object()
			.ok_or_else(|| ToolError::InvalidArguments("Arguments must be an object".to_string()))?;
		let action = params
			.get("action")
			.and_then(Value::as_str)
			.ok_or_else(|| ToolError::InvalidArguments("action is required".to_string()))?;
		let timeout_secs = resolve_timeout(params.get("timeout").and_then(Value::as_f64), self.max_timeout);
		let deadline = Instant::now() + Duration::from_secs(timeout_secs);
		let cancel = ctx.cancellation_token.child_token();
		let env = &ctx.env;
		let manager = self.manager.as_ref();
		let session_cwd = ctx.cwd.as_path();
		let mut details = Map::new();
		details.insert("action".to_string(), Value::String(action.to_string()));
		details.insert("success".to_string(), Value::Bool(true));

		match action {
			"launch" => {
				let program_arg = params
					.get("program")
					.and_then(Value::as_str)
					.ok_or_else(|| invalid("program is required for launch"))?;
				let command_cwd = match params.get("cwd").and_then(Value::as_str) {
					Some(cwd) => resolve_to_cwd(cwd, session_cwd),
					None => session_cwd.to_path_buf(),
				};
				let program = resolve_to_cwd(program_arg, &command_cwd);
				let program_kind = classify_program(&program);
				let adapter_name = params.get("adapter").and_then(Value::as_str);
				let selection =
					select_launch_adapter(&program, &command_cwd, env, adapter_name, program_kind);
				let adapter = match selection {
					LaunchAdapterSelection::Adapter(adapter) => adapter,
					LaunchAdapterSelection::Unavailable { adapter_name, command } => {
						return Err(invalid(adapter_unavailable_message(
							&adapter_name,
							&command,
							&command_cwd,
							env,
						)));
					},
					LaunchAdapterSelection::None => {
						return Err(invalid(no_adapter_message(&command_cwd, env)));
					},
				};
				if program_kind == LaunchProgramKind::Directory && !adapter.accepts_directory_program {
					return Err(invalid(format!(
						"launch program resolves to a directory: {}. Pass an executable file path or choose an adapter that supports package directories.",
						program.display()
					)));
				}
				let extra_launch_arguments = resolve_launch_overrides(&adapter, &program, program_kind);
				let options = super::session::LaunchOptions {
					adapter: adapter.clone(),
					program: program.to_string_lossy().into_owned(),
					args: string_array(params.get("args")),
					cwd: command_cwd,
					extra_launch_arguments,
					env: env.clone(),
				};
				let snapshot = manager.launch(options, &cancel, deadline).await.map_err(to_tool_error)?;
				details.insert("snapshot".to_string(), snapshot_value(&snapshot));
				details.insert("adapter".to_string(), Value::String(adapter.name.clone()));
				Ok(finish(details, format_session_snapshot(&snapshot).join("\n")))
			},
			"attach" => {
				let pid = optional_i64(params, "pid")?;
				let port = optional_port(params, "port")?;
				let adapter_name = params.get("adapter").and_then(Value::as_str);
				if pid.is_none() && port.is_none() && adapter_name.is_none() {
					return Err(invalid("attach requires pid or port"));
				}
				let command_cwd = match params.get("cwd").and_then(Value::as_str) {
					Some(cwd) => resolve_to_cwd(cwd, session_cwd),
					None => session_cwd.to_path_buf(),
				};
				let adapter = match select_attach_adapter(&command_cwd, env, adapter_name, port) {
					Some(adapter) => adapter,
					None => {
						if let Some(name) = adapter_name {
							let command = get_adapter_configs(&command_cwd, env)
								.get(name)
								.map(|config| config.command.clone())
								.unwrap_or_else(|| name.to_string());
							return Err(invalid(adapter_unavailable_message(name, &command, &command_cwd, env)));
						}
						return Err(invalid(no_adapter_message(&command_cwd, env)));
					},
				};
				let options = super::session::AttachOptions {
					adapter: adapter.clone(),
					cwd: command_cwd,
					pid,
					port,
					host: params.get("host").and_then(Value::as_str).map(str::to_string),
					env: env.clone(),
				};
				let snapshot = manager.attach(options, &cancel, deadline).await.map_err(to_tool_error)?;
				details.insert("snapshot".to_string(), snapshot_value(&snapshot));
				details.insert("adapter".to_string(), Value::String(adapter.name.clone()));
				Ok(finish(details, format_session_snapshot(&snapshot).join("\n")))
			},
			"set_breakpoint" => {
				if let Some(function) = params.get("function").and_then(Value::as_str) {
					let response = manager
						.set_function_breakpoint(
							function,
							optional_string(params, "condition"),
							&cancel,
							deadline,
						)
						.await
						.map_err(to_tool_error)?;
					details.insert("snapshot".to_string(), snapshot_value(&response.snapshot));
					details.insert("functionBreakpoints".to_string(), value(&response.breakpoints));
					return Ok(finish(details, format_function_breakpoints(&response.breakpoints)));
				}
				let (file, line) = breakpoint_target(params, session_cwd, "set_breakpoint")?;
				let response = manager
					.set_breakpoint(&file, line, optional_string(params, "condition"), &cancel, deadline)
					.await
					.map_err(to_tool_error)?;
				details.insert("snapshot".to_string(), snapshot_value(&response.snapshot));
				details.insert("breakpoints".to_string(), value(&response.breakpoints));
				Ok(finish(details, format_breakpoints(&response.source_path, &response.breakpoints)))
			},
			"remove_breakpoint" => {
				if let Some(function) = params.get("function").and_then(Value::as_str) {
					let response = manager
						.remove_function_breakpoint(function, &cancel, deadline)
						.await
						.map_err(to_tool_error)?;
					details.insert("snapshot".to_string(), snapshot_value(&response.snapshot));
					details.insert("functionBreakpoints".to_string(), value(&response.breakpoints));
					return Ok(finish(details, format_function_breakpoints(&response.breakpoints)));
				}
				let (file, line) = breakpoint_target(params, session_cwd, "remove_breakpoint")?;
				let response = manager
					.remove_breakpoint(&file, line, &cancel, deadline)
					.await
					.map_err(to_tool_error)?;
				details.insert("snapshot".to_string(), snapshot_value(&response.snapshot));
				details.insert("breakpoints".to_string(), value(&response.breakpoints));
				Ok(finish(details, format_breakpoints(&response.source_path, &response.breakpoints)))
			},
			"set_instruction_breakpoint" => {
				require_capability(manager, "supportsInstructionBreakpoints", "instruction breakpoints")?;
				let reference = params
					.get("instruction_reference")
					.and_then(Value::as_str)
					.ok_or_else(|| invalid("instruction_reference is required for set_instruction_breakpoint"))?;
				let response = manager
					.set_instruction_breakpoint(
						reference,
						optional_i64(params, "offset")?,
						optional_string(params, "condition"),
						optional_string(params, "hit_condition"),
						&cancel,
						deadline,
					)
					.await
					.map_err(to_tool_error)?;
				details.insert("snapshot".to_string(), snapshot_value(&response.snapshot));
				details.insert("instructionBreakpoints".to_string(), value(&response.breakpoints));
				Ok(finish(details, format_instruction_breakpoints(&response.breakpoints)))
			},
			"remove_instruction_breakpoint" => {
				require_capability(manager, "supportsInstructionBreakpoints", "instruction breakpoints")?;
				let reference = params
					.get("instruction_reference")
					.and_then(Value::as_str)
					.ok_or_else(|| invalid("instruction_reference is required for remove_instruction_breakpoint"))?;
				let response = manager
					.remove_instruction_breakpoint(reference, optional_i64(params, "offset")?, &cancel, deadline)
					.await
					.map_err(to_tool_error)?;
				details.insert("snapshot".to_string(), snapshot_value(&response.snapshot));
				details.insert("instructionBreakpoints".to_string(), value(&response.breakpoints));
				Ok(finish(details, format_instruction_breakpoints(&response.breakpoints)))
			},
			"data_breakpoint_info" => {
				require_capability(manager, "supportsDataBreakpoints", "data breakpoints")?;
				let name = params
					.get("name")
					.and_then(Value::as_str)
					.ok_or_else(|| invalid("name is required for data_breakpoint_info"))?;
				let variables_reference = optional_i64(params, "variable_ref")?
					.or(optional_i64(params, "scope_id")?);
				let response = manager
					.data_breakpoint_info(
						name,
						variables_reference,
						optional_i64(params, "frame_id")?,
						&cancel,
						deadline,
					)
					.await
					.map_err(to_tool_error)?;
				details.insert("snapshot".to_string(), snapshot_value(&response.snapshot));
				details.insert("dataBreakpointInfo".to_string(), value(&response.info));
				Ok(finish(details, format_data_breakpoint_info(&response.info)))
			},
			"set_data_breakpoint" => {
				require_capability(manager, "supportsDataBreakpoints", "data breakpoints")?;
				let data_id = params
					.get("data_id")
					.and_then(Value::as_str)
					.ok_or_else(|| invalid("data_id is required for set_data_breakpoint"))?;
				let response = manager
					.set_data_breakpoint(
						data_id,
						optional_string(params, "access_type"),
						optional_string(params, "condition"),
						optional_string(params, "hit_condition"),
						&cancel,
						deadline,
					)
					.await
					.map_err(to_tool_error)?;
				details.insert("snapshot".to_string(), snapshot_value(&response.snapshot));
				details.insert("dataBreakpoints".to_string(), value(&response.breakpoints));
				Ok(finish(details, format_data_breakpoints(&response.breakpoints)))
			},
			"remove_data_breakpoint" => {
				require_capability(manager, "supportsDataBreakpoints", "data breakpoints")?;
				let data_id = params
					.get("data_id")
					.and_then(Value::as_str)
					.ok_or_else(|| invalid("data_id is required for remove_data_breakpoint"))?;
				let response = manager
					.remove_data_breakpoint(data_id, &cancel, deadline)
					.await
					.map_err(to_tool_error)?;
				details.insert("snapshot".to_string(), snapshot_value(&response.snapshot));
				details.insert("dataBreakpoints".to_string(), value(&response.breakpoints));
				Ok(finish(details, format_data_breakpoints(&response.breakpoints)))
			},
			"continue" => self.continue_action(manager, &cancel, deadline, timeout_secs, "Continue", details).await,
			"step_over" => {
				let outcome = manager.step_over(&cancel, deadline).await.map_err(to_tool_error)?;
				Ok(step_result(details, outcome, timeout_secs, "Step over"))
			},
			"step_in" => {
				let outcome = manager.step_in(&cancel, deadline).await.map_err(to_tool_error)?;
				Ok(step_result(details, outcome, timeout_secs, "Step in"))
			},
			"step_out" => {
				let outcome = manager.step_out(&cancel, deadline).await.map_err(to_tool_error)?;
				Ok(step_result(details, outcome, timeout_secs, "Step out"))
			},
			"pause" => {
				let snapshot = manager.pause(&cancel, deadline).await.map_err(to_tool_error)?;
				details.insert("snapshot".to_string(), snapshot_value(&snapshot));
				let mut lines = format_session_snapshot(&snapshot);
				lines.push("Program paused.".to_string());
				Ok(finish(details, lines.join("\n")))
			},
			"evaluate" => {
				let expression = params
					.get("expression")
					.and_then(Value::as_str)
					.ok_or_else(|| invalid("expression is required for evaluate"))?;
				let evaluation_context = params.get("context").and_then(Value::as_str).unwrap_or("repl");
				let response = manager
					.evaluate(
						expression,
						evaluation_context,
						optional_i64(params, "frame_id")?,
						&cancel,
						deadline,
					)
					.await
					.map_err(to_tool_error)?;
				details.insert("snapshot".to_string(), snapshot_value(&response.snapshot));
				details.insert("evaluation".to_string(), value(&response.evaluation));
				Ok(finish(details, format_evaluation(&response.evaluation)))
			},
			"stack_trace" => {
				let response = manager
					.stack_trace(optional_i64(params, "levels")?, &cancel, deadline)
					.await
					.map_err(to_tool_error)?;
				details.insert("snapshot".to_string(), snapshot_value(&response.snapshot));
				details.insert("stackFrames".to_string(), value(&response.stack_frames));
				Ok(finish(details, format_stack_frames(&response.stack_frames)))
			},
			"threads" => {
				let response = manager.threads(&cancel, deadline).await.map_err(to_tool_error)?;
				details.insert("snapshot".to_string(), snapshot_value(&response.snapshot));
				details.insert("threads".to_string(), value(&response.threads));
				Ok(finish(details, format_threads(&response.threads)))
			},
			"scopes" => {
				let response = manager
					.scopes(optional_i64(params, "frame_id")?, &cancel, deadline)
					.await
					.map_err(to_tool_error)?;
				details.insert("snapshot".to_string(), snapshot_value(&response.snapshot));
				details.insert("scopes".to_string(), value(&response.scopes));
				Ok(finish(details, format_scopes(&response.scopes)))
			},
			"variables" => {
				let variable_reference = optional_i64(params, "variable_ref")?
					.or(optional_i64(params, "scope_id")?)
					.ok_or_else(|| invalid("variables requires variable_ref or scope_id"))?;
				let response = manager
					.variables(variable_reference, &cancel, deadline)
					.await
					.map_err(to_tool_error)?;
				details.insert("snapshot".to_string(), snapshot_value(&response.snapshot));
				details.insert("variables".to_string(), value(&response.variables));
				Ok(finish(details, format_variables(&response.variables)))
			},
			"disassemble" => {
				require_capability(manager, "supportsDisassembleRequest", "disassembly")?;
				let instruction_count = optional_i64(params, "instruction_count")?
					.ok_or_else(|| invalid("instruction_count is required for disassemble"))?;
				let reference = resolve_disassembly_reference(params, manager)?;
				let response = manager
					.disassemble(
						&reference,
						instruction_count,
						optional_i64(params, "offset")?,
						optional_i64(params, "instruction_offset")?,
						params.get("resolve_symbols").and_then(Value::as_bool),
						&cancel,
						deadline,
					)
					.await
					.map_err(to_tool_error)?;
				details.insert("snapshot".to_string(), snapshot_value(&response.snapshot));
				details.insert("disassembly".to_string(), value(&response.instructions));
				Ok(finish(details, format_disassembly(&response.instructions)))
			},
			"read_memory" => {
				require_capability(manager, "supportsReadMemoryRequest", "memory reads")?;
				let reference = params
					.get("memory_reference")
					.and_then(Value::as_str)
					.ok_or_else(|| invalid("memory_reference is required for read_memory"))?;
				let count = optional_i64(params, "count")?
					.ok_or_else(|| invalid("count is required for read_memory"))?;
				let response = manager
					.read_memory(reference, count, optional_i64(params, "offset")?, &cancel, deadline)
					.await
					.map_err(to_tool_error)?;
				details.insert("snapshot".to_string(), snapshot_value(&response.snapshot));
				details.insert("memoryAddress".to_string(), Value::String(response.address.clone()));
				details.insert("memoryData".to_string(), option_value(&response.data));
				details.insert("unreadableBytes".to_string(), option_value(&response.unreadable_bytes));
				Ok(finish(
					details,
					format_memory_read(&response.address, response.data.as_deref(), response.unreadable_bytes),
				))
			},
			"write_memory" => {
				require_capability(manager, "supportsWriteMemoryRequest", "memory writes")?;
				let reference = params
					.get("memory_reference")
					.and_then(Value::as_str)
					.ok_or_else(|| invalid("memory_reference is required for write_memory"))?;
				let data = params
					.get("data")
					.and_then(Value::as_str)
					.ok_or_else(|| invalid("data is required for write_memory"))?;
				let response = manager
					.write_memory(
						reference,
						data,
						optional_i64(params, "offset")?,
						params.get("allow_partial").and_then(Value::as_bool),
						&cancel,
						deadline,
					)
					.await
					.map_err(to_tool_error)?;
				details.insert("snapshot".to_string(), snapshot_value(&response.snapshot));
				details.insert("bytesWritten".to_string(), option_value(&response.bytes_written));
				let mut lines = vec!["Memory write completed.".to_string()];
				if let Some(bytes_written) = response.bytes_written {
					lines.push(format!("Bytes written: {bytes_written}"));
				}
				if let Some(offset) = response.offset {
					lines.push(format!("Offset: {offset}"));
				}
				Ok(finish(details, lines.join("\n")))
			},
			"modules" => {
				require_capability(manager, "supportsModulesRequest", "module introspection")?;
				let response = manager
					.modules(
						optional_i64(params, "start_module")?,
						optional_i64(params, "module_count")?,
						&cancel,
						deadline,
					)
					.await
					.map_err(to_tool_error)?;
				details.insert("snapshot".to_string(), snapshot_value(&response.snapshot));
				details.insert("modules".to_string(), value(&response.modules));
				Ok(finish(details, format_modules(&response.modules)))
			},
			"loaded_sources" => {
				require_capability(manager, "supportsLoadedSourcesRequest", "loaded sources")?;
				let response = manager.loaded_sources(&cancel, deadline).await.map_err(to_tool_error)?;
				details.insert("snapshot".to_string(), snapshot_value(&response.snapshot));
				details.insert("sources".to_string(), value(&response.sources));
				Ok(finish(details, format_loaded_sources(&response.sources)))
			},
			"custom_request" => {
				let command = params
					.get("command")
					.and_then(Value::as_str)
					.ok_or_else(|| invalid("command is required for custom_request"))?;
				let arguments = params.get("arguments").cloned();
				let response = manager
					.custom_request(command, arguments, &cancel, deadline)
					.await
					.map_err(to_tool_error)?;
				details.insert("snapshot".to_string(), snapshot_value(&response.snapshot));
				details.insert("customBody".to_string(), response.body.clone());
				Ok(finish(details, format_custom_response(command, &response.body)))
			},
			"output" => {
				let response = manager.get_output().map_err(to_tool_error)?;
				details.insert("snapshot".to_string(), snapshot_value(&response.snapshot));
				details.insert("output".to_string(), Value::String(response.output.clone()));
				let text = if response.output.is_empty() {
					"(no output captured)".to_string()
				} else {
					response.output
				};
				Ok(finish(details, text))
			},
			"terminate" => {
				let snapshot = manager.terminate(&cancel, deadline).await.map_err(to_tool_error)?;
				let Some(snapshot) = snapshot else {
					return Ok(finish(details, "No debug session to terminate.".to_string()));
				};
				details.insert("snapshot".to_string(), snapshot_value(&snapshot));
				let mut lines = format_session_snapshot(&snapshot);
				lines.push("Debug session terminated.".to_string());
				Ok(finish(details, lines.join("\n")))
			},
			"sessions" => {
				let sessions = manager.list_sessions();
				details.insert("sessions".to_string(), value(&sessions));
				Ok(finish(details, format_sessions(&sessions)))
			},
			other => Err(invalid(format!("Unsupported debug action: {other}"))),
		}
	}
}

impl DapTool {
	async fn continue_action(
		&self,
		manager: &DapSessionManager,
		cancel: &CancellationToken,
		deadline: Instant,
		timeout_secs: u64,
		verb: &str,
		mut details: Map<String, Value>,
	) -> Result<ToolResult, ToolError> {
		let outcome = manager.continue_execution(cancel, deadline).await.map_err(to_tool_error)?;
		details.insert("snapshot".to_string(), snapshot_value(&outcome.snapshot));
		details.insert("state".to_string(), state_value(outcome.state));
		details.insert("timedOut".to_string(), Value::Bool(outcome.timed_out));
		Ok(finish(details, outcome_text(&outcome, timeout_secs, verb)))
	}
}

fn step_result(
	mut details: Map<String, Value>,
	outcome: ContinueOutcome,
	timeout_secs: u64,
	verb: &str,
) -> ToolResult {
	details.insert("snapshot".to_string(), snapshot_value(&outcome.snapshot));
	details.insert("state".to_string(), state_value(outcome.state));
	details.insert("timedOut".to_string(), Value::Bool(outcome.timed_out));
	finish(details, outcome_text(&outcome, timeout_secs, verb))
}

fn state_value(state: ContinueState) -> Value {
	Value::String(
		match state {
			ContinueState::Running => "running",
			ContinueState::Stopped => "stopped",
			ContinueState::Terminated => "terminated",
		}
		.to_string(),
	)
}

fn finish(details: Map<String, Value>, content: String) -> ToolResult {
	ToolResult {
		tool_call_id: String::new(),
		content,
		is_error: false,
		terminate: false,
		details: Some(Value::Object(details)),
	}
}

fn invalid(message: impl Into<String>) -> ToolError {
	ToolError::InvalidArguments(message.into())
}

fn to_tool_error(error: DapError) -> ToolError {
	match error {
		DapError::Aborted => ToolError::Aborted("Operation aborted".to_string()),
		DapError::RequestTimeout { command, ms } => {
			ToolError::ExecutionFailed(format!("DAP request {command} timed out after {ms}ms"))
		},
		other => ToolError::ExecutionFailed(other.to_string()),
	}
}

fn resolve_timeout(raw: Option<f64>, max_timeout: Option<u64>) -> u64 {
	let requested = raw.filter(|value| value.is_finite()).unwrap_or(DEFAULT_TIMEOUT_SECS);
	let capped = match max_timeout {
		Some(limit) => requested.min(limit as f64),
		None => requested,
	};
	capped.clamp(MIN_TIMEOUT_SECS, MAX_TIMEOUT_SECS) as u64
}

fn resolve_to_cwd(path: &str, cwd: &Path) -> PathBuf {
	let candidate = Path::new(path);
	if candidate.is_absolute() {
		candidate.to_path_buf()
	} else {
		cwd.join(candidate)
	}
}

fn string_array(value: Option<&Value>) -> Option<Vec<String>> {
	let array = value?.as_array()?;
	Some(array.iter().filter_map(Value::as_str).map(str::to_string).collect())
}

fn optional_string(params: &Map<String, Value>, key: &str) -> Option<String> {
	params.get(key).and_then(Value::as_str).map(str::to_string)
}

fn optional_i64(params: &Map<String, Value>, key: &str) -> Result<Option<i64>, ToolError> {
	match params.get(key) {
		None | Some(Value::Null) => Ok(None),
		Some(value) => value
			.as_i64()
			.or_else(|| value.as_f64().filter(|number| number.is_finite()).map(|number| number as i64))
			.map(Some)
			.ok_or_else(|| invalid(format!("{key} must be a number"))),
	}
}

fn optional_port(params: &Map<String, Value>, key: &str) -> Result<Option<u16>, ToolError> {
	match optional_i64(params, key)? {
		None => Ok(None),
		Some(port) => u16::try_from(port)
			.map(Some)
			.map_err(|_| invalid(format!("{key} must be a valid TCP port"))),
	}
}

fn breakpoint_target(
	params: &Map<String, Value>,
	session_cwd: &Path,
	action: &str,
) -> Result<(String, i64), ToolError> {
	let file = params
		.get("file")
		.and_then(Value::as_str)
		.ok_or_else(|| invalid(format!("{action} requires file+line or function")))?;
	let line = optional_i64(params, "line")?
		.ok_or_else(|| invalid(format!("{action} requires file+line or function")))?;
	Ok((resolve_to_cwd(file, session_cwd).to_string_lossy().into_owned(), line))
}

fn require_capability(
	manager: &DapSessionManager,
	capability: &str,
	description: &str,
) -> Result<(), ToolError> {
	match manager.get_capabilities() {
		Some(capabilities) if capabilities.supports(capability) => Ok(()),
		_ => Err(ToolError::ExecutionFailed(format!(
			"Current adapter does not support {description}"
		))),
	}
}

fn resolve_disassembly_reference(
	params: &Map<String, Value>,
	manager: &DapSessionManager,
) -> Result<String, ToolError> {
	if let Some(reference) = params.get("memory_reference").and_then(Value::as_str) {
		return Ok(reference.to_string());
	}
	if let Some(snapshot) = manager.get_active_session() {
		if let Some(reference) = snapshot.instruction_pointer_reference {
			return Ok(reference);
		}
	}
	Err(invalid(
		"disassemble requires memory_reference unless the current stop location has an instruction pointer reference",
	))
}

fn no_adapter_message(cwd: &Path, env: &std::collections::HashMap<String, String>) -> String {
	let names = configured_adapter_names(cwd, env);
	let listed = if names.is_empty() { "none".to_string() } else { names.join(", ") };
	format!("No debugger adapter available. Installed adapters: {listed}")
}

fn value<T: serde::Serialize>(item: &T) -> Value {
	serde_json::to_value(item).unwrap_or(Value::Null)
}

fn option_value<T: serde::Serialize>(item: &Option<T>) -> Value {
	match item {
		Some(item) => value(item),
		None => Value::Null,
	}
}

fn snapshot_value(snapshot: &SessionSummary) -> Value {
	value(snapshot)
}

// ── renderers (port of the TypeScript formatting helpers) ──────────────────

fn format_location(snapshot: &SessionSummary) -> Option<String> {
	let path = snapshot.source.as_ref().and_then(|source| source.path.as_ref())?;
	let line = snapshot.line?;
	Some(match snapshot.column {
		Some(column) => format!("{path}:{line}:{column}"),
		None => format!("{path}:{line}"),
	})
}

fn format_session_snapshot(snapshot: &SessionSummary) -> Vec<String> {
	let mut lines = vec![
		format!("Session {}", snapshot.id),
		format!("Adapter: {}", snapshot.adapter),
		format!("Status: {}", snapshot.status.as_str()),
		format!("CWD: {}", snapshot.cwd),
	];
	if let Some(program) = &snapshot.program {
		lines.push(format!("Program: {program}"));
	}
	if let Some(reason) = &snapshot.stop_reason {
		lines.push(format!("Stop reason: {reason}"));
	}
	if let Some(frame) = &snapshot.frame_name {
		lines.push(format!("Frame: {frame}"));
	}
	if let Some(pointer) = &snapshot.instruction_pointer_reference {
		lines.push(format!("Instruction pointer: {pointer}"));
	}
	if let Some(location) = format_location(snapshot) {
		lines.push(format!("Location: {location}"));
	}
	if snapshot.needs_configuration_done {
		lines.push("Configuration: pending configurationDone; set breakpoints, then continue.".to_string());
	}
	if let Some(exit_code) = snapshot.exit_code {
		lines.push(format!("Exit code: {exit_code}"));
	}
	lines
}

fn format_breakpoints(file_path: &str, breakpoints: &[BreakpointRecord]) -> String {
	let mut lines = vec![format!("Breakpoints for {file_path}:")];
	if breakpoints.is_empty() {
		lines.push("(none)".to_string());
		return lines.join("\n");
	}
	for breakpoint in breakpoints {
		lines.push(format!(
			"- line {}: {}{}{}",
			breakpoint.line,
			verified_label(breakpoint.verified),
			condition_suffix(breakpoint.condition.as_deref()),
			message_suffix(breakpoint.message.as_deref()),
		));
	}
	lines.join("\n")
}

fn format_function_breakpoints(breakpoints: &[FunctionBreakpointRecord]) -> String {
	let mut lines = vec!["Function breakpoints:".to_string()];
	if breakpoints.is_empty() {
		lines.push("(none)".to_string());
		return lines.join("\n");
	}
	for breakpoint in breakpoints {
		lines.push(format!(
			"- {}: {}{}{}",
			breakpoint.name,
			verified_label(breakpoint.verified),
			condition_suffix(breakpoint.condition.as_deref()),
			message_suffix(breakpoint.message.as_deref()),
		));
	}
	lines.join("\n")
}

fn format_instruction_breakpoints(breakpoints: &[InstructionBreakpointRecord]) -> String {
	let mut lines = vec!["Instruction breakpoints:".to_string()];
	if breakpoints.is_empty() {
		lines.push("(none)".to_string());
		return lines.join("\n");
	}
	for breakpoint in breakpoints {
		let location = match breakpoint.offset {
			Some(offset) => format!("{}+{offset}", breakpoint.instruction_reference),
			None => breakpoint.instruction_reference.clone(),
		};
		lines.push(format!(
			"- {location}: {}{}{}{}",
			verified_label(breakpoint.verified),
			condition_suffix(breakpoint.condition.as_deref()),
			hit_condition_suffix(breakpoint.hit_condition.as_deref()),
			message_suffix(breakpoint.message.as_deref()),
		));
	}
	lines.join("\n")
}

fn format_data_breakpoint_info(info: &DataBreakpointInfoResponse) -> String {
	let mut lines = vec![format!("Data breakpoint info: {}", info.description)];
	lines.push(format!(
		"Data ID: {}",
		info.data_id.clone().unwrap_or_else(|| "(not available)".to_string())
	));
	if let Some(access_types) = &info.access_types {
		if !access_types.is_empty() {
			lines.push(format!("Access types: {}", access_types.join(", ")));
		}
	}
	if let Some(can_persist) = info.can_persist {
		lines.push(format!("Persistent: {}", if can_persist { "yes" } else { "no" }));
	}
	lines.join("\n")
}

fn format_data_breakpoints(breakpoints: &[DataBreakpointRecord]) -> String {
	let mut lines = vec!["Data breakpoints:".to_string()];
	if breakpoints.is_empty() {
		lines.push("(none)".to_string());
		return lines.join("\n");
	}
	for breakpoint in breakpoints {
		lines.push(format!(
			"- {}: {}{}{}{}{}",
			breakpoint.data_id,
			verified_label(breakpoint.verified),
			breakpoint
				.access_type
				.as_ref()
				.map(|access| format!(" ({access})"))
				.unwrap_or_default(),
			condition_suffix(breakpoint.condition.as_deref()),
			hit_condition_suffix(breakpoint.hit_condition.as_deref()),
			message_suffix(breakpoint.message.as_deref()),
		));
	}
	lines.join("\n")
}

fn format_stack_frames(frames: &[StackFrame]) -> String {
	let mut lines = vec!["Stack trace:".to_string()];
	if frames.is_empty() {
		lines.push("(empty)".to_string());
		return lines.join("\n");
	}
	for frame in frames {
		let location = match frame.source.as_ref().and_then(|source| source.path.as_ref()) {
			Some(path) => format!("{path}:{}:{}", frame.line, frame.column),
			None => format!("<unknown>:{}:{}", frame.line, frame.column),
		};
		lines.push(format!("- #{} {} @ {location}", frame.id, frame.name));
	}
	lines.join("\n")
}

fn format_threads(threads: &[Thread]) -> String {
	let mut lines = vec!["Threads:".to_string()];
	if threads.is_empty() {
		lines.push("(none)".to_string());
		return lines.join("\n");
	}
	for thread in threads {
		lines.push(format!("- {}: {}", thread.id, thread.name));
	}
	lines.join("\n")
}

fn format_scopes(scopes: &[Scope]) -> String {
	let mut lines = vec!["Scopes:".to_string()];
	if scopes.is_empty() {
		lines.push("(none)".to_string());
		return lines.join("\n");
	}
	for scope in scopes {
		lines.push(format!(
			"- {}: ref={}, expensive={}{}",
			scope.name,
			scope.variables_reference,
			if scope.expensive { "yes" } else { "no" },
			scope
				.presentation_hint
				.as_ref()
				.map(|hint| format!(", hint={hint}"))
				.unwrap_or_default(),
		));
	}
	lines.join("\n")
}

fn format_variables(variables: &[Variable]) -> String {
	let mut lines = vec!["Variables:".to_string()];
	if variables.is_empty() {
		lines.push("(none)".to_string());
		return lines.join("\n");
	}
	for variable in variables {
		lines.push(format!(
			"- {} = {}{}{}",
			variable.name,
			variable.value,
			variable
				.r#type
				.as_ref()
				.map(|kind| format!(" ({kind})"))
				.unwrap_or_default(),
			if variable.variables_reference > 0 {
				format!(" [ref={}]", variable.variables_reference)
			} else {
				String::new()
			},
		));
	}
	lines.join("\n")
}

fn format_source_label(source: Option<&Source>, line: Option<i64>, column: Option<i64>) -> Option<String> {
	let source = source?;
	let base = source.path.as_ref().or(source.name.as_ref())?;
	let line = line?;
	Some(match column {
		Some(column) => format!("{base}:{line}:{column}"),
		None => format!("{base}:{line}"),
	})
}

fn format_disassembly(instructions: &[DisassembledInstruction]) -> String {
	let mut lines = vec!["Disassembly:".to_string()];
	if instructions.is_empty() {
		lines.push("(empty)".to_string());
		return lines.join("\n");
	}
	let address_width = instructions.iter().map(|instruction| instruction.address.len()).max().unwrap_or(0);
	let bytes_width = instructions
		.iter()
		.map(|instruction| instruction.instruction_bytes.as_ref().map(String::len).unwrap_or(0))
		.max()
		.unwrap_or(0)
		.max(2);
	for instruction in instructions {
		let location = format_source_label(
			instruction.location.as_ref(),
			instruction.line,
			instruction.column,
		);
		let mut parts = vec![
			format!("{:<address_width$}", instruction.address),
			format!(
				"{:<bytes_width$}",
				instruction.instruction_bytes.as_deref().unwrap_or("")
			),
			instruction.instruction.clone(),
		];
		if let Some(symbol) = &instruction.symbol {
			parts.push(format!("<{symbol}>"));
		}
		if let Some(location) = location {
			parts.push(format!("[{location}]"));
		}
		let text = parts
			.into_iter()
			.filter(|part| !part.is_empty())
			.collect::<Vec<_>>()
			.join("  ");
		lines.push(text.trim_end().to_string());
	}
	lines.join("\n")
}

fn format_memory_read(address: &str, data: Option<&str>, unreadable_bytes: Option<i64>) -> String {
	let mut lines = vec![format!("Memory at {address}:")];
	let buffer = data.map(decode_base64).unwrap_or_default();
	if buffer.is_empty() {
		lines.push("(no readable bytes)".to_string());
	} else {
		for (offset, chunk) in buffer.chunks(16).enumerate() {
			let hex = chunk
				.iter()
				.map(|byte| format!("{byte:02x}"))
				.collect::<Vec<_>>()
				.join(" ");
			let ascii: String = chunk
				.iter()
				.map(|byte| if (32u8..127).contains(byte) { *byte as char } else { '.' })
				.collect();
			let label = if offset == 0 {
				address.to_string()
			} else {
				format!("+0x{:x}", offset * 16)
			};
			lines.push(format!("{label:<18} {hex:<47} |{ascii}|"));
		}
	}
	if let Some(unreadable_bytes) = unreadable_bytes {
		if unreadable_bytes > 0 {
			lines.push(format!("Unreadable bytes: {unreadable_bytes}"));
		}
	}
	lines.join("\n")
}

fn format_table(headers: &[&str], rows: &[Vec<String>]) -> String {
	let mut widths: Vec<usize> = headers.iter().map(|header| header.len()).collect();
	for row in rows {
		for (index, cell) in row.iter().enumerate() {
			if index < widths.len() {
				widths[index] = widths[index].max(cell.len());
			}
		}
	}
	let render = |row: &[String]| {
		row.iter()
			.enumerate()
			.map(|(index, cell)| format!("{:<width$}", cell, width = widths.get(index).copied().unwrap_or(0)))
			.collect::<Vec<_>>()
			.join("  ")
	};
	let mut lines = vec![render(&headers.iter().map(|header| header.to_string()).collect::<Vec<_>>())];
	lines.push(render(&widths.iter().map(|width| "-".repeat(*width)).collect::<Vec<_>>()));
	for row in rows {
		lines.push(render(row));
	}
	lines.join("\n")
}

fn format_modules(modules: &[Module]) -> String {
	if modules.is_empty() {
		return "Modules:\n(none)".to_string();
	}
	let rows: Vec<Vec<String>> = modules
		.iter()
		.map(|module| {
			vec![
				module.id.to_string(),
				module.name.clone(),
				module.path.clone().unwrap_or_default(),
				module.symbol_status.clone().unwrap_or_default(),
				module.address_range.clone().unwrap_or_default(),
			]
		})
		.collect();
	format!("Modules:\n{}", format_table(&["ID", "Name", "Path", "Symbols", "Range"], &rows))
}

fn format_loaded_sources(sources: &[Source]) -> String {
	let mut lines = vec!["Loaded sources:".to_string()];
	if sources.is_empty() {
		lines.push("(none)".to_string());
		return lines.join("\n");
	}
	for source in sources {
		let label = source
			.path
			.clone()
			.or_else(|| source.name.clone())
			.unwrap_or_else(|| "<unknown>".to_string());
		let reference = source
			.source_reference
			.map(|reference| format!(" [ref={reference}]"))
			.unwrap_or_default();
		lines.push(format!("- {label}{reference}"));
	}
	lines.join("\n")
}

fn format_custom_response(command: &str, body: &Value) -> String {
	let serialized = serde_json::to_string_pretty(body).unwrap_or_else(|_| body.to_string());
	format!("{command} response:\n{serialized}")
}

fn format_sessions(sessions: &[SessionSummary]) -> String {
	if sessions.is_empty() {
		return "No debug sessions.".to_string();
	}
	sessions
		.iter()
		.map(|session| {
			let mut lines = vec![
				format!("{}: {}", session.id, session.status.as_str()),
				format!("  adapter={}", session.adapter),
				format!("  cwd={}", session.cwd),
			];
			if let Some(program) = &session.program {
				lines.push(format!("  program={program}"));
			}
			if let Some(location) = format_location(session) {
				lines.push(format!("  location={location}"));
			}
			if let Some(reason) = &session.stop_reason {
				lines.push(format!("  reason={reason}"));
			}
			lines.join("\n")
		})
		.collect::<Vec<_>>()
		.join("\n\n")
}

fn format_evaluation(evaluation: &EvaluateResponse) -> String {
	let mut lines = vec![format!("Result: {}", evaluation.result)];
	if let Some(kind) = &evaluation.r#type {
		lines.push(format!("Type: {kind}"));
	}
	if evaluation.variables_reference > 0 {
		lines.push(format!("Variables ref: {}", evaluation.variables_reference));
	}
	lines.join("\n")
}

fn outcome_text(outcome: &ContinueOutcome, timeout_secs: u64, verb: &str) -> String {
	let mut lines = format_session_snapshot(&outcome.snapshot);
	if outcome.timed_out {
		lines.push(format!(
			"Program is still running after {timeout_secs}s. Use pause to interrupt and inspect state."
		));
		return lines.join("\n");
	}
	match outcome.state {
		ContinueState::Stopped => {
			let location = format_location(&outcome.snapshot).unwrap_or_else(|| "unknown location".to_string());
			lines.push(format!("{verb} stopped at {location}."));
		},
		ContinueState::Terminated => {
			let suffix = outcome
				.snapshot
				.exit_code
				.map(|exit_code| format!(" with exit code {exit_code}"))
				.unwrap_or_default();
			lines.push(format!("Program terminated{suffix}."));
		},
		ContinueState::Running => lines.push("Program is running.".to_string()),
	}
	lines.join("\n")
}

fn verified_label(verified: bool) -> &'static str {
	if verified { "verified" } else { "pending" }
}

fn condition_suffix(condition: Option<&str>) -> String {
	condition.map(|condition| format!(" if {condition}")).unwrap_or_default()
}

fn hit_condition_suffix(hit_condition: Option<&str>) -> String {
	hit_condition.map(|hit| format!(" after {hit}")).unwrap_or_default()
}

fn message_suffix(message: Option<&str>) -> String {
	message.map(|message| format!(" ({message})")).unwrap_or_default()
}

fn decode_base64(input: &str) -> Vec<u8> {
	let mut decoded = Vec::with_capacity(input.len() * 3 / 4);
	let mut accumulator = 0u32;
	let mut bits = 0u32;
	for byte in input.bytes() {
		let Some(value) = base64_value(byte) else {
			continue;
		};
		accumulator = ((accumulator << 6) | u32::from(value)) & 0x00ff_ffff;
		bits += 6;
		if bits >= 8 {
			bits -= 8;
			decoded.push((accumulator >> bits) as u8);
			accumulator &= (1u32 << bits) - 1;
		}
	}
	decoded
}

fn base64_value(byte: u8) -> Option<u8> {
	match byte {
		b'A'..=b'Z' => Some(byte - b'A'),
		b'a'..=b'z' => Some(byte - b'a' + 26),
		b'0'..=b'9' => Some(byte - b'0' + 52),
		b'+' => Some(62),
		b'/' => Some(63),
		_ => None,
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn readonly_classification_matches_upstream() {
		for action in DEBUG_READONLY_ACTIONS {
			assert!(is_readonly_action(action));
		}
		for action in ["launch", "continue", "evaluate", "write_memory", "set_breakpoint"] {
			assert!(!is_readonly_action(action));
		}
	}

	#[test]
	fn timeout_clamps_like_the_typescript_tool() {
		assert_eq!(resolve_timeout(None, None), 30);
		assert_eq!(resolve_timeout(Some(1.0), None), 5);
		assert_eq!(resolve_timeout(Some(1000.0), None), 300);
		assert_eq!(resolve_timeout(Some(60.0), Some(10)), 10);
		assert_eq!(resolve_timeout(Some(60.0), Some(0)), 60);
		assert_eq!(resolve_timeout(Some(f64::NAN), None), 30);
	}

	#[test]
	fn base64_round_trip_decodes_padded_payload() {
		assert_eq!(decode_base64("aGVsbG8="), b"hello");
		assert_eq!(decode_base64("AAECA/8="), vec![0, 1, 2, 3, 255]);
	}

	#[test]
	fn memory_dump_reports_unreadable_bytes() {
		let text = format_memory_read("0x1000", Some("aGVsbG8="), Some(3));
		assert!(text.contains("Memory at 0x1000:"));
		assert!(text.contains("68 65 6c 6c 6f"), "hexdump: {text}");
		assert!(text.contains("|hello|"), "ascii: {text}");
		assert!(text.contains("Unreadable bytes: 3"));
	}

	#[test]
	fn shared_content_length_framer_splits_and_resyncs() {
		let mut framer = MessageFramer::new();
		framer.push(b"Content-Length: 2\r\n\r\n{}Con");
		assert_eq!(framer.drain(|_| {}).unwrap(), vec!["{}".to_string()]);
		framer.push(b"tent-Length: 2\r\n\r\n[]");
		assert_eq!(framer.drain(|_| {}).unwrap(), vec!["[]".to_string()]);

		let mut junk = MessageFramer::new();
		junk.push(b"garbage\r\n\r\nContent-Length: 2\r\n\r\n{}");
		assert_eq!(junk.drain(|_| {}).unwrap(), vec!["{}".to_string()]);
	}

	use crate::dap::client::{DapClient, DapError};
	use crate::dap::config::{ConnectMode, ResolvedAdapter};
	use crate::lsp::framing::MessageFramer;
	use std::collections::HashMap;
	use std::sync::{Arc, Mutex as StdMutex};
	use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

	fn fake_adapter() -> ResolvedAdapter {
		ResolvedAdapter {
			name: "fake".to_string(),
			command: "fake".to_string(),
			args: Vec::new(),
			resolved_command: "fake".to_string(),
			languages: Vec::new(),
			file_types: Vec::new(),
			root_markers: Vec::new(),
			launch_defaults: json!({}),
			attach_defaults: json!({}),
			connect_mode: ConnectMode::Stdio,
			accepts_directory_program: false,
		}
	}

	fn skip_attach_adapter() -> ResolvedAdapter {
		let mut adapter = fake_adapter();
		adapter.attach_defaults = json!({ "skipAttachRequest": true });
		adapter
	}

	fn fake_client() -> (DapClient, tokio::task::JoinHandle<()>, Arc<StdMutex<Vec<String>>>) {
		let (client_read, adapter_write) = tokio::io::duplex(64 * 1024);
		let (adapter_read, client_write) = tokio::io::duplex(64 * 1024);
		let client = DapClient::from_duplex("fake", client_read, client_write);
		let log = Arc::new(StdMutex::new(Vec::new()));
		let task_log = Arc::clone(&log);
		let task = tokio::spawn(async move {
			let mut framer = MessageFramer::new();
			let mut input = adapter_read;
			let mut output = adapter_write;
			let mut buffer = [0u8; 8192];
			let mut seq = 0u64;
			loop {
				let read = match input.read(&mut buffer).await {
					Ok(0) | Err(_) => break,
					Ok(read) => read,
				};
				framer.push(&buffer[..read]);
				let Ok(messages) = framer.drain(|_| {}) else { break };
				for text in messages {
					let Ok(message) = serde_json::from_str::<Value>(&text) else {
						continue;
					};
					if let Some(command) = message.get("command").and_then(Value::as_str) {
						task_log.lock().unwrap().push(command.to_string());
					}
					handle_fake_request(&mut output, &message, &mut seq).await;
				}
			}
		});
		(client, task, log)
	}

	/// Adapter that answers `initialize`, then emits `initialized` after
	/// `initialized_delay`; `configurationDone` replies success or rejection.
	/// Log order records requests plus the emitted event.
	fn initialized_configuration_client(
		initialized_delay: Duration,
		reject_configuration_done: bool,
	) -> (DapClient, tokio::task::JoinHandle<()>, Arc<StdMutex<Vec<String>>>) {
		let (client_read, adapter_write) = tokio::io::duplex(64 * 1024);
		let (adapter_read, client_write) = tokio::io::duplex(64 * 1024);
		let client = DapClient::from_duplex("fake", client_read, client_write);
		let log = Arc::new(StdMutex::new(Vec::new()));
		let task_log = Arc::clone(&log);
		let task = tokio::spawn(async move {
			let mut framer = MessageFramer::new();
			let mut input = adapter_read;
			let mut output = adapter_write;
			let mut buffer = [0u8; 8192];
			let mut seq = 0u64;
			loop {
				let read = match input.read(&mut buffer).await {
					Ok(0) | Err(_) => break,
					Ok(read) => read,
				};
				framer.push(&buffer[..read]);
				let Ok(messages) = framer.drain(|_| {}) else { break };
				for text in messages {
					let Ok(message) = serde_json::from_str::<Value>(&text) else {
						continue;
					};
					let command = message.get("command").and_then(Value::as_str).unwrap_or_default();
					let request_seq = message.get("seq").and_then(Value::as_u64).unwrap_or(0);
					task_log.lock().unwrap().push(command.to_string());
					match command {
						"initialize" => {
							send_response(
								&mut output,
								&mut seq,
								request_seq,
								command,
								json!({ "supportsConfigurationDoneRequest": true }),
							)
							.await;
							if !initialized_delay.is_zero() {
								tokio::time::sleep(initialized_delay).await;
							}
							task_log.lock().unwrap().push("initialized".to_string());
							send_event(&mut output, &mut seq, "initialized", json!({})).await;
						},
						"configurationDone" if reject_configuration_done => {
							*seq += 1;
							send_frame(
								&mut output,
								&json!({
									"seq": *seq,
									"type": "response",
									"request_seq": request_seq,
									"success": false,
									"command": command,
									"message": "configurationDone rejected by fake",
								}),
							)
							.await;
						},
						"configurationDone" => {
							send_response(&mut output, &mut seq, request_seq, command, json!({})).await;
						},
						_ => {},
					}
				}
			}
		});
		(client, task, log)
	}

	/// Adapter that never answers `launch`, sends `initialized` right after
	/// `initialize`, and rejects `configurationDone`: the handshake fails while
	/// the start request is still pending.
	fn stalled_launch_client() -> (DapClient, tokio::task::JoinHandle<()>) {
		let (client_read, adapter_write) = tokio::io::duplex(64 * 1024);
		let (adapter_read, client_write) = tokio::io::duplex(64 * 1024);
		let client = DapClient::from_duplex("fake", client_read, client_write);
		let task = tokio::spawn(async move {
			let mut framer = MessageFramer::new();
			let mut input = adapter_read;
			let mut output = adapter_write;
			let mut buffer = [0u8; 8192];
			let mut seq = 0u64;
			loop {
				let read = match input.read(&mut buffer).await {
					Ok(0) | Err(_) => break,
					Ok(read) => read,
				};
				framer.push(&buffer[..read]);
				let Ok(messages) = framer.drain(|_| {}) else { break };
				for text in messages {
					let Ok(message) = serde_json::from_str::<Value>(&text) else {
						continue;
					};
					let command = message.get("command").and_then(Value::as_str).unwrap_or_default();
					let request_seq = message.get("seq").and_then(Value::as_u64).unwrap_or(0);
					match command {
						"initialize" => {
							send_response(
								&mut output,
								&mut seq,
								request_seq,
								command,
								json!({
									"supportsConfigurationDoneRequest": true,
									"supportsTerminateRequest": true,
								}),
							)
							.await;
							send_event(&mut output, &mut seq, "initialized", json!({})).await;
						},
						"configurationDone" => {
							*seq += 1;
							send_frame(
								&mut output,
								&json!({
									"seq": *seq,
									"type": "response",
									"request_seq": request_seq,
									"success": false,
									"command": command,
									"message": "configurationDone rejected by fake",
								}),
							)
							.await;
						},
						_ => {},
					}
				}
			}
		});
		(client, task)
	}

	async fn send_frame(output: &mut DuplexStream, body: &Value) {
		let payload = body.to_string();
		let frame = format!("Content-Length: {}\r\n\r\n{payload}", payload.len());
		output.write_all(frame.as_bytes()).await.unwrap();
	}

	async fn send_response(
		output: &mut DuplexStream,
		seq: &mut u64,
		request_seq: u64,
		command: &str,
		body: Value,
	) {
		*seq += 1;
		send_frame(
			output,
			&json!({
				"seq": *seq,
				"type": "response",
				"request_seq": request_seq,
				"success": true,
				"command": command,
				"body": body,
			}),
		)
		.await;
	}

	async fn send_event(output: &mut DuplexStream, seq: &mut u64, event: &str, body: Value) {
		*seq += 1;
		send_frame(output, &json!({ "seq": *seq, "type": "event", "event": event, "body": body })).await;
	}

	async fn handle_fake_request(output: &mut DuplexStream, message: &Value, seq: &mut u64) {
		let command = message.get("command").and_then(Value::as_str).unwrap_or_default();
		let request_seq = message.get("seq").and_then(Value::as_u64).unwrap_or(0);
		let arguments = message.get("arguments").cloned().unwrap_or(Value::Null);
		match command {
			"initialize" => {
				send_response(
					output,
					seq,
					request_seq,
					command,
					json!({
						"supportsConfigurationDoneRequest": true,
						"supportsTerminateRequest": true,
						"supportsInstructionBreakpoints": true,
						"supportsDataBreakpoints": true,
						"supportsDisassembleRequest": true,
						"supportsReadMemoryRequest": true,
						"supportsWriteMemoryRequest": true,
						"supportsModulesRequest": true,
						"supportsLoadedSourcesRequest": true,
					}),
				)
				.await;
			},
			"launch" => {
				send_response(output, seq, request_seq, command, json!({})).await;
				send_event(output, seq, "initialized", json!({})).await;
			},
			"configurationDone" => {
				send_response(output, seq, request_seq, command, json!({})).await;
				send_event(
					output,
					seq,
					"output",
					json!({ "category": "stdout", "output": "fake output\n" }),
				)
				.await;
				send_event(output, seq, "stopped", json!({ "reason": "entry", "threadId": 1 })).await;
			},
			"setBreakpoints" => {
				let line = arguments
					.get("breakpoints")
					.and_then(Value::as_array)
					.and_then(|breakpoints| breakpoints.first())
					.and_then(|breakpoint| breakpoint.get("line"))
					.cloned()
					.unwrap_or(json!(1));
				send_response(
					output,
					seq,
					request_seq,
					command,
					json!({ "breakpoints": [{ "id": 1, "verified": true, "line": line }] }),
				)
				.await;
			},
			"threads" => {
				send_response(
					output,
					seq,
					request_seq,
					command,
					json!({ "threads": [{ "id": 1, "name": "main" }] }),
				)
				.await;
			},
			"continue" => {
				send_response(output, seq, request_seq, command, json!({ "allThreadsContinued": true })).await;
				send_event(output, seq, "stopped", json!({ "reason": "breakpoint", "threadId": 1 })).await;
			},
			"stackTrace" => {
				send_response(
					output,
					seq,
					request_seq,
					command,
					json!({
						"stackFrames": [{
							"id": 11,
							"name": "main",
							"source": { "path": "/tmp/main.c", "name": "main.c" },
							"line": 7,
							"column": 1
						}],
						"totalFrames": 1
					}),
				)
				.await;
			},
			_ => {
				send_response(output, seq, request_seq, command, json!({})).await;
			},
		}
	}

	async fn start_fake_session(manager: &DapSessionManager, cwd: &Path) -> tokio::task::JoinHandle<()> {
		let (client, task, log) = fake_client();
		let cancel = CancellationToken::new();
		let deadline = Instant::now() + Duration::from_secs(5);
		let snapshot = manager
			.launch_with_client_for_test(
				client,
				fake_adapter(),
				cwd.to_path_buf(),
				"/tmp/fake-app".to_string(),
				HashMap::new(),
				&cancel,
				deadline,
			)
			.await
			.expect("fake launch completes");
		assert_eq!(snapshot.status, crate::dap::types::SessionStatus::Stopped);
		assert_eq!(snapshot.frame_name.as_deref(), Some("main"));
		assert_eq!(snapshot.line, Some(7));
		let seen = log.lock().unwrap().clone();
		let launch_at = seen.iter().position(|command| command == "launch").expect("launch is sent");
		let configured_at = seen
			.iter()
			.position(|command| command == "configurationDone")
			.expect("configurationDone follows initialized");
		assert!(launch_at < configured_at, "handshake ran before launch: {seen:?}");
		task
	}

	#[tokio::test]
	async fn fake_adapter_launch_breakpoint_continue_output_terminate() {
		let manager = DapSessionManager::new();
		let cwd = std::env::temp_dir();
		let adapter_task = start_fake_session(&manager, &cwd).await;
		let tool = DapTool::shared(Arc::new(manager.clone()));
		let ctx = AgentContext::new(cwd.clone());

		let result = tool
			.execute(&json!({ "action": "set_breakpoint", "file": "main.c", "line": 7 }), &ctx)
			.await
			.expect("set_breakpoint");
		assert_eq!(result.details.as_ref().unwrap()["breakpoints"][0]["verified"], true);
		assert!(result.content.contains("Breakpoints for"), "{}", result.content);

		let result = tool
			.execute(&json!({ "action": "continue" }), &ctx)
			.await
			.expect("continue");
		assert_eq!(result.details.as_ref().unwrap()["state"], "stopped");
		assert_eq!(result.details.as_ref().unwrap()["timedOut"], false);
		assert!(
			result.content.contains("Continue stopped at /tmp/main.c:7:1."),
			"{}",
			result.content
		);

		let result = tool
			.execute(&json!({ "action": "output" }), &ctx)
			.await
			.expect("output");
		assert!(result.content.contains("fake output"), "{}", result.content);

		let result = tool
			.execute(&json!({ "action": "terminate" }), &ctx)
			.await
			.expect("terminate");
		assert!(result.content.contains("Debug session terminated."), "{}", result.content);
		assert!(manager.list_sessions().is_empty());
		adapter_task.abort();
	}

	#[tokio::test]
	async fn cancelled_operation_aborts_without_touching_the_session() {
		let manager = DapSessionManager::new();
		let cwd = std::env::temp_dir();
		let adapter_task = start_fake_session(&manager, &cwd).await;
		let tool = DapTool::shared(Arc::new(manager.clone()));
		let token = CancellationToken::new();
		token.cancel();
		let ctx = AgentContext::with_cancellation(cwd.clone(), token);

		let error = tool
			.execute(&json!({ "action": "threads" }), &ctx)
			.await
			.expect_err("cancelled request aborts");
		assert!(matches!(&error, ToolError::Aborted(_)), "{error}");
		assert_eq!(manager.list_sessions().len(), 1);

		let ctx = AgentContext::new(cwd);
		tool.execute(&json!({ "action": "terminate" }), &ctx).await.expect("terminate");
		adapter_task.abort();
	}

	async fn initialized_session_without_events(
		manager: &DapSessionManager,
		cwd: &Path,
	) -> Arc<crate::dap::session::DapSession> {
		let (client, _task, _log) = fake_client();
		let session = manager
			.register_for_test(client, fake_adapter(), cwd.to_path_buf(), None, HashMap::new())
			.await;
		let cancel = CancellationToken::new();
		manager
			.initialize_for_test(&session, &cancel, Instant::now() + Duration::from_secs(2))
			.await
			.expect("initialize");
		session
	}

	#[tokio::test]
	async fn skip_attach_silent_adapter_succeeds_without_configuration_done() {
		let manager = DapSessionManager::new();
		let cwd = std::env::temp_dir();
		let (client, task, log) = fake_client();
		let cancel = CancellationToken::new();
		let deadline = Instant::now() + Duration::from_millis(150);

		manager
			.attach_with_client_for_test(
				client,
				skip_attach_adapter(),
				cwd,
				HashMap::new(),
				&cancel,
				deadline,
			)
			.await
			.expect("skipAttachRequest succeeds without initialized");
		task.abort();

		let commands = log.lock().unwrap().clone();
		assert!(
			!commands.iter().any(|command| command == "configurationDone"),
			"configurationDone must not be sent without initialized: {commands:?}"
		);
		assert!(
			!commands.iter().any(|command| command == "attach"),
			"skipAttachRequest must not send attach: {commands:?}"
		);
	}

	#[tokio::test]
	async fn skip_attach_delayed_initialized_sends_configuration_done_after_event() {
		let manager = DapSessionManager::new();
		let cwd = std::env::temp_dir();
		let (client, task, log) =
			initialized_configuration_client(Duration::from_millis(100), false);
		let cancel = CancellationToken::new();
		let deadline = Instant::now() + Duration::from_secs(2);

		manager
			.attach_with_client_for_test(
				client,
				skip_attach_adapter(),
				cwd,
				HashMap::new(),
				&cancel,
				deadline,
			)
			.await
			.expect("skipAttachRequest completes after initialized");
		task.abort();

		let commands = log.lock().unwrap().clone();
		let initialized = commands.iter().position(|command| command == "initialized");
		let configuration_done = commands.iter().position(|command| command == "configurationDone");
		assert!(
			matches!((initialized, configuration_done), (Some(event), Some(done)) if event < done),
			"configurationDone must follow initialized: {commands:?}"
		);
	}

	#[tokio::test]
	async fn skip_attach_configuration_done_error_after_initialized_propagates() {
		let manager = DapSessionManager::new();
		let cwd = std::env::temp_dir();
		let (client, task, _log) = initialized_configuration_client(Duration::ZERO, true);
		let cancel = CancellationToken::new();
		let deadline = Instant::now() + Duration::from_secs(2);

		let error = manager
			.attach_with_client_for_test(
				client,
				skip_attach_adapter(),
				cwd,
				HashMap::new(),
				&cancel,
				deadline,
			)
			.await
			.expect_err("configurationDone rejection must fail the attach");
		task.abort();
		assert!(error.to_string().contains("configurationDone"), "{error}");
	}

	#[tokio::test]
	async fn missing_initialized_without_skip_attach_stays_strict() {
		let manager = DapSessionManager::new();
		let cwd = std::env::temp_dir();
		let session = initialized_session_without_events(&manager, &cwd).await;
		let cancel = CancellationToken::new();
		let deadline = Instant::now() + Duration::from_millis(150);

		let error = manager
			.handshake_for_test(&session, false, &cancel, deadline)
			.await
			.expect_err("strict path must reject missing initialized");
		assert!(error.to_string().contains("event initialized timed out"), "{error}");
	}

	#[tokio::test]
	async fn skip_attach_without_initialized_still_honors_cancellation() {
		let manager = DapSessionManager::new();
		let cwd = std::env::temp_dir();
		let session = initialized_session_without_events(&manager, &cwd).await;
		let cancel = CancellationToken::new();
		cancel.cancel();

		let error = manager
			.handshake_for_test(&session, true, &cancel, Instant::now() + Duration::from_secs(5))
			.await
			.expect_err("cancellation is never swallowed");
		assert!(matches!(&error, DapError::Aborted), "{error}");
	}

	#[tokio::test]
	async fn handshake_failure_returns_without_waiting_for_start_deadline() {
		let manager = DapSessionManager::new();
		let cwd = std::env::temp_dir();
		let (client, task) = stalled_launch_client();
		let cancel = CancellationToken::new();
		let deadline = Instant::now() + Duration::from_secs(5);
		let started = Instant::now();
		let error = manager
			.launch_with_client_for_test(
				client,
				fake_adapter(),
				cwd,
				"/tmp/fake-app".to_string(),
				HashMap::new(),
				&cancel,
				deadline,
			)
			.await
			.expect_err("configurationDone rejection fails the handshake");
		let elapsed = started.elapsed();
		task.abort();
		assert!(
			error.to_string().contains("configurationDone"),
			"unexpected error: {error}"
		);
		assert!(
			elapsed < Duration::from_secs(2),
			"handshake failure waited for the start deadline: {elapsed:?}"
		);
	}

	#[tokio::test]
	async fn disposing_a_failed_child_keeps_the_root_session_alive() {
		let manager = DapSessionManager::new();
		let cwd = std::env::temp_dir();
		let adapter_task = start_fake_session(&manager, &cwd).await;
		let root = manager.active_session_for_test().expect("root session active");
		let (child_client, child_task, _child_log) = fake_client();
		let child = manager.register_child_for_test(
			&root,
			child_client,
			fake_adapter(),
			cwd.clone(),
			Some("/tmp/child-app".to_string()),
			HashMap::new(),
		);
		assert_eq!(manager.list_sessions().len(), 2);

		manager.dispose_for_test(&child).await;

		let sessions = manager.list_sessions();
		assert_eq!(sessions.len(), 1, "child disposal removed the root");
		assert_eq!(sessions[0].id, root.id);
		assert!(root.client.is_alive(), "root client was disposed with the child");
		let active = manager.active_session_for_test().expect("root remains active");
		assert_eq!(active.id, root.id);

		let ctx = AgentContext::new(cwd);
		let tool = DapTool::shared(Arc::new(manager.clone()));
		let result = tool
			.execute(&json!({ "action": "terminate" }), &ctx)
			.await
			.expect("terminate root");
		assert!(result.content.contains("Debug session terminated."));
		adapter_task.abort();
		child_task.abort();
	}
}
