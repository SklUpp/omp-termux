//! Native eval backend: one persistent cell per call for py/js/rb/jl.
//!
//! The module is wired by the parent (`ToolRegistry`/runtime); it never mutates
//! global process state and never reads configuration from the environment.
//! Host capabilities required by the preludes are explicit [`EvalHost`] seams.

pub mod bridge;
pub mod env;
pub mod host;
pub mod idle;
pub mod kernel;
pub mod languages;
pub mod output;
pub mod probe;
pub mod runtime;
pub mod session;
pub mod tool;
pub mod types;
pub mod util;

pub use bridge::{Bridge, BridgeEndpoint, EVAL_TIMEOUT_PAUSE_OP, EVAL_TIMEOUT_RESUME_OP};
pub use host::{
	ArtifactRef, BRIDGE_REJECTED_TOOLS, EVAL_AGENT_BRIDGE_NAME, EVAL_BUDGET_BRIDGE_NAME,
	EVAL_COMPLETION_BRIDGE_NAME, EVAL_CONCURRENCY_BRIDGE_NAME, EvalHost, INTENT_FIELD,
};
pub use idle::{CellControl, IdleTimeout};
pub use kernel::{Kernel, KernelSpawn};
pub use output::{
	ARTIFACT_MAX_BYTES, DEFAULT_MAX_BYTES, LIVE_TAIL_BYTES, MAX_DISPLAY_TEXT_BYTES,
	MAX_OUTPUT_LINES, OutputSink, RenderedDisplay, format_display_json_for_text,
	format_display_outputs_for_text, image_dimensions, render_display_bundle,
};
pub use probe::{CandidateProbe, ProbeCandidate, ProbeResult, probe_candidates, run_bounded_probe};
pub use runtime::{
	AutoBackground, DEFAULT_AUTO_BACKGROUND_THRESHOLD_MS, EVAL_DEFAULT_TIMEOUT_SECS,
	EVAL_MAX_TIMEOUT_SECS, EVAL_MIN_TIMEOUT_SECS, EvalAllowance, EvalFlagOverrides, EvalRuntime,
	EvalSettings, Interpreters, KernelMode,
};
pub use session::{KernelKey, SessionRegistry, normalize_cwd};
pub use tool::{EvalSessionInfo, EvalTool};
pub use types::{
	AsyncJob, AsyncState, CellStatus, EvalBackendId, EvalCellResult, EvalDisplayOutput, EvalError,
	EvalImage, EvalLanguage, EvalStatusEvent, EvalToolDetails, ImageKind, KernelError,
	KernelExecuteOptions, KernelExecuteResult, KernelFrame, KernelStatus, OutputMeta,
};
