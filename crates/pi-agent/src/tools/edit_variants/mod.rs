//! Non-hashline `edit` modes and the shared mode/display resolver.
//!
//! Owns the wire contracts for `replace`, `patch`, `apply_patch`, and `sloppy`
//! plus [`mode`]'s settings resolution. The native hashline engine stays in
//! `tools::hashline`; [`execute`] deliberately refuses `Hashline` so there is
//! exactly one implementation and one dispatch point (the parent `EditTool`).

pub mod apply_patch;
pub mod fsops;
pub mod mode;
pub mod patch;
pub mod paths;
pub mod replace;
pub mod sloppy;
pub mod text;

pub use mode::{
	DEFAULT_EDIT_MODE, EDIT_MODES, EditMode, EditModeSettings, FileDisplayMode, match_model_variant,
	normalize_edit_mode, resolve_edit_mode, resolve_file_display_mode, supports_hashline_edits,
};

use serde_json::Value;

use crate::{
	state::{AgentContext, ToolResult},
	tools::ToolError,
};

/// Per-mode tool metadata the parent `EditTool` returns from
/// `description()`/`schema()`. `wire_name` overrides `EditMode::as_str` when
/// the registry/approval surface uses a different name (`apply_patch`).
///
/// The schema is a builder, not a materialized [`Value`]: `description()`
/// stays allocation-free and the parent calls [`EditContract::schema`] once
/// when it registers the tool.
#[derive(Clone, Copy)]
pub struct EditContract {
	pub description: &'static str,
	build_schema: fn() -> Value,
	pub wire_name: Option<&'static str>,
}

impl EditContract {
	pub fn schema(&self) -> Value {
		(self.build_schema)()
	}
}

/// `None` for `Hashline`, whose contract belongs to the native engine.
pub fn contract(mode: EditMode) -> Option<EditContract> {
	match mode {
		EditMode::Hashline => None,
		EditMode::Replace => Some(EditContract {
			description: replace::DESCRIPTION,
			build_schema: replace::schema,
			wire_name: None,
		}),
		EditMode::Patch => Some(EditContract {
			description: patch::PATCH_DESCRIPTION,
			build_schema: patch::schema,
			wire_name: None,
		}),
		EditMode::ApplyPatch => Some(EditContract {
			description: patch::APPLY_PATCH_DESCRIPTION,
			build_schema: apply_patch::schema,
			wire_name: Some("apply_patch"),
		}),
		EditMode::Sloppy => Some(EditContract {
			description: sloppy::DESCRIPTION,
			build_schema: sloppy::schema,
			wire_name: None,
		}),
	}
}

/// Wire/registry name for a mode, including the `apply_patch` override.
pub fn wire_name(mode: EditMode) -> &'static str {
	match mode {
		EditMode::ApplyPatch => "apply_patch",
		other => other.as_str(),
	}
}

/// Execute a non-hashline mode. `Hashline` returns an argument error because
/// the parent dispatches it to the native engine.
pub async fn execute(mode: EditMode, args: &Value, ctx: &AgentContext) -> Result<ToolResult, ToolError> {
	match mode {
		EditMode::Hashline => Err(ToolError::InvalidArguments(
			"hashline edits are handled by the native hashline engine".to_string(),
		)),
		EditMode::Replace => replace::execute(args, ctx).await,
		EditMode::Patch => patch::execute_patch(args, ctx).await,
		EditMode::ApplyPatch => apply_patch::execute(args, ctx).await,
		EditMode::Sloppy => sloppy::execute(args, ctx).await,
	}
}

/// Build a successful tool result with structured `details`.
pub(crate) fn result(content: impl Into<String>, details: Value) -> ToolResult {
	ToolResult {
		tool_call_id: String::new(),
		content: content.into(),
		is_error: false,
		terminate: false,
		details: Some(details),
	}
}

pub(crate) fn require_str(args: &Value, key: &str) -> Result<String, ToolError> {
	match args.get(key) {
		Some(Value::String(value)) => Ok(value.clone()),
		Some(_) => Err(ToolError::InvalidArguments(format!("{key} must be a string"))),
		None => Err(ToolError::InvalidArguments(format!("{key} is required"))),
	}
}

pub(crate) fn optional_bool(args: &Value, key: &str) -> Result<Option<bool>, ToolError> {
	match args.get(key) {
		None | Some(Value::Null) => Ok(None),
		Some(Value::Bool(value)) => Ok(Some(*value)),
		Some(_) => Err(ToolError::InvalidArguments(format!("{key} must be a boolean"))),
	}
}

#[cfg(test)]
mod tests;
