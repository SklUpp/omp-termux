//! The `ast_edit` tool: preview structural rewrites, then apply them through
//! the session's pending-action queue.
//!
//! Preview is always read-only. It compiles each `ops` rule per language found
//! among the candidate files, records the exact bytes of every file it would
//! touch, and stages the prepared outputs in a [`PendingAction`]. A later
//! `write xd://resolve` re-reads every affected file and compares bytes before
//! the first write; any change is stale, so zero files are written. Writes use
//! the existing [`write_atomic`] helper, which preserves the destination's
//! mode and resolves symlinks.
//!
//! There is no filesystem lock. The initial all-files byte gate catches a
//! change before the write phase, and a second per-file check narrows the gate
//! window for each write; a writer racing inside that window can still be
//! overwritten. A failure after some files were written is reported as a
//! partial apply, never as a successful multi-file transaction.
//!
//! Parsing, pattern compilation, and edit validation come from `pi-ast`; path
//! and candidate discovery are shared with [`super::ast_grep`].

use std::{
	collections::{BTreeMap, BTreeSet, HashMap},
	path::PathBuf,
	sync::{Arc, Mutex},
};

use async_trait::async_trait;
use pi_ast::{
	SupportLang,
	ops::{
		Pattern, apply_edits, compile_pattern, parse_source, resolve_language,
		resolve_strictness, tree_has_parse_error,
	},
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::{
	Tool, ToolError,
	ast_grep::{Candidate, collect_candidates, common_base, plan_targets_single},
	format_path_relative_to_cwd,
	hashline::{SnapshotStore, canonical_key, format_header},
	pending::{PendingAction, PendingStore, REJECT_DEVICE_PATH, RESOLVE_DEVICE_PATH},
	tool_result, write_atomic,
};
use crate::state::{AgentContext, ToolResult};

/// Upstream `PI_MAX_AST_FILES` default.
const DEFAULT_MAX_AST_FILES: usize = 1000;
/// Upstream `capParseErrors` / `PARSE_ERRORS_LIMIT`.
const MAX_PARSE_ERRORS: usize = 20;
/// Characters of one `before` / `after` line kept in the preview.
const DIFF_LINE_CHARS: usize = 120;
/// In-result signal that the diff is staged, not applied.
fn preview_pending_notice() -> String {
	format!(
		"Staged as a proposal — files NOT modified yet. To apply: write a one-sentence reason to \
		 {}. To discard: write to {}.",
		RESOLVE_DEVICE_PATH, REJECT_DEVICE_PATH
	)
}

/// How the preview addresses changed lines in model-facing text. Mirrors the
/// upstream `resolveFileDisplayMode(...).hashLines` branch of
/// `packages/coding-agent/src/tools/ast-edit.ts`.
///
/// The mode is an explicit constructor input: this port has no settings store
/// for `edit.mode`, so the caller that builds the tool picks the mode (the
/// builtin registry selects [`PreviewDisplay::Hashline`], matching upstream's
/// default `hashline` edit mode).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PreviewDisplay {
	/// `-LINE:COLUMN text` / `+LINE:COLUMN text`, no headers.
	#[default]
	Plain,
	/// `[PATH#TAG]` header (single file) or `# PATH#TAG (N replacement)` header
	/// (grouped) plus `-LINE:text` / `+LINE:text`. The tag is recorded from the
	/// pre-edit bytes, so the next `edit` can anchor to it.
	Hashline,
}

/// Builtin structural rewrite tool.
///
/// `snapshots` is the session's hashline store (fresh tags after apply);
/// `pending` is the session's staging queue shared with `write xd://resolve`.
pub struct AstEditTool {
	snapshots: Arc<SnapshotStore>,
	pending:   Arc<PendingStore>,
	display:   PreviewDisplay,
}

impl AstEditTool {
	/// Plain previews (no snapshot headers); the port's historical default.
	pub fn new(snapshots: Arc<SnapshotStore>, pending: Arc<PendingStore>) -> Self {
		Self::with_display(snapshots, pending, PreviewDisplay::Plain)
	}

	/// Preview in the given display mode.
	pub fn with_display(
		snapshots: Arc<SnapshotStore>,
		pending: Arc<PendingStore>,
		display: PreviewDisplay,
	) -> Self {
		Self { snapshots, pending, display }
	}
}

#[async_trait]
impl Tool for AstEditTool {
	fn name(&self) -> &str {
		"ast_edit"
	}

	fn description(&self) -> &str {
		"Perform AST-aware code edits (structural refactoring)"
	}

	fn schema(&self) -> Value {
		json!({
			"type": "object",
			"properties": {
				"ops": {
					"type": "array",
					"minItems": 1,
					"description": "rewrite ops",
					"items": {
						"type": "object",
						"properties": {
							"pat": {
								"type": "string",
								"description": "ast pattern"
							},
							"out": {
								"type": "string",
								"description": "replacement template"
							}
						},
						"required": ["pat", "out"]
					}
				},
				"paths": {
					"type": "array",
					"minItems": 1,
					"description": "files, directories, globs, or local file:// URLs to rewrite",
					"items": { "type": "string" }
				}
			},
			"required": ["ops", "paths"]
		})
	}

	async fn execute(&self, args: &Value, ctx: &AgentContext) -> Result<ToolResult, ToolError> {
		if ctx.cancellation_token.is_cancelled() {
			return Ok(tool_result("AST edit cancelled", true));
		}

		let ops = parse_ops(args)?;
		let paths = parse_paths(args)?;
		let max_files = max_ast_files(&ctx.env);

		let path_count = paths.len();
		let target_args = json!({ "path": paths });
		let (targets, missing, scope_path) = plan_targets_single(&target_args, &ctx.cwd)?;
		let Some(collected) = collect_candidates(&targets, ctx)? else {
			return Ok(tool_result("AST edit cancelled", true));
		};
		// Overlapping entries (`src`, `src/*.ts`) can discover the same file
		// twice; count and write each canonical file once.
		let mut seen_files = BTreeSet::new();
		let candidates: Vec<Candidate> = collected
			.into_iter()
			.filter(|candidate| seen_files.insert(candidate.absolute.clone()))
			.collect();
		let grouped = path_count > 1
			|| targets.len() > 1
			|| targets.iter().any(|target| !target.is_file);
		let search_path = format_path_relative_to_cwd(&common_base(&targets), &ctx.cwd, false);

		let Some(mut computed) =
			compute_edits(&ops, &candidates, max_files, &ctx.cancellation_token)?
		else {
			return Ok(tool_result("AST edit cancelled", true));
		};

		let (parse_errors, parse_errors_total) =
			cap_parse_errors(std::mem::take(&mut computed.parse_errors));
		let file_replacements: Vec<Value> = computed
			.file_changes
			.iter()
			.map(|(path, count)| json!({ "path": path, "count": count }))
			.collect();
		let change_details: Vec<Value> = computed
			.changes
			.iter()
			.map(|change| {
				json!({
					"path": change.path,
					"before": change.before,
					"after": change.after,
					"byteStart": change.byte_start,
					"byteEnd": change.byte_end,
					"deletedLength": change.deleted_length,
					"startLine": change.start_line,
					"startColumn": change.start_column,
					"endLine": change.end_line,
					"endColumn": change.end_column,
					"captures": change.captures,
				})
			})
			.collect();

		let mut details = json!({
			"totalReplacements": computed.total_replacements,
			"filesTouched": computed.files_touched,
			"filesSearched": computed.files_searched,
			"applied": false,
			"limitReached": computed.limit_reached,
			"scopePath": scope_path,
			"searchPath": search_path,
			"cwd": ctx.cwd.to_string_lossy().replace('\\', "/"),
			"files": computed.file_changes.iter().map(|(path, _)| path).collect::<Vec<_>>(),
			"fileReplacements": file_replacements,
			"changes": change_details,
		});
		if !parse_errors.is_empty() {
			details["parseErrors"] = json!(parse_errors);
			details["parseErrorsTotal"] = json!(parse_errors_total);
		}

		if computed.total_replacements == 0 {
			let mut message = "No replacements made".to_string();
			let error_lines = parse_error_lines(&parse_errors, parse_errors_total);
			if !error_lines.is_empty() {
				message.push('\n');
				message.push_str(&error_lines.join("\n"));
			}
			let mut result = tool_result(message, false);
			result.details = Some(details);
			return Ok(result);
		}

		// Hashline previews record the pre-edit bytes so the tag the model sees
		// is the one a follow-up `edit` validates against (upstream records the
		// live file the same way before rendering).
		let tags: BTreeMap<String, String> = if self.display == PreviewDisplay::Hashline {
			computed
				.prepared
				.iter()
				.filter_map(|file| {
					let text = normalize_snapshot_text(&file.original);
					self
						.snapshots
						.record(&canonical_key(&file.absolute), &text, None)
						.map(|tag| (file.display.clone(), tag))
				})
				.collect()
		} else {
			BTreeMap::new()
		};
		let render = render_preview(&computed, grouped, &tags);
		details["displayContent"] = json!(render.display.join("\n"));
		let mut output = render.model;
		if computed.limit_reached {
			output.push(String::new());
			output.push("Limit reached; narrow paths.".to_string());
		}
		let error_lines = parse_error_lines(&parse_errors, parse_errors_total);
		if !error_lines.is_empty() {
			output.push(String::new());
			output.extend(error_lines);
		}
		if !missing.is_empty() {
			output.push(String::new());
			output.push(format!("Skipped missing paths: {}", missing.join(", ")));
		}

		let label = format!(
			"AST Edit: {} replacement{} in {} file{}",
			computed.total_replacements,
			plural(computed.total_replacements),
			computed.files_touched,
			plural(computed.files_touched),
		);
		self.pending.register(Arc::new(AstEditPending {
			label,
			prepared: computed.prepared,
			file_changes: computed.file_changes,
			total_replacements: computed.total_replacements,
			files_touched: computed.files_touched,
			files_searched: computed.files_searched,
			limit_reached: computed.limit_reached,
			parse_errors: parse_errors.clone(),
			parse_errors_total,
			scope_path,
			display: self.display,
			snapshots: Arc::clone(&self.snapshots),
			attempts: Mutex::new(AttemptWrites::default()),
		}));

		let mut lines = vec![preview_pending_notice(), String::new()];
		lines.extend(output);
		let mut result = tool_result(lines.join("\n"), false);
		result.details = Some(details);
		Ok(result)
	}
}

/// One planned rewrite rendered in the preview and stored for apply.
#[derive(Clone)]
struct EditChange {
	path:         String,
	before:       String,
	after:        String,
	byte_start:   usize,
	byte_end:     usize,
	deleted_length: usize,
	start_line:   usize,
	start_column: usize,
	end_line:     usize,
	end_column:   usize,
	captures:     BTreeMap<String, String>,
}

/// Every affected file: exact bytes read at preview time plus the prepared
/// replacement when the rewrite actually changes them.
struct PreparedFile {
	absolute: PathBuf,
	display:  String,
	original: Vec<u8>,
	output:   Option<Vec<u8>>,
}

struct ComputedEdits {
	changes:            Vec<EditChange>,
	/// Display path and replacement count, in deterministic file order.
	file_changes:       Vec<(String, u32)>,
	prepared:           Vec<PreparedFile>,
	total_replacements: usize,
	files_touched:      usize,
	files_searched:     usize,
	limit_reached:      bool,
	parse_errors:       Vec<String>,
}

struct ResolvedCandidate {
	candidate:      Candidate,
	language:       Option<SupportLang>,
	language_error: Option<String>,
}

struct CompiledRule {
	pattern:   String,
	rewrite:   String,
	by_lang:   HashMap<String, Pattern>,
	errors:    HashMap<String, String>,
}

fn parse_ops(args: &Value) -> Result<Vec<(String, String)>, ToolError> {
	let entries = args
		.get("ops")
		.and_then(Value::as_array)
		.ok_or_else(|| ToolError::InvalidArguments("missing 'ops' parameter".to_string()))?;
	if entries.is_empty() {
		return Err(ToolError::InvalidArguments(
			"`ops` must include at least one op entry".to_string(),
		));
	}
	let mut ops = Vec::with_capacity(entries.len());
	let mut seen = BTreeSet::new();
	for (index, entry) in entries.iter().enumerate() {
		let pat = entry
			.get("pat")
			.and_then(Value::as_str)
			.ok_or_else(|| {
				ToolError::InvalidArguments(format!("`ops[{index}].pat` must be a string"))
			})?
			.to_string();
		if pat.is_empty() {
			return Err(ToolError::InvalidArguments(format!(
				"`ops[{index}].pat` must be a non-empty pattern"
			)));
		}
		let out = entry
			.get("out")
			.and_then(Value::as_str)
			.ok_or_else(|| {
				ToolError::InvalidArguments(format!("`ops[{index}].out` must be a string"))
			})?
			.to_string();
		if !seen.insert(pat.clone()) {
			return Err(ToolError::InvalidArguments(format!(
				"Duplicate rewrite pattern: {pat}"
			)));
		}
		ops.push((pat, out));
	}
	// Native `normalize_rewrite_map` sorts rules by pattern string.
	ops.sort_by(|left, right| left.0.cmp(&right.0));
	Ok(ops)
}

fn parse_paths(args: &Value) -> Result<Vec<String>, ToolError> {
	let entries = args
		.get("paths")
		.and_then(Value::as_array)
		.ok_or_else(|| ToolError::InvalidArguments("missing 'paths' parameter".to_string()))?;
	if entries.is_empty() {
		return Err(ToolError::InvalidArguments(
			"`paths` must include at least one entry".to_string(),
		));
	}
	let mut paths = Vec::with_capacity(entries.len());
	for (index, entry) in entries.iter().enumerate() {
		let path = entry.as_str().ok_or_else(|| {
			ToolError::InvalidArguments(format!("`paths[{index}]` must be a string"))
		})?;
		if path.trim().is_empty() {
			return Err(ToolError::InvalidArguments(format!(
				"`paths[{index}]` must be a non-empty path"
			)));
		}
		paths.push(path.to_string());
	}
	Ok(paths)
}

fn max_ast_files(env: &HashMap<String, String>) -> usize {
	env
		.get("PI_MAX_AST_FILES")
		.and_then(|value| value.trim().parse::<usize>().ok())
		.filter(|value| *value > 0)
		.unwrap_or(DEFAULT_MAX_AST_FILES)
}

fn compute_edits(
	ops: &[(String, String)],
	candidates: &[Candidate],
	max_files: usize,
	token: &CancellationToken,
) -> Result<Option<ComputedEdits>, ToolError> {
	let mut resolved: Vec<ResolvedCandidate> = Vec::with_capacity(candidates.len());
	let mut languages: BTreeMap<String, SupportLang> = BTreeMap::new();
	for candidate in candidates {
		if token.is_cancelled() {
			return Ok(None);
		}
		match resolve_language(None, &candidate.absolute) {
			Ok(language) => {
				languages.entry(language.canonical_name().to_string()).or_insert(language);
				resolved.push(ResolvedCandidate {
					candidate: candidate.clone(),
					language: Some(language),
					language_error: None,
				});
			},
			Err(error) => resolved.push(ResolvedCandidate {
				candidate: candidate.clone(),
				language: None,
				language_error: Some(error.to_string()),
			}),
		}
	}
	if resolved.is_empty() {
		return Err(ToolError::ExecutionFailed(
			"ast_edit found no supported source files for the given path/glob".to_string(),
		));
	}

	let mut parse_errors: Vec<String> = Vec::new();
	let mut compiled_rules: Vec<CompiledRule> = Vec::with_capacity(ops.len());
	let strictness = resolve_strictness(None);
	for (pattern, rewrite) in ops {
		if token.is_cancelled() {
			return Ok(None);
		}
		let mut by_lang = HashMap::new();
		let mut errors = HashMap::new();
		for (lang_key, language) in &languages {
			if token.is_cancelled() {
				return Ok(None);
			}
			match compile_pattern(pattern, None, &strictness, *language) {
				Ok(compiled) => {
					by_lang.insert(lang_key.clone(), compiled);
				},
				Err(error) => {
					errors.insert(lang_key.clone(), error.to_string());
				},
			}
		}
		// A pattern that parses in no discovered language is a pattern error.
		// Failing in only some languages is expected in a mixed tree and is
		// reported per skipped file below.
		if by_lang.is_empty() && !languages.is_empty() {
			let mut entries: Vec<_> = errors.iter().collect();
			entries.sort_by_key(|(lang_key, _)| lang_key.as_str());
			for (lang_key, error) in entries {
				parse_errors.push(if languages.len() > 1 {
					format!("{pattern} ({lang_key}): {error}")
				} else {
					format!("{pattern}: {error}")
				});
			}
			continue;
		}
		compiled_rules.push(CompiledRule {
			pattern: pattern.clone(),
			rewrite: rewrite.clone(),
			by_lang,
			errors,
		});
	}

	if compiled_rules.is_empty() {
		return Ok(Some(ComputedEdits {
			changes: Vec::new(),
			file_changes: Vec::new(),
			prepared: Vec::new(),
			total_replacements: 0,
			files_touched: 0,
			files_searched: resolved.len(),
			limit_reached: false,
			parse_errors,
		}));
	}

	let mut changes: Vec<EditChange> = Vec::new();
	let mut file_changes: Vec<(String, u32)> = Vec::new();
	let mut prepared: Vec<PreparedFile> = Vec::new();
	let mut files_touched = 0usize;
	let mut limit_reached = false;

	for resolved in &resolved {
		if token.is_cancelled() {
			return Ok(None);
		}
		let ResolvedCandidate { candidate, language, language_error } = resolved;
		if let Some(error) = language_error {
			parse_errors.push(format!("{}: {error}", candidate.display));
			continue;
		}
		let Some(language) = *language else {
			continue;
		};
		let lang_key = language.canonical_name();

		let mut runnable: Vec<(&str, Pattern)> = Vec::new();
		for rule in &compiled_rules {
			if let Some(error) = rule.errors.get(lang_key) {
				parse_errors.push(format!("{}: {}: {error}", rule.pattern, candidate.display));
				continue;
			}
			if let Some(compiled) = rule.by_lang.get(lang_key) {
				runnable.push((rule.rewrite.as_str(), (*compiled).clone()));
			}
		}
		if runnable.is_empty() {
			continue;
		}

		let original = match std::fs::read(&candidate.absolute) {
			Ok(bytes) => bytes,
			Err(error) => {
				parse_errors.push(format!("{}: {error}", candidate.display));
				continue;
			},
		};
		let source = match String::from_utf8(original.clone()) {
			Ok(source) => source,
			Err(error) => {
				parse_errors.push(format!("{}: {error}", candidate.display));
				continue;
			},
		};

		let ast = parse_source(&source, language);
		if tree_has_parse_error(&ast) {
			parse_errors.push(format!(
				"{}: parse error (syntax tree contains error nodes)",
				candidate.display
			));
			continue;
		}

		let mut file_edits = Vec::new();
		let mut seen_edits = BTreeSet::new();
		let mut file_file_changes = Vec::new();
		for (rewrite, pattern) in &runnable {
			for matched in ast.root().find_all((*pattern).clone()) {
				if token.is_cancelled() {
					return Ok(None);
				}
				let edit = matched.replace_by(*rewrite);
				// Byte-identical edits (same span, same replacement) are one
				// deterministic edit; divergent overlaps still fail in
				// `apply_edits`.
				let edit_key =
					(edit.position, edit.deleted_length, edit.inserted_text.clone());
				if !seen_edits.insert(edit_key) {
					continue;
				}
				let after = match String::from_utf8(edit.inserted_text.clone()) {
					Ok(after) => after,
					Err(error) => {
						return Err(ToolError::ExecutionFailed(format!(
							"{}: replacement text is not valid UTF-8: {error}",
							candidate.display
						)));
					},
				};
				let range = matched.range();
				let start = matched.start_pos();
				let end = matched.end_pos();
				let node = matched.get_node();
				let captures: BTreeMap<String, String> =
					HashMap::<String, String>::from(matched.get_env().clone())
						.into_iter()
						.collect();
				file_file_changes.push(EditChange {
					path: candidate.display.clone(),
					before: matched.text().into_owned(),
					after,
					byte_start: range.start,
					byte_end: range.end,
					deleted_length: edit.deleted_length,
					start_line: start.line() + 1,
					start_column: start.column(node) + 1,
					end_line: end.line() + 1,
					end_column: end.column(node) + 1,
					captures,
				});
				file_edits.push(edit);
			}
		}

		if file_file_changes.is_empty() {
			continue;
		}
		if files_touched >= max_files {
			limit_reached = true;
			break;
		}
		files_touched = files_touched.saturating_add(1);

		let output = apply_edits(&source, &file_edits)
			.map_err(|error| ToolError::ExecutionFailed(error.to_string()))?;
		let output = if output == source { None } else { Some(output.into_bytes()) };
		file_changes.push((
			candidate.display.clone(),
			u32::try_from(file_file_changes.len()).unwrap_or(u32::MAX),
		));
		changes.extend(file_file_changes);
		prepared.push(PreparedFile {
			absolute: candidate.absolute.clone(),
			display: candidate.display.clone(),
			original,
			output,
		});
	}

	Ok(Some(ComputedEdits {
		total_replacements: changes.len(),
		changes,
		file_changes,
		prepared,
		files_touched,
		files_searched: resolved.len(),
		limit_reached,
		parse_errors,
	}))
}

/// Model-facing preview text plus the code-frame display view exposed as
/// `details.displayContent` (upstream's TUI lines).
struct PreviewRender {
	model:   Vec<String>,
	display: Vec<String>,
}

struct FileRender {
	model:   Vec<String>,
	display: Vec<String>,
}

fn render_preview(
	computed: &ComputedEdits,
	grouped: bool,
	tags: &BTreeMap<String, String>,
) -> PreviewRender {
	let mut render = PreviewRender {
		model:   Vec::new(),
		display: Vec::new(),
	};
	if grouped {
		let counts: BTreeMap<&str, u32> = computed
			.file_changes
			.iter()
			.map(|(path, count)| (path.as_str(), *count))
			.collect();
		let tree = build_path_tree(computed.file_changes.iter().map(|(path, _)| path.as_str()));
		let mut writer = GroupedWriter {
			computed,
			tags,
			counts,
			render: &mut render,
			emitted: false,
		};
		walk_path_tree(&tree, 0, &mut writer);
		return render;
	}
	if let Some((path, _)) = computed.file_changes.first() {
		let tag = tags.get(path).map(String::as_str);
		if let Some(tag) = tag {
			render.model.push(format_header(path, tag));
		}
		let file = render_file_changes(computed, path, tag);
		render.model.extend(file.model);
		render.display.extend(file.display);
	}
	render
}

/// `tag` is present only for a hashline preview whose file fits the snapshot
/// cap; a tag-less file falls back to plain `LINE:COLUMN` lines, matching
/// upstream's best-effort per-file `hashContext`. Display frames are mode
/// independent, as upstream builds them beside the model lines.
fn render_file_changes(computed: &ComputedEdits, path: &str, tag: Option<&str>) -> FileRender {
	let changes: Vec<&EditChange> = computed
		.changes
		.iter()
		.filter(|change| change.path.as_str() == path)
		.collect();
	let line_number_width = changes
		.iter()
		.map(|change| change.start_line.to_string().len())
		.max()
		.unwrap_or(0);
	let mut render = FileRender {
		model:   Vec::with_capacity(changes.len() * 2),
		display: Vec::with_capacity(changes.len() * 2),
	};
	for change in changes {
		let before = first_line(&change.before);
		let after = first_line(&change.after);
		if tag.is_some() {
			render.model.push(format!("-{}:{before}", change.start_line));
			render.model.push(format!("+{}:{after}", change.start_line));
		} else {
			render.model.push(format!(
				"-{}:{} {before}",
				change.start_line, change.start_column
			));
			render.model.push(format!(
				"+{}:{} {after}",
				change.start_line, change.start_column
			));
		}
		render.display.push(code_frame_line(
			'-',
			change.start_line,
			&before,
			line_number_width,
		));
		render.display.push(code_frame_line(
			'+',
			change.start_line,
			&after,
			line_number_width,
		));
	}
	render
}

/// `-LINE│text` / `+LINE│text` with the gutter padded to the widest line
/// number (`formatCodeFrameLine` in `render-utils.ts`).
fn code_frame_line(marker: char, line: usize, content: &str, line_number_width: usize) -> String {
	let gutter = format!("{marker}{line}");
	let padded = pad_start(&gutter, line_number_width + 1);
	format!("{padded}│{content}")
}

fn pad_start(text: &str, width: usize) -> String {
	let length = text.chars().count();
	if length >= width {
		text.to_string()
	} else {
		format!("{}{text}", " ".repeat(width - length))
	}
}

/// Port of `buildPathTree` in `packages/utils/src/path-tree.ts`: first-seen
/// child order, file names deduped per node, URL-like entries kept whole.
#[derive(Default)]
struct PathTreeNode {
	files:   Vec<(String, String)>,
	subdirs: Vec<(String, PathTreeNode)>,
}

fn build_path_tree<'a>(paths: impl IntoIterator<Item = &'a str>) -> PathTreeNode {
	let mut root = PathTreeNode::default();
	for raw in paths {
		let normalized = raw.replace('\\', "/");
		if is_url_like_path(&normalized) {
			add_tree_file(&mut root, &normalized, raw);
			continue;
		}
		let trimmed = normalized.trim_end_matches('/');
		if trimmed.is_empty() {
			continue;
		}
		let segments: Vec<&str> = trimmed.split('/').collect();
		let mut node = &mut root;
		for segment in &segments[..segments.len() - 1] {
			let index = match node.subdirs.iter().position(|(name, _)| name == segment) {
				Some(index) => index,
				None => {
					node.subdirs.push(((*segment).to_string(), PathTreeNode::default()));
					node.subdirs.len() - 1
				},
			};
			node = &mut node.subdirs[index].1;
		}
		add_tree_file(node, segments[segments.len() - 1], raw);
	}
	root
}

fn add_tree_file(node: &mut PathTreeNode, name: &str, path: &str) {
	if node.files.iter().any(|(existing, _)| existing == name) {
		return;
	}
	node.files.push((name.to_string(), path.to_string()));
}

fn is_url_like_path(path: &str) -> bool {
	let Some((scheme, _)) = path.split_once("://") else {
		return false;
	};
	let mut chars = scheme.chars();
	matches!(chars.next(), Some(first) if first.is_ascii_alphabetic())
		&& chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '+' | '-' | '.'))
}

struct GroupedWriter<'a> {
	computed: &'a ComputedEdits,
	tags:     &'a BTreeMap<String, String>,
	counts:   BTreeMap<&'a str, u32>,
	render:   &'a mut PreviewRender,
	emitted:  bool,
}

fn push_separator(render: &mut PreviewRender, emitted: &mut bool) {
	if *emitted {
		render.model.push(String::new());
		render.display.push(String::new());
	}
	*emitted = true;
}

fn walk_path_tree(node: &PathTreeNode, depth: usize, writer: &mut GroupedWriter) {
	for (file_name, path) in &node.files {
		let tag = writer.tags.get(path).map(String::as_str);
		let file = render_file_changes(writer.computed, path, tag);
		if file.model.is_empty() {
			continue;
		}
		if depth == 0 {
			push_separator(writer.render, &mut writer.emitted);
		} else {
			writer.emitted = true;
		}
		let count = writer.counts.get(path.as_str()).copied().unwrap_or(0);
		let tag_suffix = tag.map(|tag| format!("#{tag}")).unwrap_or_default();
		let header = format!(
			"{} {file_name}{tag_suffix} ({count} replacement{})",
			"#".repeat(depth + 1),
			plural(count as usize)
		);
		writer.render.model.push(header.clone());
		writer.render.model.extend(file.model);
		writer.render.display.push(header);
		writer.render.display.extend(file.display);
	}
	for (dir_name, subdir) in &node.subdirs {
		let mut dir_node = subdir;
		let mut parts = vec![dir_name.clone()];
		while dir_node.files.is_empty() && dir_node.subdirs.len() == 1 {
			let only = &dir_node.subdirs[0];
			parts.push(only.0.clone());
			dir_node = &only.1;
		}
		push_separator(writer.render, &mut writer.emitted);
		let header = format!("{} {}/", "#".repeat(depth + 1), parts.join("/"));
		writer.render.model.push(header.clone());
		writer.render.display.push(header);
		walk_path_tree(dir_node, depth + 1, writer);
	}
}

fn first_line(text: &str) -> String {
	text.split('\n').next().unwrap_or("").chars().take(DIFF_LINE_CHARS).collect()
}

fn plural(count: usize) -> &'static str {
	if count == 1 { "" } else { "s" }
}

fn cap_parse_errors(errors: Vec<String>) -> (Vec<String>, usize) {
	let mut seen = BTreeSet::new();
	let mut deduped = Vec::new();
	for error in errors {
		if seen.insert(error.clone()) {
			deduped.push(error);
		}
	}
	let total = deduped.len();
	deduped.truncate(MAX_PARSE_ERRORS);
	(deduped, total)
}

fn parse_error_lines(errors: &[String], total: usize) -> Vec<String> {
	if errors.is_empty() {
		return Vec::new();
	}
	let mut lines = Vec::with_capacity(errors.len() + 1);
	if total > errors.len() {
		lines.push(format!("Parse issues ({} / {total}):", errors.len()));
	} else {
		lines.push("Parse issues:".to_string());
	}
	lines.extend(errors.iter().map(|error| format!("- {error}")));
	lines
}

/// Files already rewritten by earlier attempts of this staged action. The
/// byte gate cannot tell its own prior writes from external edits, so a stale
/// retry reports the previous write set instead of claiming zero writes.
#[derive(Clone, Default)]
struct AttemptWrites {
	written:     Vec<String>,
	invalidated: Vec<String>,
}

/// Staged apply action. The preview's exact bytes are the stale gate; the
/// prepared outputs are never recomputed from live content.
struct AstEditPending {
	label:              String,
	prepared:           Vec<PreparedFile>,
	file_changes:       Vec<(String, u32)>,
	total_replacements: usize,
	files_touched:      usize,
	files_searched:     usize,
	limit_reached:      bool,
	parse_errors:       Vec<String>,
	parse_errors_total: usize,
	scope_path:         String,
	display:            PreviewDisplay,
	snapshots:          Arc<SnapshotStore>,
	attempts:           Mutex<AttemptWrites>,
}

#[async_trait]
impl PendingAction for AstEditPending {
	fn label(&self) -> &str {
		&self.label
	}

	fn source_tool(&self) -> &str {
		"ast_edit"
	}

	async fn apply(&self, ctx: &AgentContext, _reason: &str) -> Result<ToolResult, ToolError> {
		if ctx.cancellation_token.is_cancelled() {
			return Err(ToolError::Aborted("AST edit cancelled".to_string()));
		}

		// Gate 1: every affected file must still hold the exact bytes this
		// preview read. No write starts until this pass succeeds.
		let mut gate_failures = Vec::new();
		for file in &self.prepared {
			if ctx.cancellation_token.is_cancelled() {
				return Err(ToolError::Aborted("AST edit cancelled".to_string()));
			}
			match tokio::fs::read(&file.absolute).await {
				Ok(bytes) if bytes == file.original => {},
				Ok(_) => gate_failures.push(format!("{}: content changed since preview", file.display)),
				Err(error) => gate_failures.push(format!("{}: {error}", file.display)),
			}
		}
		if !gate_failures.is_empty() {
			let prior = self
				.attempts
				.lock()
				.unwrap_or_else(|poison| poison.into_inner())
				.clone();
			let mut message = format!(
				"Preview is stale / no longer matches; this attempt applied nothing. Preview \
				 expected {} replacement{} in {} file{}.\n{}",
				self.total_replacements,
				plural(self.total_replacements),
				self.files_touched,
				plural(self.files_touched),
				gate_failures.join("\n"),
			);
			if !prior.written.is_empty() {
				message.push_str("\nPreviously written by this staged action: ");
				message.push_str(&prior.written.join(", "));
			}
			if !prior.invalidated.is_empty() {
				message.push_str("\nPrevious snapshot invalidations: ");
				message.push_str(&prior.invalidated.join(", "));
			}
			let mut result = tool_result(message, true);
			let mut details = self.details(false);
			details["stale"] = json!(true);
			details["partial"] = json!(!prior.written.is_empty());
			details["gateFailures"] = json!(gate_failures);
			details["writtenFiles"] = json!(prior.written);
			details["snapshotInvalidations"] = json!(prior.invalidated);
			details["attemptWrittenFiles"] = json!([]);
			result.details = Some(details);
			return Ok(result);
		}

		let mut written: Vec<String> = Vec::new();
		let mut invalidated: Vec<String> = Vec::new();
		let mut fresh_tags: Vec<String> = Vec::new();
		let prepared_writes = self.prepared.iter().filter(|file| file.output.is_some()).count();

		for file in &self.prepared {
			let Some(output) = &file.output else {
				continue;
			};
			if ctx.cancellation_token.is_cancelled() {
				return Ok(self.partial_result(
					format!(
						"Cancelled after writing {} of {prepared_writes} files; remaining files were not modified.",
						written.len()
					),
					&written,
					&invalidated,
					&fresh_tags,
					!written.is_empty(),
					false,
					None,
				));
			}
			// Gate 2: narrow the window between gate 1 and this write. This is
			// a recheck, not a lock; a writer racing after it can still lose.
			match tokio::fs::read(&file.absolute).await {
				Ok(bytes) if bytes == file.original => {},
				Ok(_) => {
					return Ok(self.partial_result(
						format!(
							"Preview is stale / no longer matches at {} after {} of {prepared_writes} files were written. Remaining files were not modified.",
							file.display,
							written.len()
						),
						&written,
						&invalidated,
						&fresh_tags,
						!written.is_empty(),
						true,
						Some(file.display.clone()),
					));
				},
				Err(error) => {
					return Ok(self.partial_result(
						format!(
							"Cannot re-read {} before writing: {error}. {} of {prepared_writes} files were written.",
							file.display,
							written.len()
						),
						&written,
						&invalidated,
						&fresh_tags,
						!written.is_empty(),
						false,
						Some(file.display.clone()),
					));
				},
			}
			if ctx.cancellation_token.is_cancelled() {
				return Ok(self.partial_result(
					format!(
						"Cancelled after writing {} of {prepared_writes} files; remaining files were not modified.",
						written.len()
					),
					&written,
					&invalidated,
					&fresh_tags,
					!written.is_empty(),
					false,
					None,
				));
			}

			match write_atomic(&file.absolute, output, &ctx.cancellation_token).await {
				Ok(()) => {
					written.push(file.display.clone());
					let key = canonical_key(&file.absolute);
					self.snapshots.invalidate(&key);
					invalidated.push(file.display.clone());
					{
						let mut attempts = self
							.attempts
							.lock()
							.unwrap_or_else(|poison| poison.into_inner());
						attempts.written.push(file.display.clone());
						attempts.invalidated.push(file.display.clone());
					}
					// Re-record either way (a later `edit` validates against the
					// post-apply bytes), but only hashline output shows the headers,
					// matching upstream's `useHashLines` gate.
					let text = normalize_snapshot_text(output);
					let tag = self.snapshots.record(&key, &text, None);
					if self.display == PreviewDisplay::Hashline {
						fresh_tags.extend(tag.map(|tag| format_header(&file.display, &tag)));
					}
				},
				Err(error)
					if error.kind() == std::io::ErrorKind::Interrupted
						|| ctx.cancellation_token.is_cancelled() =>
				{
					return Ok(self.partial_result(
						format!(
							"Cancelled after writing {} of {prepared_writes} files; remaining files were not modified.",
							written.len()
						),
						&written,
						&invalidated,
						&fresh_tags,
						!written.is_empty(),
						false,
						Some(file.display.clone()),
					));
				},
				Err(error) => {
					return Ok(self.partial_result(
						format!(
							"Failed to write {}: {error}. {} of {prepared_writes} files were written; remaining files were not modified.",
							file.display,
							written.len()
						),
						&written,
						&invalidated,
						&fresh_tags,
						!written.is_empty(),
						false,
						Some(file.display.clone()),
					));
				},
			}
		}

		let mut text = format!(
			"Applied {} replacement{} in {} file{}.",
			self.total_replacements,
			plural(self.total_replacements),
			self.files_touched,
			plural(self.files_touched),
		);
		if !fresh_tags.is_empty() {
			text.push('\n');
			text.push_str(&fresh_tags.join("\n"));
		}
		let mut details = self.details(true);
		details["writtenFiles"] = json!(written);
		details["snapshotInvalidations"] = json!(invalidated);
		details["freshTagLines"] = json!(fresh_tags);
		let mut result = tool_result(text, false);
		result.details = Some(details);
		Ok(result)
	}
}

impl AstEditPending {
	fn details(&self, applied: bool) -> Value {
		let mut details = json!({
			"totalReplacements": self.total_replacements,
			"filesTouched": self.files_touched,
			"filesSearched": self.files_searched,
			"applied": applied,
			"limitReached": self.limit_reached,
			"scopePath": self.scope_path,
			"files": self.file_changes.iter().map(|(path, _)| path).collect::<Vec<_>>(),
			"fileReplacements": self
				.file_changes
				.iter()
				.map(|(path, count)| json!({ "path": path, "count": count }))
				.collect::<Vec<_>>(),
		});
		if !self.parse_errors.is_empty() {
			details["parseErrors"] = json!(self.parse_errors);
			details["parseErrorsTotal"] = json!(self.parse_errors_total);
		}
		details
	}

	#[allow(clippy::too_many_arguments)]
	fn partial_result(
		&self,
		message: String,
		written: &[String],
		invalidated: &[String],
		fresh_tags: &[String],
		partial: bool,
		stale: bool,
		failed_file: Option<String>,
	) -> ToolResult {
		let mut text = message;
		if !written.is_empty() {
			text.push_str("\nWritten: ");
			text.push_str(&written.join(", "));
		}
		if !invalidated.is_empty() {
			text.push_str("\nSnapshot invalidations: ");
			text.push_str(&invalidated.join(", "));
		}
		if !fresh_tags.is_empty() {
			text.push('\n');
			text.push_str(&fresh_tags.join("\n"));
		}
		let mut details = self.details(false);
		details["partial"] = json!(partial);
		details["stale"] = json!(stale);
		details["writtenFiles"] = json!(written);
		details["snapshotInvalidations"] = json!(invalidated);
		details["freshTagLines"] = json!(fresh_tags);
		if let Some(failed_file) = failed_file {
			details["failedFile"] = json!(failed_file);
		}
		let mut result = tool_result(text, true);
		result.details = Some(details);
		result
	}
}

fn normalize_snapshot_text(bytes: &[u8]) -> String {
	let text = String::from_utf8_lossy(bytes);
	let borrowed: &str = &text;
	let stripped = borrowed.strip_prefix('\u{feff}').unwrap_or(borrowed);
	stripped.replace("\r\n", "\n").replace('\r', "\n")
}

#[cfg(test)]
mod tests {
	use std::{
		fs,
		path::PathBuf,
		time::{SystemTime, UNIX_EPOCH},
	};

	use serde_json::json;

	use super::*;
	use crate::tools::Tool as _;

	struct TempDir(PathBuf);

	impl TempDir {
		fn new(label: &str) -> Self {
			let nanos = SystemTime::now()
				.duration_since(UNIX_EPOCH)
				.map(|duration| duration.as_nanos())
				.unwrap_or_default();
			let path = std::env::temp_dir().join(format!(
				"omp-ast-edit-{label}-{}-{nanos}",
				std::process::id()
			));
			fs::create_dir_all(&path).expect("create temp dir");
			Self(path)
		}
	}

	impl Drop for TempDir {
		fn drop(&mut self) {
			let _ = fs::remove_dir_all(&self.0);
		}
	}

	fn tool() -> (AstEditTool, Arc<PendingStore>) {
		let pending = Arc::new(PendingStore::new());
		let snapshots = Arc::new(SnapshotStore::new());
		(AstEditTool::new(snapshots, Arc::clone(&pending)), pending)
	}

	fn args(paths: &[&str]) -> Value {
		json!({
			"ops": [{ "pat": "foo($A)", "out": "bar($A)" }],
			"paths": paths,
		})
	}

	#[tokio::test]
	async fn preview_does_not_mutate_and_registers_pending() {
		let dir = TempDir::new("preview");
		let file = dir.0.join("a.ts");
		fs::write(&file, "const a = foo(1);\n").expect("write");
		let (tool, pending) = tool();
		let ctx = AgentContext::new(dir.0.clone());

		let result = tool.execute(&args(&["a.ts"]), &ctx).await.expect("execute");
		assert!(!result.is_error, "{}", result.content);
		assert!(result.content.contains("Staged as a proposal"), "{}", result.content);
		assert!(result.content.contains("-1:"), "{}", result.content);
		let details = result.details.as_ref().expect("details");
		assert_eq!(details["totalReplacements"].as_u64(), Some(1));
		assert_eq!(details["filesTouched"].as_u64(), Some(1));
		assert_eq!(details["changes"][0]["captures"]["A"].as_str(), Some("1"));
		assert_eq!(
			fs::read_to_string(&file).expect("read"),
			"const a = foo(1);\n",
			"preview must not write"
		);
		assert_eq!(pending.len(), 1);
	}

	#[tokio::test]
	async fn hashline_preview_records_tag_that_edit_accepts() {
		let dir = TempDir::new("hashline-preview");
		let file = dir.0.join("a.ts");
		fs::write(&file, "const a = foo(1);\n").expect("write");
		let ctx = AgentContext::new(dir.0.clone());
		let snapshots = Arc::new(SnapshotStore::new());
		let pending = Arc::new(PendingStore::new());
		let tool = AstEditTool::with_display(
			Arc::clone(&snapshots),
			pending,
			PreviewDisplay::Hashline,
		);

		let result = tool.execute(&args(&["a.ts"]), &ctx).await.expect("execute");
		assert!(!result.is_error, "{}", result.content);
		assert!(!result.content.contains("-1:11 "), "{}", result.content);
		assert!(
			result.content.contains("-1:const a = foo(1);"),
			"{}",
			result.content
		);
		let header = result
			.content
			.lines()
			.find(|line| line.starts_with("[a.ts#"))
			.unwrap_or_else(|| panic!("missing hashline header:\n{}", result.content));
		let tag = header
			.strip_prefix("[a.ts#")
			.and_then(|rest| rest.strip_suffix(']'))
			.expect("header shape");

		let edit = crate::tools::EditTool::new(Arc::clone(&snapshots));
		let applied = edit
			.execute(
				&json!({ "input": format!("[a.ts#{tag}]\nPUT 1.=1:\n+const a = baz(1);\n") }),
				&ctx,
			)
			.await
			.expect("edit executes");
		assert!(!applied.is_error, "{}", applied.content);
		assert_eq!(fs::read_to_string(&file).expect("read"), "const a = baz(1);\n");
	}

	#[tokio::test]
	async fn grouped_preview_folds_directories_and_renders_code_frames() {
		let dir = TempDir::new("grouped-preview");
		fs::create_dir_all(dir.0.join("src/nested")).expect("nested dir");
		fs::write(dir.0.join("src/a.ts"), "const a = foo(1);\n").expect("write a");
		fs::write(dir.0.join("src/nested/b.ts"), "const b = foo(2);\n").expect("write b");
		let tool = AstEditTool::with_display(
			Arc::new(SnapshotStore::new()),
			Arc::new(PendingStore::new()),
			PreviewDisplay::Hashline,
		);
		let ctx = AgentContext::new(dir.0.clone());

		let result = tool.execute(&args(&["src"]), &ctx).await.expect("execute");
		assert!(!result.is_error, "{}", result.content);
		let lines: Vec<&str> = result.content.lines().collect();
		let root = lines
			.iter()
			.position(|line| *line == "# src/")
			.unwrap_or_else(|| panic!("missing folded root header:\n{}", result.content));
		assert!(lines[root + 1].starts_with("## a.ts#"), "{}", result.content);
		assert!(
			lines[root + 1].ends_with("(1 replacement)"),
			"{}",
			result.content
		);
		assert_eq!(lines[root + 2], "-1:const a = foo(1);");
		assert_eq!(lines[root + 3], "+1:const a = bar(1);");
		assert_eq!(lines[root + 4], "");
		assert_eq!(lines[root + 5], "## nested/");
		assert!(lines[root + 6].starts_with("### b.ts#"), "{}", result.content);

		let details = result.details.as_ref().expect("details");
		let display = details["displayContent"].as_str().expect("displayContent");
		assert!(display.contains("# src/"), "{display}");
		assert!(display.contains("## nested/"), "{display}");
		assert!(display.contains("-1│const a = foo(1);"), "{display}");
	}

	#[tokio::test]
	async fn zero_matches_stages_nothing() {
		let dir = TempDir::new("zero");
		let file = dir.0.join("a.ts");
		fs::write(&file, "const a = 1;\n").expect("write");
		let (tool, pending) = tool();
		let ctx = AgentContext::new(dir.0.clone());

		let result = tool.execute(&args(&["a.ts"]), &ctx).await.expect("execute");
		assert!(!result.is_error, "{}", result.content);
		assert!(result.content.contains("No replacements made"), "{}", result.content);
		assert!(pending.is_empty());
		assert_eq!(fs::read_to_string(&file).expect("read"), "const a = 1;\n");
	}

	#[tokio::test]
	async fn resolve_applies_prepared_output() {
		let dir = TempDir::new("resolve");
		let file = dir.0.join("a.ts");
		fs::write(&file, "const a = foo(1);\n").expect("write");
		let (tool, pending) = tool();
		let ctx = AgentContext::new(dir.0.clone());

		tool.execute(&args(&["a.ts"]), &ctx).await.expect("preview");
		let applied = pending.resolve(&ctx, "apply it").await.expect("resolve");
		assert!(!applied.is_error, "{}", applied.content);
		assert!(applied.content.contains("Applied 1 replacement in 1 file."), "{}", applied.content);
		assert_eq!(fs::read_to_string(&file).expect("read"), "const a = bar(1);\n");
		assert!(pending.is_empty());
	}

	#[tokio::test]
	async fn reject_discards_without_writing() {
		let dir = TempDir::new("reject");
		let file = dir.0.join("a.ts");
		fs::write(&file, "const a = foo(1);\n").expect("write");
		let (tool, pending) = tool();
		let ctx = AgentContext::new(dir.0.clone());

		tool.execute(&args(&["a.ts"]), &ctx).await.expect("preview");
		let rejected = pending.reject(&ctx, "not now").await.expect("reject");
		assert!(!rejected.is_error, "{}", rejected.content);
		assert_eq!(fs::read_to_string(&file).expect("read"), "const a = foo(1);\n");
		assert!(pending.is_empty());
	}

	#[tokio::test]
	async fn same_count_changed_content_is_stale_and_writes_nothing() {
		let dir = TempDir::new("stale");
		let file = dir.0.join("a.ts");
		fs::write(&file, "const a = foo(1);\n").expect("write");
		let (tool, pending) = tool();
		let ctx = AgentContext::new(dir.0.clone());

		tool.execute(&args(&["a.ts"]), &ctx).await.expect("preview");
		// Same replacement count, different bytes: a counts-only check would
		// mistakenly apply the stale preview.
		fs::write(&file, "const a = foo(2);\n").expect("user edit");

		let stale = pending.resolve(&ctx, "apply").await.expect("resolve");
		assert!(stale.is_error, "{}", stale.content);
		assert!(stale.content.contains("stale / no longer matches"), "{}", stale.content);
		assert_eq!(fs::read_to_string(&file).expect("read"), "const a = foo(2);\n");
		assert_eq!(pending.len(), 1, "failed apply must requeue the action");

		let retry = pending.resolve(&ctx, "retry").await.expect("retry");
		assert!(retry.is_error, "{}", retry.content);
		assert_eq!(fs::read_to_string(&file).expect("read"), "const a = foo(2);\n");
	}

	#[tokio::test]
	async fn multi_file_stale_gate_writes_zero_files() {
		let dir = TempDir::new("multifile-stale");
		let a = dir.0.join("a.ts");
		let b = dir.0.join("b.ts");
		fs::write(&a, "const a = foo(1);\n").expect("write a");
		fs::write(&b, "const b = foo(1);\n").expect("write b");
		let (tool, pending) = tool();
		let ctx = AgentContext::new(dir.0.clone());

		tool.execute(&args(&["a.ts", "b.ts"]), &ctx).await.expect("preview");
		fs::write(&b, "const b = foo(2);\n").expect("user edit b");

		let stale = pending.resolve(&ctx, "apply").await.expect("resolve");
		assert!(stale.is_error, "{}", stale.content);
		assert_eq!(
			fs::read_to_string(&a).expect("read a"),
			"const a = foo(1);\n",
			"the unchanged file must not be partially written"
		);
		assert_eq!(fs::read_to_string(&b).expect("read b"), "const b = foo(2);\n");
		let details = stale.details.as_ref().expect("details");
		assert_eq!(
			details["writtenFiles"].as_array().map(|files| files.len()),
			Some(0)
		);
	}

	#[tokio::test]
	async fn multiple_paths_keep_single_glob_scope() {
		let dir = TempDir::new("glob-scope");
		let top = dir.0.join("a.ts");
		let nested_dir = dir.0.join("lib");
		let nested = nested_dir.join("x.ts");
		fs::create_dir_all(&nested_dir).expect("nested dir");
		fs::write(&top, "const a = foo(1);\n").expect("write top");
		fs::write(&nested, "const x = foo(1);\n").expect("write nested");
		let (tool, pending) = tool();
		let ctx = AgentContext::new(dir.0.clone());

		// Upstream parses each paths entry with single-path semantics: `*.ts`
		// stays top-level even beside a literal entry. Only `a.ts` is in scope.
		let preview = tool
			.execute(&args(&["a.ts", "*.ts"]), &ctx)
			.await
			.expect("preview");
		assert!(!preview.is_error, "{}", preview.content);
		let details = preview.details.as_ref().expect("details");
		assert_eq!(details["totalReplacements"].as_u64(), Some(1));
		assert_eq!(details["files"].as_array().map(|files| files.len()), Some(1));

		pending.resolve(&ctx, "apply").await.expect("resolve");
		assert_eq!(fs::read_to_string(&top).expect("read top"), "const a = bar(1);\n");
		assert_eq!(
			fs::read_to_string(&nested).expect("read nested"),
			"const x = foo(1);\n",
			"nested file must stay out of a top-level glob"
		);
	}

	#[cfg(unix)]
	#[tokio::test]
	async fn partial_apply_retry_reports_prior_writes_and_writes_nothing() {
		use std::os::unix::fs::PermissionsExt;

		let dir = TempDir::new("partial-retry");
		let first = dir.0.join("a.ts");
		let blocked = dir.0.join("blocked");
		let second = blocked.join("b.ts");
		fs::create_dir_all(&blocked).expect("blocked dir");
		fs::write(&first, "const a = foo(1);\n").expect("write first");
		fs::write(&second, "const b = foo(1);\n").expect("write second");
		let (tool, pending) = tool();
		let ctx = AgentContext::new(dir.0.clone());

		tool.execute(&args(&["a.ts", "blocked/b.ts"]), &ctx)
			.await
			.expect("preview");
		// `a.ts` is written first; the read-only directory makes the second
		// write fail, exercising the partial path.
		fs::set_permissions(&blocked, fs::Permissions::from_mode(0o500)).expect("chmod");
		let probe = blocked.join(".permission-probe");
		if fs::write(&probe, b"probe").is_ok() {
			// Root ignores directory write bits; this environment cannot
			// exercise the partial-write path through permissions.
			fs::remove_file(&probe).ok();
			fs::set_permissions(&blocked, fs::Permissions::from_mode(0o700)).expect("restore chmod");
			return;
		}

		let partial = pending.resolve(&ctx, "apply").await.expect("partial");
		fs::set_permissions(&blocked, fs::Permissions::from_mode(0o700)).expect("restore chmod");
		assert!(partial.is_error, "{}", partial.content);
		assert!(partial.content.contains("Failed to write"), "{}", partial.content);
		let partial_details = partial.details.as_ref().expect("details");
		assert_eq!(partial_details["partial"], json!(true));
		assert_eq!(
			partial_details["writtenFiles"],
			json!(["a.ts"]),
			"partial result must name the written file"
		);
		assert_eq!(partial_details["snapshotInvalidations"], json!(["a.ts"]));
		assert_eq!(fs::read_to_string(&first).expect("read first"), "const a = bar(1);\n");
		assert_eq!(fs::read_to_string(&second).expect("read second"), "const b = foo(1);\n");

		let retry = pending.resolve(&ctx, "retry").await.expect("stale retry");
		assert!(retry.is_error, "{}", retry.content);
		assert!(
			retry.content.contains("this attempt applied nothing"),
			"{}",
			retry.content
		);
		assert!(
			retry.content.contains("Previously written by this staged action: a.ts"),
			"{}",
			retry.content
		);
		let retry_details = retry.details.as_ref().expect("details");
		assert_eq!(retry_details["writtenFiles"], json!(["a.ts"]));
		assert_eq!(retry_details["attemptWrittenFiles"], json!([]));
		assert_eq!(retry_details["partial"], json!(true));
		assert_eq!(fs::read_to_string(&first).expect("read first"), "const a = bar(1);\n");
		assert_eq!(fs::read_to_string(&second).expect("read second"), "const b = foo(1);\n");
	}

	#[tokio::test]
	async fn cancellation_before_apply_writes_nothing() {
		let dir = TempDir::new("cancel");
		let a = dir.0.join("a.ts");
		let b = dir.0.join("b.ts");
		fs::write(&a, "const a = foo(1);\n").expect("write a");
		fs::write(&b, "const b = foo(1);\n").expect("write b");
		let (tool, pending) = tool();
		let ctx = AgentContext::new(dir.0.clone());

		tool.execute(&args(&["a.ts", "b.ts"]), &ctx).await.expect("preview");
		ctx.cancellation_token.cancel();

		let cancelled = pending.resolve(&ctx, "apply").await;
		assert!(matches!(cancelled, Err(ToolError::Aborted(_))));
		assert_eq!(fs::read_to_string(&a).expect("read a"), "const a = foo(1);\n");
		assert_eq!(fs::read_to_string(&b).expect("read b"), "const b = foo(1);\n");
		assert_eq!(pending.len(), 1);
	}

	#[cfg(unix)]
	#[tokio::test]
	async fn apply_preserves_file_mode() {
		use std::os::unix::fs::PermissionsExt;

		let dir = TempDir::new("mode");
		let file = dir.0.join("a.ts");
		fs::write(&file, "const a = foo(1);\n").expect("write");
		fs::set_permissions(&file, fs::Permissions::from_mode(0o640)).expect("chmod");
		let (tool, pending) = tool();
		let ctx = AgentContext::new(dir.0.clone());

		tool.execute(&args(&["a.ts"]), &ctx).await.expect("preview");
		pending.resolve(&ctx, "apply").await.expect("resolve");

		let mode = fs::metadata(&file).expect("metadata").permissions().mode() & 0o777;
		assert_eq!(mode, 0o640);
		assert_eq!(fs::read_to_string(&file).expect("read"), "const a = bar(1);\n");
	}
}
