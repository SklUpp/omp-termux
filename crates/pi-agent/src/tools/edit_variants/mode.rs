//! `edit.mode` selection and file display mode resolution.
//!
//! Ports `packages/coding-agent/src/utils/edit-mode.ts` and
//! `file-display-mode.ts`. Precedence is model-specific variant (first
//! case-insensitive substring match in insertion order), `PI_EDIT_VARIANT`,
//! `edit.mode`, then the `hashline` default. A model on the known-bad list
//! falls back to `sloppy` unless `PI_STRICT_EDIT_MODE` pins the configured
//! mode.

/// Wire contract selected for the `edit` tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum EditMode {
	Replace,
	Patch,
	Hashline,
	ApplyPatch,
	Sloppy,
}

impl EditMode {
	pub fn as_str(self) -> &'static str {
		match self {
			Self::Replace => "replace",
			Self::Patch => "patch",
			Self::Hashline => "hashline",
			Self::ApplyPatch => "apply_patch",
			Self::Sloppy => "sloppy",
		}
	}

	/// Modes whose wire payload is a single freeform `input` string.
	pub fn uses_input_string(self) -> bool {
		matches!(self, Self::Hashline | Self::ApplyPatch | Self::Sloppy)
	}
}

/// Every supported mode, in the upstream `EDIT_MODE_IDS` order.
pub const EDIT_MODES: [EditMode; 5] = [
	EditMode::ApplyPatch,
	EditMode::Hashline,
	EditMode::Patch,
	EditMode::Replace,
	EditMode::Sloppy,
];

pub const DEFAULT_EDIT_MODE: EditMode = EditMode::Hashline;

/// Normalize a raw settings/env string; unknown values are `None`.
pub fn normalize_edit_mode(mode: &str) -> Option<EditMode> {
	EDIT_MODES.iter().copied().find(|candidate| candidate.as_str() == mode)
}

/// Settings surface the resolver needs. Implemented by the CLI config layer;
/// defaults keep callers that only know `edit.mode` compiling.
pub trait EditModeSettings {
	/// Raw `edit.mode` value, unnormalized.
	fn get_edit_mode(&self) -> Option<&str>;

	/// Model-specific variant. Implementations MUST scan `edit.modelVariants`
	/// in insertion order and compare lowercased substrings.
	fn get_edit_variant_for_model(&self, _model: Option<&str>) -> Option<EditMode> {
		None
	}

	/// `readLineNumbers` setting; default `false`.
	fn get_read_line_numbers(&self) -> bool {
		false
	}
}

/// First case-insensitive substring match over `edit.modelVariants` in
/// insertion order. Callers MUST pass entries in the order the config layer
/// parsed them (a sorted map would change which pattern wins); this helper
/// never sorts.
pub fn match_model_variant<'a, I>(entries: I, active_model: Option<&str>) -> Option<EditMode>
where
	I: IntoIterator<Item = (&'a str, EditMode)>,
{
	let model = active_model?.to_lowercase();
	entries
		.into_iter()
		.find(|(pattern, _)| model.contains(pattern.to_lowercase().as_str()))
		.map(|(_, mode)| mode)
}

/// Resolve the active mode for one session/model.
///
/// `env_variant` is `$PI_EDIT_VARIANT`; `strict` is `$PI_STRICT_EDIT_MODE`.
pub fn resolve_edit_mode(
	settings: &dyn EditModeSettings,
	active_model: Option<&str>,
	env_variant: Option<&str>,
	strict: bool,
) -> EditMode {
	if let Some(variant) = settings.get_edit_variant_for_model(active_model) {
		return variant;
	}
	if let Some(env_mode) = env_variant.and_then(normalize_edit_mode) {
		return env_mode;
	}
	let mode = settings
		.get_edit_mode()
		.and_then(normalize_edit_mode)
		.unwrap_or(DEFAULT_EDIT_MODE);
	if mode == EditMode::Hashline
		&& !strict
		&& active_model.is_some_and(|model| !supports_hashline_edits(model))
	{
		return EditMode::Sloppy;
	}
	mode
}

/// Models that reliably follow the line-anchored hashline dialect.
///
/// Kimi, MiMo, DeepSeek V4 Flash, and Step 3.7 Flash miscount anchors or drop
/// the tag header often enough that hosts fall back to the sloppy format.
pub fn supports_hashline_edits(model: &str) -> bool {
	let lower = model.to_lowercase();
	let kimi = lower.contains("moonshotai/kimi") || contains_kimi_token(&lower);
	let mimo = lower.contains("mimo");
	let deepseek_v4_flash = lower.contains("deepseek-v4-flash");
	let step37_flash = lower.contains("step-3.7-flash");
	!(kimi || mimo || deepseek_v4_flash || step37_flash)
}

/// `/(^|\/)kimi[-.]/i` over a lowercased id.
fn contains_kimi_token(lower: &str) -> bool {
	let bytes = lower.as_bytes();
	for (index, _) in lower.match_indices("kimi") {
		let at_boundary = index == 0 || bytes[index - 1] == b'/';
		if !at_boundary {
			continue;
		}
		let next = bytes.get(index + 4).copied();
		if matches!(next, Some(b'-') | Some(b'.')) {
			return true;
		}
	}
	false
}

/// Effective line display for file-like tool output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileDisplayMode {
	pub line_numbers: bool,
	pub hash_lines: bool,
}

impl FileDisplayMode {
	pub fn plain() -> Self {
		Self { line_numbers: false, hash_lines: false }
	}
}

/// Hashline display takes precedence and implies line-addressed output.
///
/// Hashlines are suppressed without the edit tool (e.g. scout agents), for
/// `raw` reads, and for immutable internal sources (`artifact://`, `agent://`,
/// `memory://`) that have no edit path to consume anchors.
pub fn resolve_file_display_mode(
	edit_mode: EditMode,
	has_edit_tool: bool,
	read_line_numbers: bool,
	raw: bool,
	immutable: bool,
) -> FileDisplayMode {
	let hash_lines = !raw && !immutable && has_edit_tool && edit_mode == EditMode::Hashline;
	FileDisplayMode {
		hash_lines,
		line_numbers: !raw && (hash_lines || read_line_numbers),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	struct Settings {
		mode: Option<&'static str>,
		variant: Option<EditMode>,
	}

	impl EditModeSettings for Settings {
		fn get_edit_mode(&self) -> Option<&str> {
			self.mode
		}
		fn get_edit_variant_for_model(&self, _model: Option<&str>) -> Option<EditMode> {
			self.variant
		}
	}

	#[test]
	fn normalizes_known_modes_only() {
		assert_eq!(normalize_edit_mode("apply_patch"), Some(EditMode::ApplyPatch));
		assert_eq!(normalize_edit_mode("hashline"), Some(EditMode::Hashline));
		assert_eq!(normalize_edit_mode("auto"), None);
		assert_eq!(normalize_edit_mode(""), None);
	}

	#[test]
	fn precedence_model_then_env_then_setting_then_default() {
		let settings = Settings { mode: Some("patch"), variant: Some(EditMode::Replace) };
		assert_eq!(resolve_edit_mode(&settings, Some("m"), Some("sloppy"), false), EditMode::Replace);
		let settings = Settings { mode: Some("patch"), variant: None };
		assert_eq!(resolve_edit_mode(&settings, Some("m"), Some("sloppy"), false), EditMode::Sloppy);
		assert_eq!(resolve_edit_mode(&settings, Some("m"), None, false), EditMode::Patch);
		let settings = Settings { mode: None, variant: None };
		assert_eq!(resolve_edit_mode(&settings, Some("m"), None, false), EditMode::Hashline);
	}

	#[test]
	fn bad_model_falls_back_to_sloppy_unless_strict() {
		let settings = Settings { mode: Some("hashline"), variant: None };
		assert_eq!(resolve_edit_mode(&settings, Some("moonshotai/kimi-k2"), None, false), EditMode::Sloppy);
		assert_eq!(resolve_edit_mode(&settings, Some("moonshotai/kimi-k2"), None, true), EditMode::Hashline);
		assert_eq!(resolve_edit_mode(&settings, Some("anthropic/claude-sonnet"), None, false), EditMode::Hashline);
	}

	#[test]
	fn hashline_support_exclusion_list() {
		for model in [
			"moonshotai/kimi-k2.6",
			"kimi-k3",
			"xiaomi/mimo-7b",
			"deepseek-v4-flash",
			"kilo/stepfun/step-3.7-flash:free",
		] {
			assert!(!supports_hashline_edits(model), "{model}");
		}
		assert!(supports_hashline_edits("deepseek-v4-pro"));
		assert!(supports_hashline_edits("anthropic/claude-sonnet-4"));
	}

	#[test]
	fn display_mode_suppression() {
		let shown = resolve_file_display_mode(EditMode::Hashline, true, false, false, false);
		assert!(shown.hash_lines && shown.line_numbers);
		let raw = resolve_file_display_mode(EditMode::Hashline, true, true, true, false);
		assert!(!raw.hash_lines && !raw.line_numbers);
		let immutable = resolve_file_display_mode(EditMode::Hashline, true, false, false, true);
		assert!(!immutable.hash_lines && !immutable.line_numbers);
		let no_edit = resolve_file_display_mode(EditMode::Hashline, false, false, false, false);
		assert!(!no_edit.hash_lines && !no_edit.line_numbers);
		let numbers = resolve_file_display_mode(EditMode::Patch, true, true, false, false);
		assert!(!numbers.hash_lines && numbers.line_numbers);
	}
}
