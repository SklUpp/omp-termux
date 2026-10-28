//! `edit` tool on the native hashline contract.
//!
//! One call carries one or more `[PATH#TAG]` sections. Every section is read,
//! tag-validated, and applied in memory before the first write hits disk, so a
//! batch never writes a partially-validated set. Commits then land one section
//! at a time: a later failure or cancellation reports the sections already
//! written (upstream `patcher.ts` behavior). Each commit re-checks the target's
//! canonical identity and content tag, so bytes replaced or a symlink retargeted
//! after validation are not overwritten. Writes preserve BOM, the dominant line
//! ending, file mode, and symlink targets; moves carry the source permissions.

use std::{
	collections::BTreeMap,
	path::{Path, PathBuf},
	sync::Mutex,
};

use tokio_util::sync::CancellationToken;

use super::{
	apply,
	block,
	budget::Budget,
	clipboard::{self, Clipboard},
	hash,
	mismatch::mismatch_message,
	parser::{self, FileOp, ParsedPatch, PatchSection},
	recovery, snapshot::{self, SnapshotStore},
	syntax,
};
use crate::{
	state::{AgentContext, ToolResult},
	tools::{MAX_EDIT_BYTES, ToolError, read_capped, write_atomic_with_mode},
};

/// Inline cap on unseen anchor lines echoed back by the rejection.
const SEEN_LINE_REVEAL_CAP: usize = 40;
/// Per-revealed-line column cap (matches the read/search column cap).
const SEEN_LINE_REVEAL_MAX_COLUMNS: usize = 512;
const HEADTAIL_DRIFT_WARNING: &str =
	"Applied the `PUT <1:`/`PUT >$:` edit despite a stale snapshot tag (file changed since your read) — \
	 head/tail position is content-independent. Re-read if the drift was unexpected.";
/// Behavioural knobs for the hashline edit tool.
#[derive(Debug, Clone, Copy)]
pub struct EditToolOptions {
	/// Reject edits anchored on source lines the minting read never displayed.
	/// Matches upstream `edit.enforceSeenLines`, whose default is `false`
	/// (opt-in guard).
	pub enforce_seen_lines: bool,
}

impl Default for EditToolOptions {
	fn default() -> Self {
		// Upstream `edit.enforceSeenLines` schema default is `false`; the guard
		// is opt-in.
		Self { enforce_seen_lines: false }
	}
}

/// Per-session guard against repeated byte-identical no-op payloads.
pub type NoopGuard = Mutex<BTreeMap<String, (u64, u32)>>;

/// Consecutive identical no-ops before the soft hint escalates to a hard stop.
const NOOP_HARD_LIMIT: u32 = 3;
const CLIPBOARD_INTERLEAVED_SECTIONS: &str =
	"`CUT`/register-`PUT` ops cannot be used in a file whose sections are interleaved with another file's: \
	 same-path sections merge into the first occurrence, which would reorder the register sequence. \
	 Keep each file's ops under ONE `[path#TAG]` header.";

struct Prepared {
	section: PatchSection,
	canonical: String,
	bom: String,
	line_ending: hash::LineEnding,
	normalized: String,
	after: String,
	first_changed_line: Option<u32>,
	warnings: Vec<String>,
	file_op: Option<FileOp>,
	noop: bool,
	/// Exact validated text (UTF-8 bytes; non-UTF-8 is rejected at read);
	/// commits compare this, never the 16-bit tag.
	source_raw: String,
	/// Unix mode of the validated source (moves publish it on the temp file).
	source_mode: Option<u32>,
	/// `(dev, ino)` of the validated target where the platform exposes it.
	source_id: Option<(u64, u64)>,
	/// Resolved absolute move destination and its canonical identity.
	move_dest: Option<PathBuf>,
	dest_canonical: Option<String>,
}

enum Outcome {
	Updated { path: String, tag: String, first_changed: Option<u32>, warnings: Vec<String> },
	Deleted { path: String, warnings: Vec<String> },
	Moved { from: String, to: String, tag: String, first_changed: Option<u32>, warnings: Vec<String> },
	Noop { path: String, warnings: Vec<String> },
}

/// Tool result on the hashline contract.
pub async fn execute(
	store: &SnapshotStore,
	registers: &Mutex<BTreeMap<String, Vec<String>>>,
	noop_guard: &NoopGuard,
	options: &EditToolOptions,
	args: &serde_json::Value,
	ctx: &AgentContext,
) -> Result<ToolResult, ToolError> {
	let input = args
		.get("input")
		.and_then(serde_json::Value::as_str)
		.ok_or_else(|| ToolError::InvalidArguments("missing 'input' parameter".to_string()))?;
	if input.len() > MAX_EDIT_BYTES {
		return Err(ToolError::InvalidArguments(format!(
			"input is {} bytes, the edit limit is {MAX_EDIT_BYTES}",
			input.len()
		)));
	}

	let sections = match parser::split_patch_sections(input, Some(ctx.cwd.as_path())) {
		Ok(sections) => sections,
		Err(message) => return Ok(tool_result(format!("Failed to edit: {message}"), true)),
	};
	if sections.is_empty() {
		return Ok(tool_result("No hashline sections found in input.".to_string(), true));
	}
	if ctx.cancellation_token.is_cancelled() {
		return Ok(tool_result("Edit cancelled".to_string(), true));
	}

	let mut budget = Budget::default();
	// Reserve and clone the session map in a lexical block so the std guard is
	// released before the first `await`; the clone itself was already charged.
	let session_registers = {
		let guard = registers.lock().unwrap_or_else(|poison| poison.into_inner());
		if let Err(message) = budget.charge_bytes(clipboard::named_bytes(&guard)) {
			return Ok(tool_result(format!("Failed to edit: {message}"), true));
		}
		(*guard).clone()
	};
	let mut batch = clipboard::start_batch(session_registers);
	let mut prepared = Vec::with_capacity(sections.len());
	let mut states = Vec::with_capacity(sections.len());
	for section in sections {
		match prepare(section, &ctx.cwd, &ctx.cancellation_token, &mut batch, store, options, &mut budget).await {
			Ok(entry) => {
				if let Err(message) = budget.charge_bytes(clipboard::batch_bytes(&batch)) {
					return Ok(tool_result(format!("Failed to edit: {message}"), true));
				}
				states.push(batch.clone());
				prepared.push(entry);
			}
			Err(PrepareError::Cancelled) => {
				return Ok(tool_result("Edit cancelled".to_string(), true));
			}
			Err(PrepareError::Failed(message)) => {
				return Ok(tool_result(format!("Failed to edit: {message}"), true));
			}
		}
	}
	if let Err(message) = assert_no_target_collisions(&prepared) {
		return Ok(tool_result(format!("Failed to edit: {message}"), true));
	}

	let input_hash = xxhash_rust::xxh64::xxh64(input.as_bytes(), 0);
	// A byte-identical no-op is a hard failure, not a hint; repeating the same
	// payload escalates (upstream `noop-loop-guard.ts`).
	if let Some(entry) = prepared.iter().find(|entry| entry.noop) {
		let (count, escalate) = record_noop(noop_guard, &entry.canonical, input_hash);
		let message = if escalate {
			no_change_loop_diagnostic(&entry.section.path, count)
		} else {
			no_change_diagnostic(&entry.section.path)
		};
		return Ok(tool_result(message, true));
	}

	let total = prepared.len();
	let mut rendered: Vec<String> = Vec::with_capacity(total);
	let mut landed: Vec<String> = Vec::new();
	for (index, entry) in prepared.into_iter().enumerate() {
		let canonical = entry.canonical.clone();
		match commit(entry, &ctx.cancellation_token, store).await {
			Ok(outcome) => {
				let mut session = registers
					.lock()
					.unwrap_or_else(|poison| poison.into_inner());
				let stored_registers = clipboard::commit_batch(&states[index], &mut session);
				drop(session);
				reset_noop(noop_guard, &canonical);
				let text = render(outcome);
				landed.push(text.lines().next().unwrap_or_default().to_string());
				rendered.push(text);
				if let Err(message) = stored_registers {
					return Ok(tool_result(
						failure_report(
							&format!(
								"The file update landed, but named register state was not stored: {message}."
							),
							&landed,
							total - index - 1,
						),
						true,
					));
				}
			}
			Err(CommitError::Failed(message)) => {
				return Ok(tool_result(
					failure_report(&message, &landed, total - index),
					true,
				));
			}
			Err(CommitError::Cancelled) => {
				return Ok(tool_result(
					failure_report("Edit cancelled", &landed, total - index),
					true,
				));
			}
		}
	}
	Ok(tool_result(rendered.join("\n"), false))
}

#[derive(Debug)]
enum PrepareError {
	Cancelled,
	Failed(String),
}

impl From<String> for PrepareError {
	fn from(message: String) -> Self {
		PrepareError::Failed(message)
	}
}

#[derive(Debug)]
enum CommitError {
	Cancelled,
	Failed(String),
}

fn assert_no_target_collisions(prepared: &[Prepared]) -> Result<(), String> {
	let mut owners: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
	for entry in prepared {
		if let Some(previous) = owners.insert(entry.canonical.clone(), entry.section.path.clone()) {
			return Err(format!(
				"Multiple hashline sections resolve to the same file ({previous} and {}). \
				 Merge their ops under one header before applying.",
				entry.section.path
			));
		}
	}
	// Move destinations are writes too: they must not alias any source or
	// another destination in this batch, or a later section can overwrite the
	// moved content after the move already reported success.
	for entry in prepared {
		let Some(dest) = &entry.dest_canonical else { continue };
		if let Some(owner) = owners.get(dest) {
			return Err(format!(
				"The MV destination in the section for {} resolves to {owner}, which this batch also \
				 touches (directly or as another move target). Split the move into its own edit call so \
				 dependent content cannot overwrite each other.",
				entry.section.path
			));
		}
		if let Some(previous) = owners.insert(dest.clone(), entry.section.path.clone()) {
			return Err(format!(
				"Two sections in this batch write the same destination {dest} ({previous} and {}). \
				 Split the moves into separate edit calls.",
				entry.section.path
			));
		}
	}
	Ok(())
}

fn failure_report(message: &str, landed: &[String], not_written: usize) -> String {
	let mut text = message.to_string();
	if !landed.is_empty() {
		text.push_str("\n\nAlready written by this call:");
		for line in landed {
			text.push_str("\n- ");
			text.push_str(line);
		}
	}
	if not_written > 0 {
		text.push_str(&format!(
			"\n\n{not_written} section(s) starting with the failing one did not complete; any partial \
			 mutation the message above reports still stands. Re-read their current content before retrying."
		));
	}
	text
}

fn missing_tag_message(path: &str) -> String {
	format!(
		"Missing hashline snapshot tag for {path}; use `[{path}#tag]` from your latest read/search output. \
		 To create a new file, use the write tool."
	)
}

fn no_change_diagnostic(path: &str) -> String {
	format!(
		"Edits to {path} parsed and applied cleanly, but produced no change: your body row(s) are byte-identical \
		 to the file at the targeted lines. The bug is somewhere else — re-read the file before issuing another \
		 edit. Do NOT widen the payload or add lines; verify the anchor first."
	)
}

async fn read_target(path: &Path, token: &CancellationToken) -> Result<String, PrepareError> {
	if token.is_cancelled() {
		return Err(PrepareError::Cancelled);
	}
	let meta = tokio::fs::metadata(path)
		.await
		.map_err(|err| PrepareError::Failed(format!("Failed to edit {}: {err}", path.display())))?;
	if !meta.is_file() {
		return Err(PrepareError::Failed(format!("{} is not a regular file", path.display())));
	}
	let (bytes, size) = read_capped(path, MAX_EDIT_BYTES)
		.await
		.map_err(|err| PrepareError::Failed(format!("{}: {err}", path.display())))?;
	if size > bytes.len() as u64 {
		return Err(PrepareError::Failed(format!(
			"{} is {size} bytes, the edit limit is {MAX_EDIT_BYTES}",
			path.display()
		)));
	}
	String::from_utf8(bytes)
		.map_err(|_| PrepareError::Failed(format!("{} is not valid UTF-8", path.display())))
}

async fn prepare(
	section: PatchSection,
	cwd: &Path,
	token: &CancellationToken,
	batch: &mut Clipboard,
	store: &SnapshotStore,
	options: &EditToolOptions,
	budget: &mut Budget,
) -> Result<Prepared, PrepareError> {
	let parsed: ParsedPatch = parser::parse_section_body(&section.diff)?;
	if section.interleaved && clipboard::has_clipboard_edit(&parsed.edits) {
		return Err(PrepareError::Failed(CLIPBOARD_INTERLEAVED_SECTIONS.to_string()));
	}
	let tag = section
		.file_hash
		.clone()
		.ok_or_else(|| PrepareError::Failed(missing_tag_message(&section.path)))?;

	if !section.file_path.exists() {
		return Err(PrepareError::Failed(format!(
			"File not found: {}. Use the write tool to create new files.",
			section.path
		)));
	}
	let raw = read_target(&section.file_path, token).await?;

	let canonical = snapshot::canonical_key(&section.file_path);
	let mut move_dest: Option<PathBuf> = None;
	let mut dest_canonical: Option<String> = None;
	if let Some(FileOp::Move(dest)) = &parsed.file_op {
		let dest_path = resolve_relative(dest, cwd);
		let dest_key = snapshot::canonical_key(&dest_path);
		if dest_key == canonical {
			return Err(PrepareError::Failed(format!(
				"MV destination is the same as {}.",
				section.path
			)));
		}
		move_dest = Some(dest_path);
		dest_canonical = Some(dest_key);
	}

	let source_raw = raw.clone();

	#[cfg(unix)]
	let (source_mode, source_id) = {
		use std::os::unix::fs::MetadataExt;
		match std::fs::metadata(&section.file_path) {
			Ok(meta) => (Some(meta.mode() & 0o7777), Some((meta.dev(), meta.ino()))),
			Err(_) => (None, None),
		}
	};
	#[cfg(not(unix))]
	let (source_mode, source_id): (Option<u32>, Option<(u64, u64)>) = (None, None);
	let (bom, text) = hash::strip_bom(&raw);
	let bom = bom.to_string();
	let line_ending = hash::detect_line_ending(text);
	let normalized = hash::normalize_to_lf(text);

	let live_tag = hash::compute_file_hash(&normalized);
	let live_matches = live_tag == tag;
	let stored = store.by_hash(&canonical, &tag);
	let line_vec: Vec<String> = normalized.split('\n').map(str::to_string).collect();
	let line_refs: Vec<&str> = line_vec.iter().map(String::as_str).collect();
	let anchor_lines = parser::collect_anchor_lines(&parsed.edits);
	let rejected = |recognized: bool| {
		PrepareError::Failed(mismatch_message(
			&section.path,
			&tag,
			&live_tag,
			recognized,
			&anchor_lines,
			&line_refs,
		))
	};

	// Block anchors index the text the tag names, so a drifted file still
	// resolves them against the recorded snapshot before recovery remaps.
	let mut warnings = parsed.warnings.clone();
	let resolved = if parsed.edits.iter().any(|edit| matches!(edit, parser::Edit::Block { .. })) {
		let base = if live_matches {
			normalized.clone()
		} else {
			match &stored {
				Some(snapshot) => snapshot.text.clone(),
				None => return Err(rejected(false)),
			}
		};
		block::resolve_block_edits(parsed.edits.clone(), &base, &section.path, &mut warnings)?
	} else {
		block::resolve_block_edits(parsed.edits.clone(), &normalized, &section.path, &mut warnings)?
	};
	clipboard::validate_sequence(&resolved, batch, budget)?;
	budget
		.charge_edits(resolved.len())
		.map_err(PrepareError::Failed)?;
	budget
		.charge_bytes(normalized.len() + source_raw.len())
		.map_err(PrepareError::Failed)?;

	let anchor_scoped = parser::has_anchor_scoped_edit(&resolved);
	if !live_matches && !anchor_scoped {
		// Head/tail inserts are position-stable: apply on live, warn instead.
		warnings.push(HEADTAIL_DRIFT_WARNING.to_string());
	}

	let mut after = normalized.clone();
	let mut first_changed_line = None;
	if !matches!(&parsed.file_op, Some(FileOp::Rem)) {
		if live_matches {
			if options.enforce_seen_lines {
				if let Err(message) =
					assert_seen_lines(store, &section, &parsed, &canonical, &normalized, &tag)
				{
					return Err(PrepareError::Failed(message));
				}
			}
			if token.is_cancelled() {
				return Err(PrepareError::Cancelled);
			}
			let result = apply::apply_edits_checked(&normalized, &resolved, batch, Some(token), budget)
				.map_err(map_apply_error)?;
			warnings.extend(result.warnings);
			after = result.text;
			first_changed_line = result.first_changed_line;
		} else if !anchor_scoped {
			if token.is_cancelled() {
				return Err(PrepareError::Cancelled);
			}
			let result = apply::apply_edits_checked(&normalized, &resolved, batch, Some(token), budget)
				.map_err(map_apply_error)?;
			warnings.extend(result.warnings);
			after = result.text;
			first_changed_line = result.first_changed_line;
		} else {
			match recovery::try_recover(
				store, &canonical, &tag, &normalized, &resolved, batch, token, budget,
			) {
				Some(recovered) => {
					warnings.extend(recovered.warnings);
					after = recovered.text;
					first_changed_line = recovered.first_changed;
				}
				None => {
					// Record the live version so a later retry sees it as the
					// current snapshot (upstream `#mismatchError`).
					store.record(&canonical, &normalized, None);
					return Err(rejected(stored.is_some()));
				}
			}
		}
	}
	let noop = parsed.file_op.is_none() && after == normalized;
	budget
		.charge_bytes(after.len())
		.map_err(PrepareError::Failed)?;
	if parsed.file_op.is_none() && !noop && syntax::parses_cleanly(&section.path, &normalized)
		&& !syntax::parses_cleanly(&section.path, &after)
	{
		warnings.push(edit_broke_parse_warning(first_changed_line));
	}

	Ok(Prepared {
		section,
		canonical,
		bom,
		line_ending,
		normalized,
		after,
		first_changed_line,
		warnings,
		file_op: parsed.file_op,
		noop,
		source_raw,
		source_mode,
		source_id,
		move_dest,
		dest_canonical,
	})
}

fn map_apply_error(message: String) -> PrepareError {
	if message == apply::APPLY_CANCELLED {
		PrepareError::Cancelled
	} else {
		PrepareError::Failed(message)
	}
}

fn record_noop(guard: &NoopGuard, canonical: &str, input_hash: u64) -> (u32, bool) {
	let mut guard = guard.lock().unwrap_or_else(|poison| poison.into_inner());
	let count = match guard.get(canonical) {
		Some((hash, count)) if *hash == input_hash => count + 1,
		_ => 1,
	};
	guard.insert(canonical.to_string(), (input_hash, count));
	(count, count >= NOOP_HARD_LIMIT)
}

fn reset_noop(guard: &NoopGuard, canonical: &str) {
	guard
		.lock()
		.unwrap_or_else(|poison| poison.into_inner())
		.remove(canonical);
}

fn no_change_loop_diagnostic(path: &str, count: u32) -> String {
	format!(
		"STOP. Edits to {path} have been a byte-identical no-op {count} times in a row — the patch body \
		 matches the file at the targeted lines and the soft hint did not break the cycle. Cease \
		 re-issuing this payload. Either the intended change is already on disk (move on), or your anchor \
		 is wrong (re-read the file with `read` to observe the current line numbers and tag, then author \
		 a different edit). This exact payload will keep being rejected until it changes."
	)
}

fn edit_broke_parse_warning(first_changed_line: Option<u32>) -> String {
	let at = first_changed_line.map(|line| format!(" near line {line}")).unwrap_or_default();
	format!(
		"This edit introduced a syntax error{at}: the file parsed before the patch and no longer does. \
		 It was applied exactly as written, so a line number or range endpoint is likely wrong — \
		 re-read the touched region and re-issue a correcting edit."
	)
}

/// Reject an anchored edit that references a line the read which minted `tag`
/// never displayed. Reveals the actual content when it fits the inline cap so a
/// straight retry can succeed without a range re-read.
fn assert_seen_lines(
	store: &SnapshotStore,
	section: &PatchSection,
	parsed: &ParsedPatch,
	canonical: &str,
	normalized: &str,
	tag: &str,
) -> Result<(), String> {
	let Some(snapshot) = store.by_content(canonical, normalized) else {
		return Ok(());
	};
	let Some(seen) = snapshot.seen_lines.as_ref().filter(|seen| !seen.is_empty()) else {
		return Ok(());
	};
	let unseen: Vec<u32> = parser::collect_anchor_lines(&parsed.edits)
		.into_iter()
		.filter(|line| !seen.contains(line))
		.collect();
	if unseen.is_empty() {
		return Ok(());
	}

	let source_lines: Vec<&str> = snapshot.text.split('\n').collect();
	let mut revealed: Vec<(u32, String)> = Vec::new();
	let mut column_truncated = false;
	for line in unseen.iter().take(SEEN_LINE_REVEAL_CAP) {
		let Ok(index) = usize::try_from(*line) else { continue };
		let Some(source) = source_lines.get(index - 1) else { continue };
		if source.chars().count() > SEEN_LINE_REVEAL_MAX_COLUMNS {
			revealed.push((*line, format!("{}…", clip_columns(source, SEEN_LINE_REVEAL_MAX_COLUMNS))));
			column_truncated = true;
		} else {
			revealed.push((*line, (*source).to_string()));
		}
	}
	let truncated = unseen.len() > revealed.len() || column_truncated;
	if !truncated {
		let revealed_lines: Vec<u32> = revealed.iter().map(|(line, _)| *line).collect();
		store.record_seen_lines(canonical, tag, &revealed_lines);
	}

	let header = format!(
		"This edit anchors to lines {} of {} that [{}#{tag}] never displayed (it showed a partial range, \
		 a search hit, or a folded summary).",
		format_line_ranges(&unseen),
		section.path,
		section.path
	);
	if revealed.is_empty() {
		return Err(format!(
			"{header} Re-read them in full first with a ranged read like `{}:{}` — it skips summarization and \
			 mints a fresh tag — then re-issue the edit.",
			section.path,
			format_line_ranges(&unseen).replace(", ", ",")
		));
	}
	let preview = revealed
		.iter()
		.map(|(line, text)| format!("  {line}:{text}"))
		.collect::<Vec<_>>()
		.join("\n");
	if truncated {
		return Err(format!(
			"{header} Preview of the actual file content at the first {} unseen line(s):\n{preview}\n\
			 The range exceeds the inline preview cap — re-read the remainder with `{}:{}` before re-issuing \
			 the edit.",
			revealed.len(),
			section.path,
			format_line_ranges(&unseen).replace(", ", ",")
		));
	}
	Err(format!(
		"{header} Actual file content at those lines:\n{preview}\n\
		 Verify the content matches what you intend to touch, then re-issue the edit with the same \
		 `[path#tag]` header — a straight retry now succeeds without a re-read. If the content does NOT \
		 match, fix your line numbers."
	))
}

fn clip_columns(text: &str, columns: usize) -> String {
	text.chars().take(columns).collect()
}

fn format_line_ranges(lines: &[u32]) -> String {
	let mut ranges: Vec<String> = Vec::new();
	let mut index = 0;
	while index < lines.len() {
		let start = lines[index];
		let mut end = start;
		while index + 1 < lines.len() && lines[index + 1] == end + 1 {
			index += 1;
			end = lines[index];
		}
		ranges.push(if start == end { start.to_string() } else { format!("{start}-{end}") });
		index += 1;
	}
	ranges.join(", ")
}

fn resolve_relative(path: &str, cwd: &Path) -> PathBuf {
	let raw = PathBuf::from(path);
	if raw.is_absolute() { raw } else { cwd.join(raw) }
}

async fn assert_prepared_target(
	prepared: &Prepared,
	token: &CancellationToken,
) -> Result<(), CommitError> {
	let path = &prepared.section.file_path;
	let now_canonical = snapshot::canonical_key(path);
	if now_canonical != prepared.canonical {
		return Err(CommitError::Failed(format!(
			"{} was retargeted after the batch was prepared (it now resolves to {now_canonical}); \
			 refusing to write through the new target. Re-read and retry.",
			prepared.section.path
		)));
	}
	// Compare against the exact validated bytes: a 16-bit content tag cannot
	// prove identity, only a recorded snapshot can.
	let canonical_path = PathBuf::from(&prepared.canonical);
	let raw = read_target(&canonical_path, token).await.map_err(|_| {
		CommitError::Failed(format!(
			"{} changed or disappeared after the batch was prepared; refusing to write. Re-read and retry.",
			prepared.section.path
		))
	})?;
	if raw != prepared.source_raw {
		let now_tag = hash::compute_file_hash(&hash::normalize_to_lf(
			hash::strip_bom(&raw).1,
		));
		return Err(CommitError::Failed(format!(
			"{} changed after the batch was prepared (current content hashes to #{now_tag}); \
			 refusing to overwrite bytes that were not validated. Re-read and retry.",
			prepared.section.path
		)));
	}
	#[cfg(unix)]
	if let Some((dev, ino)) = prepared.source_id {
		use std::os::unix::fs::MetadataExt;
		let meta = tokio::fs::metadata(&canonical_path).await.map_err(|_| {
			CommitError::Failed(format!(
				"{} disappeared after the batch was prepared; refusing to write. Re-read and retry.",
				prepared.section.path
			))
		})?;
		if (meta.dev(), meta.ino()) != (dev, ino) {
			return Err(CommitError::Failed(format!(
				"{} was replaced by another file after the batch was prepared \
				 (inode changed); refusing to write. Re-read and retry.",
				prepared.section.path
			)));
		}
	}
	Ok(())
}

async fn commit(
	prepared: Prepared,
	token: &CancellationToken,
	store: &SnapshotStore,
) -> Result<Outcome, CommitError> {
	if token.is_cancelled() {
		return Err(CommitError::Cancelled);
	}
	let target = prepared.section.file_path.clone();

	if matches!(&prepared.file_op, Some(FileOp::Rem)) {
		// Type-check before reading/validating: a replacement directory must
		// fail closed, never be treated as removable content.
		let meta = tokio::fs::symlink_metadata(&target).await.map_err(|err| {
			CommitError::Failed(format!("Failed to stat {} before removing it: {err}", prepared.section.path))
		})?;
		if !meta.is_file() && !meta.file_type().is_symlink() {
			return Err(CommitError::Failed(format!(
				"{} is no longer a regular file or symlink (changed after the batch was \
				 prepared); refusing to remove it. Re-read and retry.",
				prepared.section.path
			)));
		}
		assert_prepared_target(&prepared, token).await?;
		tokio::fs::remove_file(&target).await.map_err(|err| {
			CommitError::Failed(format!(
				"Failed to delete {}: {err}. The file is still present.",
				prepared.section.path
			))
		})?;
		store.invalidate(&prepared.canonical);
		return Ok(Outcome::Deleted { path: prepared.section.path, warnings: prepared.warnings });
	}

	if let Some(FileOp::Move(dest)) = &prepared.file_op {
		assert_prepared_target(&prepared, token).await?;
		let dest_path = prepared.move_dest.clone().ok_or_else(|| {
			CommitError::Failed(format!(
				"Internal error: the move target for {} was not recorded.",
				prepared.section.path
			))
		})?;
		if let Some(expected) = &prepared.dest_canonical {
			if snapshot::canonical_key(&dest_path) != *expected {
				return Err(CommitError::Failed(format!(
					"The MV destination for {} changed after the batch was prepared; nothing was \
					 written and the source is untouched.",
					prepared.section.path
				)));
			}
		}
		let persisted = format!(
			"{}{}",
			prepared.bom,
			hash::restore_line_endings(&prepared.after, prepared.line_ending)
		);
		write_file_mode(&dest_path, persisted.as_bytes(), token, prepared.source_mode)
			.await
			.map_err(|err| match err {
				CommitError::Failed(message) => CommitError::Failed(format!(
					"{message} (destination for {}; source untouched)",
					prepared.section.path
				)),
				other => other,
			})?;
		assert_prepared_target(&prepared, token).await.map_err(|err| match err {
			CommitError::Failed(message) => CommitError::Failed(format!(
				"{message} Destination {} was already written; source is still present \
				 (partial move).",
				dest_path.display()
			)),
			other => other,
		})?;
		if tokio::fs::symlink_metadata(&target).await.is_ok() {
			tokio::fs::remove_file(&target).await.map_err(|err| {
				CommitError::Failed(format!(
					"Moved content to {} but failed to remove {}: {err}. Destination was written; \
					 source is still present (partial move).",
					dest_path.display(),
					prepared.section.path
				))
			})?;
		}
		let dest_canonical = prepared
			.dest_canonical
			.clone()
			.unwrap_or_else(|| snapshot::canonical_key(&dest_path));
		store.relocate(&prepared.canonical, &dest_canonical);
		let tag = store
			.record(&dest_canonical, &prepared.after, None)
			.unwrap_or_else(|| hash::compute_file_hash(&prepared.after));
		return Ok(Outcome::Moved {
			from: prepared.section.path,
			to: dest.clone(),
			tag,
			first_changed: prepared.first_changed_line,
			warnings: prepared.warnings,
		});
	}

	if prepared.noop {
		store.record(&prepared.canonical, &prepared.normalized, None);
		return Ok(Outcome::Noop { path: prepared.section.path, warnings: prepared.warnings });
	}

	assert_prepared_target(&prepared, token).await?;
	if token.is_cancelled() {
		return Err(CommitError::Cancelled);
	}
	let persisted = format!(
		"{}{}",
		prepared.bom,
		hash::restore_line_endings(&prepared.after, prepared.line_ending)
	);
	let canonical_target = PathBuf::from(&prepared.canonical);
	write_file(&canonical_target, persisted.as_bytes(), token).await?;
	let tag = store
		.record(&prepared.canonical, &prepared.after, None)
		.unwrap_or_else(|| hash::compute_file_hash(&prepared.after));
	Ok(Outcome::Updated {
		path: prepared.section.path,
		tag,
		first_changed: prepared.first_changed_line,
		warnings: prepared.warnings,
	})
}

async fn write_file(path: &Path, bytes: &[u8], token: &CancellationToken) -> Result<(), CommitError> {
	write_file_mode(path, bytes, token, None).await
}

/// Atomic write carrying an explicit mode into the temp file, so a moved
/// `0600` file exists at `0600` from the instant it is published.
async fn write_file_mode(
	path: &Path,
	bytes: &[u8],
	token: &CancellationToken,
	mode: Option<u32>,
) -> Result<(), CommitError> {
	match write_atomic_with_mode(path, bytes, token, mode).await {
		Ok(()) => Ok(()),
		Err(err)
			if err.kind() == std::io::ErrorKind::Interrupted || token.is_cancelled() =>
		{
			Err(CommitError::Cancelled)
		}
		Err(err) => Err(CommitError::Failed(format!("Failed to write {}: {err}", path.display()))),
	}
}

fn render(outcome: Outcome) -> String {
	match outcome {
		Outcome::Updated { path, tag, first_changed, warnings } => {
			let mut text = format!("Updated {path}{}", first_suffix(first_changed));
			text.push('\n');
			text.push_str(&snapshot::format_header(&path, &tag));
			append_warnings(&mut text, &warnings);
			text
		}
		Outcome::Deleted { path, warnings } => {
			let mut text = format!("Deleted {path}");
			append_warnings(&mut text, &warnings);
			text
		}
		Outcome::Moved { from, to, tag, first_changed, warnings } => {
			let mut text = format!("Moved {from} -> {to}{}", first_suffix(first_changed));
			text.push('\n');
			text.push_str(&snapshot::format_header(&to, &tag));
			append_warnings(&mut text, &warnings);
			text
		}
		Outcome::Noop { path, warnings } => {
			let mut text = no_change_diagnostic(&path);
			append_warnings(&mut text, &warnings);
			text
		}
	}
}

fn first_suffix(first_changed: Option<u32>) -> String {
	match first_changed {
		Some(line) => format!(" (first changed line {line})"),
		None => String::new(),
	}
}

fn append_warnings(text: &mut String, warnings: &[String]) {
	if warnings.is_empty() {
		return;
	}
	text.push_str("\nWarnings:");
	for warning in warnings {
		text.push_str("\n- ");
		text.push_str(warning);
	}
}

fn tool_result(content: String, is_error: bool) -> ToolResult {
	ToolResult {
		tool_call_id: String::new(),
		content,
		is_error,
		terminate: false,
		details: None,
	}
}

#[cfg(test)]
mod bind_tests {
	use super::*;
	use std::collections::BTreeMap;
	use std::os::unix::fs::PermissionsExt;

	fn section(dir: &Path, path: &str, tag: &str, diff: &str) -> parser::PatchSection {
		let input = format!("[{path}#{tag}]\n{diff}");
		let mut sections = parser::split_patch_sections(&input, Some(dir)).expect("split");
		sections.remove(0)
	}

	#[tokio::test]
	async fn commit_rejects_bytes_replaced_after_prepare() {
		let dir = std::env::temp_dir().join(format!("hl-bind-bytes-{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let path = dir.join("note.txt");
		std::fs::write(&path, "alpha\n").unwrap();
		let store = SnapshotStore::new();
		let _ = store.record(&snapshot::canonical_key(&path), "alpha\n", Some(&[1]));
		let tag = hash::compute_file_hash("alpha\n");
		let token = CancellationToken::new();
		let mut batch = clipboard::start_batch(BTreeMap::new());
		let mut budget = Budget::default();
		let prepared = prepare(
			section(&dir, "note.txt", &tag, "PUT 1.=1:\n+ALPHA\n"),
			&dir,
			&token,
			&mut batch,
			&store,
			&EditToolOptions::default(),
			&mut budget,
		)
		.await
		.expect("prepare");

		std::fs::write(&path, "externally replaced\n").unwrap();
		let err = commit(prepared, &token, &store).await.err().expect("must reject");
		match err {
			CommitError::Failed(message) => {
				assert!(message.contains("changed after the batch was prepared"), "{message}")
			}
			CommitError::Cancelled => panic!("unexpected cancel"),
		}
		assert_eq!(std::fs::read_to_string(&path).unwrap(), "externally replaced\n");
		let _ = std::fs::remove_dir_all(&dir);
	}

	#[tokio::test]
	async fn commit_rejects_retargeted_symlink() {
		let dir = std::env::temp_dir().join(format!("hl-bind-link-{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let safe = dir.join("safe.txt");
		let other = dir.join("other.txt");
		let link = dir.join("link.txt");
		std::fs::write(&safe, "alpha\n").unwrap();
		std::fs::write(&other, "beta\n").unwrap();
		std::os::unix::fs::symlink(&safe, &link).unwrap();
		let store = SnapshotStore::new();
		let _ = store.record(&snapshot::canonical_key(&link), "alpha\n", Some(&[1]));
		let tag = hash::compute_file_hash("alpha\n");
		let token = CancellationToken::new();
		let mut batch = clipboard::start_batch(BTreeMap::new());
		let mut budget = Budget::default();
		let prepared = prepare(
			section(&dir, "link.txt", &tag, "PUT 1.=1:\n+ALPHA\n"),
			&dir,
			&token,
			&mut batch,
			&store,
			&EditToolOptions::default(),
			&mut budget,
		)
		.await
		.expect("prepare");

		std::fs::remove_file(&link).unwrap();
		std::os::unix::fs::symlink(&other, &link).unwrap();
		let err = commit(prepared, &token, &store).await.err().expect("must reject");
		match err {
			CommitError::Failed(message) => assert!(message.contains("retargeted"), "{message}"),
			CommitError::Cancelled => panic!("unexpected cancel"),
		}
		assert_eq!(std::fs::read_to_string(&safe).unwrap(), "alpha\n");
		assert_eq!(std::fs::read_to_string(&other).unwrap(), "beta\n");
		let _ = std::fs::remove_dir_all(&dir);
	}

	#[tokio::test]
	async fn commit_rejects_same_tag_different_bytes() {
		let dir = std::env::temp_dir().join(format!("hl-bind-collision-{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let path = dir.join("note.txt");
		std::fs::write(&path, "alpha\n").unwrap();
		let store = SnapshotStore::new();
		let _ = store.record(&snapshot::canonical_key(&path), "alpha\n", Some(&[1]));
		let tag = hash::compute_file_hash("alpha\n");
		let token = CancellationToken::new();
		let mut batch = clipboard::start_batch(BTreeMap::new());
		let mut budget = Budget::default();
		let prepared = prepare(
			section(&dir, "note.txt", &tag, "PUT 1.=1:\n+ALPHA\n"),
			&dir,
			&token,
			&mut batch,
			&store,
			&EditToolOptions::default(),
			&mut budget,
		)
		.await
		.expect("prepare");

		// Find different bytes with the SAME 16-bit content tag: hash equality
		// alone must not authorize the overwrite.
		let mut collision = None;
		for index in 0..1_000_000u32 {
			let candidate = format!("alpha\n#{}", index);
			if candidate != "alpha\n" && hash::compute_file_hash(&candidate) == tag {
				collision = Some(candidate);
				break;
			}
		}
		let collision = collision.expect("find a 16-bit tag collision");
		std::fs::write(&path, &collision).unwrap();
		let err = commit(prepared, &token, &store).await.err().expect("must reject");
		match err {
			CommitError::Failed(message) => {
				assert!(message.contains("changed after the batch was prepared"), "{message}")
			}
			CommitError::Cancelled => panic!("unexpected cancel"),
		}
		assert_eq!(std::fs::read_to_string(&path).unwrap(), collision);
		let _ = std::fs::remove_dir_all(&dir);
	}

	#[tokio::test]
	async fn rem_rejects_a_replaced_file() {
		let dir = std::env::temp_dir().join(format!("hl-bind-rem-{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let path = dir.join("gone.txt");
		std::fs::write(&path, "alpha\n").unwrap();
		let store = SnapshotStore::new();
		let _ = store.record(&snapshot::canonical_key(&path), "alpha\n", Some(&[1]));
		let tag = hash::compute_file_hash("alpha\n");
		let token = CancellationToken::new();
		let mut batch = clipboard::start_batch(BTreeMap::new());
		let mut budget = Budget::default();
		let prepared = prepare(
			section(&dir, "gone.txt", &tag, "REM"),
			&dir,
			&token,
			&mut batch,
			&store,
			&EditToolOptions::default(),
			&mut budget,
		)
		.await
		.expect("prepare");

		std::fs::write(&path, "replacement\n").unwrap();
		let err = commit(prepared, &token, &store).await.err().expect("must reject");
		match err {
			CommitError::Failed(message) => {
				assert!(message.contains("changed after the batch was prepared"), "{message}")
			}
			CommitError::Cancelled => panic!("unexpected cancel"),
		}
		assert_eq!(std::fs::read_to_string(&path).unwrap(), "replacement\n");
		let _ = std::fs::remove_dir_all(&dir);
	}

	#[tokio::test]
	async fn rem_rejects_directory_replacement() {
		let dir = std::env::temp_dir().join(format!("hl-bind-remdir-{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let path = dir.join("gone.txt");
		std::fs::write(&path, "alpha\n").unwrap();
		let store = SnapshotStore::new();
		let _ = store.record(&snapshot::canonical_key(&path), "alpha\n", Some(&[1]));
		let tag = hash::compute_file_hash("alpha\n");
		let token = CancellationToken::new();
		let mut batch = clipboard::start_batch(BTreeMap::new());
		let mut budget = Budget::default();
		let prepared = prepare(
			section(&dir, "gone.txt", &tag, "REM"),
			&dir,
			&token,
			&mut batch,
			&store,
			&EditToolOptions::default(),
			&mut budget,
		)
		.await
		.expect("prepare");

		std::fs::remove_file(&path).unwrap();
		std::fs::create_dir(&path).unwrap();
		std::fs::write(path.join("kept.txt"), "inside\n").unwrap();
		let err = commit(prepared, &token, &store).await.err().expect("must reject");
		match err {
			CommitError::Failed(message) => {
				assert!(message.contains("no longer a regular file or symlink"), "{message}")
			}
			CommitError::Cancelled => panic!("unexpected cancel"),
		}
		assert_eq!(std::fs::read_to_string(path.join("kept.txt")).unwrap(), "inside\n");
		let _ = std::fs::remove_dir_all(&dir);
	}

	#[tokio::test]
	async fn move_keeps_source_permissions() {
		let dir = std::env::temp_dir().join(format!("hl-bind-mode-{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let private = dir.join("private.txt");
		std::fs::write(&private, "secret\n").unwrap();
		std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o600)).unwrap();
		let store = SnapshotStore::new();
		let _ = store.record(&snapshot::canonical_key(&private), "secret\n", Some(&[1]));
		let tag = hash::compute_file_hash("secret\n");
		let token = CancellationToken::new();
		let mut batch = clipboard::start_batch(BTreeMap::new());
		let mut budget = Budget::default();
		let prepared = prepare(
			section(&dir, "private.txt", &tag, "MV moved.txt"),
			&dir,
			&token,
			&mut batch,
			&store,
			&EditToolOptions::default(),
			&mut budget,
		)
		.await
		.expect("prepare");
		commit(prepared, &token, &store).await.expect("commit");
		let moved = dir.join("moved.txt");
		use std::os::unix::fs::MetadataExt;
		assert_eq!(std::fs::metadata(&moved).unwrap().mode() & 0o777, 0o600);
		let _ = std::fs::remove_dir_all(&dir);
	}
}
