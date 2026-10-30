//! Git-backed task isolation lifecycle for `task` subagents.
//!
//! Mirrors the TypeScript `task/worktree.ts` + `task/isolation-runner.ts`
//! contract while reusing the existing [`pi_iso`] PAL for the writable view
//! and the `git` CLI for everything the PAL does not model:
//!
//! - [`prepare_context`] snapshots the parent's HEAD + staged + unstaged +
//!   untracked content (size-capped) and discovers nested repositories.
//! - [`ensure`] resolves a backend, claims an owned base dir before the mount,
//!   materialises `m/`, then severs the sandbox's git metadata from the parent
//!   so a child's git operations can never mutate the parent checkout.
//! - [`finalize`] captures the child delta against the *baseline snapshot*
//!   (never `HEAD`, which would include inherited WIP and hide agent commits),
//!   writes a binary-safe patch artifact, optionally merges it or fetches a
//!   preserved task branch, then tears the sandbox down.
//!
//! Every mutation that touches the parent repo runs under one process-wide
//! async mutex: pi-agent has no per-repo lock, and two concurrent tasks
//! applying nested patches must not fold a sibling delta into their commit or
//! pop a sibling's stash entry.

use std::{
	collections::HashSet,
	fs,
	path::{Path, PathBuf},
	process::{Command, Stdio},
	sync::{
		Arc, LazyLock, Mutex as StdMutex,
		atomic::{AtomicBool, AtomicU64, Ordering},
	},
	time::{SystemTime, UNIX_EPOCH},
};

use pi_iso::{BackendKind, IsoError};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::{
	provider::{ModelProvider, ToolChoice},
	state::Message,
};

/// Ownership marker written into a task-isolation base dir; byte-compatible
/// with `packages/coding-agent/src/task/isolation-ownership.ts` so
/// `omp worktree clear` sees live sandboxes from either runtime.
pub const ISOLATION_OWNER_FILE: &str = ".omp-isolation-owner.json";

/// Mount dir name under the owned base dir. `merged` is a legacy alias read by
/// the CLI scanner; new sandboxes always use `m`.
pub const ISOLATION_MOUNT_DIR: &str = "m";

/// Ceiling on buffered uncommitted content, matching the TS baseline guard.
pub const ISOLATION_BASELINE_MAX_CONTENT_BYTES: u64 = 1024 * 1024 * 1024;

const WORKTREE_DIR_PREFIX: &str = "t";

/// Serializes every parent-repo mutation: root patch apply, cherry-pick, and
/// nested stash/apply/commit/pop sequences.
static MERGE_LOCK: StdMutex<()> = StdMutex::new(());

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Per-process component of the sandbox directory digest. Including it means a
/// restart never reuses a crashed run's sandbox path, so `ensure` can refuse an
/// existing directory instead of recursively deleting state it does not own.
static PROCESS_NONCE: LazyLock<String> = LazyLock::new(|| {
	let token = process_start_token(std::process::id())
		.unwrap_or_else(|| format!("{}", now_nanos()));
	format!("{}-{token}", std::process::id())
});

/// `task.isolation.mode` values accepted by the settings file.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum IsolationMode {
	#[default]
	None,
	Auto,
	Apfs,
	Btrfs,
	Zfs,
	Reflink,
	Overlayfs,
	Projfs,
	BlockClone,
	Rcopy,
}

impl IsolationMode {
	pub const fn as_str(self) -> &'static str {
		match self {
			Self::None => "none",
			Self::Auto => "auto",
			Self::Apfs => "apfs",
			Self::Btrfs => "btrfs",
			Self::Zfs => "zfs",
			Self::Reflink => "reflink",
			Self::Overlayfs => "overlayfs",
			Self::Projfs => "projfs",
			Self::BlockClone => "block-clone",
			Self::Rcopy => "rcopy",
		}
	}

	/// Parse a settings value, including the legacy aliases the TS runtime
	/// still accepts (`worktree`, `fuse-overlay`, `fuse-projfs`). Unknown text
	/// is an error: silently ignoring it would run a different isolation mode
	/// than the user configured.
	pub fn parse(value: &str) -> Result<Self, String> {
		match value.trim() {
			"none" => Ok(Self::None),
			"auto" => Ok(Self::Auto),
			"apfs" => Ok(Self::Apfs),
			"btrfs" => Ok(Self::Btrfs),
			"zfs" => Ok(Self::Zfs),
			"reflink" => Ok(Self::Reflink),
			"overlayfs" | "fuse-overlay" => Ok(Self::Overlayfs),
			"projfs" | "fuse-projfs" => Ok(Self::Projfs),
			"block-clone" => Ok(Self::BlockClone),
			"rcopy" | "worktree" => Ok(Self::Rcopy),
			other => Err(format!(
				"invalid task.isolation.mode '{other}': expected none|auto|apfs|btrfs|zfs|reflink|overlayfs|projfs|block-clone|rcopy"
			)),
		}
	}

	const fn backend(self) -> Option<BackendKind> {
		match self {
			Self::None | Self::Auto => None,
			Self::Apfs => Some(BackendKind::Apfs),
			Self::Btrfs => Some(BackendKind::Btrfs),
			Self::Zfs => Some(BackendKind::Zfs),
			Self::Reflink => Some(BackendKind::LinuxReflink),
			Self::Overlayfs => Some(BackendKind::Overlayfs),
			Self::Projfs => Some(BackendKind::Projfs),
			Self::BlockClone => Some(BackendKind::WindowsBlockClone),
			Self::Rcopy => Some(BackendKind::Rcopy),
		}
	}
}

/// `task.isolation.merge`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum IsolationMergeMode {
	#[default]
	Patch,
	Branch,
}

impl IsolationMergeMode {
	pub const fn as_str(self) -> &'static str {
		match self {
			Self::Patch => "patch",
			Self::Branch => "branch",
		}
	}

	pub fn parse(value: &str) -> Result<Self, String> {
		match value.trim() {
			"patch" => Ok(Self::Patch),
			"branch" => Ok(Self::Branch),
			other => Err(format!(
				"invalid task.isolation.merge '{other}': expected patch|branch"
			)),
		}
	}
}

/// `task.isolation.commits`. `Ai` generates a validated subject through the
/// resolved child model (`ModelProvider::complete`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum IsolationCommitStyle {
	#[default]
	Generic,
	Ai,
}

impl IsolationCommitStyle {
	pub const fn as_str(self) -> &'static str {
		match self {
			Self::Generic => "generic",
			Self::Ai => "ai",
		}
	}

	pub fn parse(value: &str) -> Result<Self, String> {
		match value.trim() {
			"generic" => Ok(Self::Generic),
			"ai" => Ok(Self::Ai),
			other => Err(format!(
				"invalid task.isolation.commits '{other}': expected generic|ai"
			)),
		}
	}
}

/// Session-scoped isolation policy threaded from the CLI settings into every
/// `TaskTool` through [`crate::supervisor::Supervisor`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskIsolationSettings {
	pub mode: IsolationMode,
	pub apply: bool,
	pub merge: IsolationMergeMode,
	pub commits: IsolationCommitStyle,
	/// Absolute base directory resolved by the CLI (`OMP_WORKTREE_DIR` →
	/// `worktree.base` → data root). `None` is only reachable from embedders
	/// and tests; the task tool then refuses isolation instead of guessing a
	/// base the CLI `worktree clear` command cannot scan.
	pub worktree_base: Option<PathBuf>,
}

impl Default for TaskIsolationSettings {
	fn default() -> Self {
		Self {
			mode: IsolationMode::None,
			apply: true,
			merge: IsolationMergeMode::Patch,
			commits: IsolationCommitStyle::Generic,
			worktree_base: None,
		}
	}
}

impl TaskIsolationSettings {
	fn preferred_backend(&self) -> Result<Option<BackendKind>, String> {
		match self.mode {
			IsolationMode::None => Err(
				"Subagent isolated execution requires task.isolation.mode to be set; current mode is \"none\"."
					.to_string(),
			),
			_ => Ok(self.mode.backend()),
		}
	}
}

/// One spawn's per-call isolation request. `isolated` is `None` when the model
/// omitted the key; `apply`/`merge` are only reachable through the eval
/// `agent()` bridge, matching the TS request shape.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IsolationRequest {
	pub isolated: Option<bool>,
	pub apply: Option<bool>,
	pub merge: Option<IsolationMergeMode>,
}

impl IsolationRequest {
	pub const fn requested(self) -> bool {
		matches!(self.isolated, Some(true))
	}
}

/// Effective policy for one spawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IsolationPolicy {
	pub merge: IsolationMergeMode,
	pub apply: bool,
	pub commits: IsolationCommitStyle,
	pub preferred: Option<BackendKind>,
}

/// Resolve settings + request. Returns `Ok(None)` for the unchanged,
/// non-isolated path.
pub fn resolve_policy(
	settings: &TaskIsolationSettings,
	request: &IsolationRequest,
	invocation_task: bool,
) -> Result<Option<IsolationPolicy>, String> {
	if !request.requested() {
		return Ok(None);
	}
	let preferred = settings.preferred_backend()?;
	Ok(Some(IsolationPolicy {
		merge: request.merge.unwrap_or(settings.merge),
		apply: request.apply.unwrap_or(if invocation_task {
			settings.apply
		} else {
			true
		}),
		commits: settings.commits,
		preferred,
	}))
}

/// Baseline of one repository: everything `git status` would report, buffered
/// as byte-exact patches so the build is independent of the live parent worktree.
#[derive(Debug, Clone)]
pub struct RepoBaseline {
	pub repo_root: PathBuf,
	pub head_commit: String,
	pub staged: Vec<u8>,
	pub unstaged: Vec<u8>,
	pub untracked: Vec<String>,
	pub untracked_patch: Vec<u8>,
	/// Gitlink paths from the index; submodule checkouts can share metadata
	/// with the parent and must be rejected before a child runs.
	pub submodules: Vec<String>,
}

impl RepoBaseline {
	fn is_clean(&self) -> bool {
		!self.head_commit.is_empty()
			&& self.staged.iter().all(u8::is_ascii_whitespace)
			&& self.unstaged.iter().all(u8::is_ascii_whitespace)
			&& self.untracked.is_empty()
	}
}

/// Parent repo plus every nested repo discovered at spawn time.
#[derive(Debug, Clone)]
pub struct WorktreeBaseline {
	pub root: RepoBaseline,
	pub nested: Vec<NestedBaseline>,
}

#[derive(Debug, Clone)]
pub struct NestedBaseline {
	pub relative_path: String,
	pub baseline: RepoBaseline,
}

/// Materialised sandbox. Cleanup is owned by [`IsolationSandbox`], never by the
/// caller's future.
#[derive(Debug, Clone)]
pub struct IsolationHandle {
	pub base_dir: PathBuf,
	pub merged_dir: PathBuf,
	pub backend: BackendKind,
	pub fell_back: bool,
	pub fallback_reason: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NestedPatchArtifact {
	pub relative_path: String,
	pub patch: Vec<u8>,
	pub patch_path: Option<PathBuf>,
}

/// Result of capturing the child delta.
#[derive(Debug, Clone, Default)]
pub struct CapturedChanges {
	pub root_patch: Vec<u8>,
	pub patch_path: Option<PathBuf>,
	pub branch: Option<String>,
	pub base_sha: Option<String>,
	pub nested: Vec<NestedPatchArtifact>,
	/// Why branch mode fell back to the patch artifact (dirty parent baseline).
	pub branch_fallback_reason: Option<String>,
	/// Capture failure that still left a recoverable patch artifact behind.
	pub error: Option<String>,
}

impl CapturedChanges {
	pub fn is_empty(&self) -> bool {
		self.root_patch.iter().all(u8::is_ascii_whitespace)
			&& self.nested.iter().all(|nested| nested.patch.iter().all(u8::is_ascii_whitespace))
	}
}

#[derive(Debug, Clone, Default)]
pub struct CommitMessages {
	pub root: Option<String>,
	pub nested: Vec<Option<String>>,
}

#[derive(Debug, Clone)]
pub struct MergeOutcome {
	pub applied: bool,
	pub had_changes: bool,
	pub summary: String,
	pub merged_branch_for_nested: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NestedRepoPatch {
	pub relative_path: String,
	pub patch: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IsolationArtifacts {
	#[serde(skip_serializing_if = "Option::is_none")]
	pub patch_path: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub branch_name: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub branch_base_sha: Option<String>,
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub nested_patches: Vec<NestedRepoPatch>,
}

/// Model/UI-facing isolation report carried on [`crate::tools::task::SingleResult`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IsolationReport {
	#[serde(skip_serializing_if = "Option::is_none")]
	pub patch_path: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub branch_name: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub base_sha: Option<String>,
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub nested_patches: Vec<NestedRepoPatch>,
	/// `None` means the caller skipped merging (`apply=false`).
	#[serde(skip_serializing_if = "Option::is_none")]
	pub changes_applied: Option<bool>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub backend: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub error: Option<String>,
	/// Human-facing merge/capture summary (including notifications).
	#[serde(skip_serializing_if = "Option::is_none")]
	pub summary: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct IsolationFinalize {
	pub report: Option<IsolationReport>,
	pub summary: String,
	pub cleanup_error: Option<String>,
	/// Cancellation arrived before/during finalize; callers must not mark the
	/// child as completed.
	pub cancelled: bool,
}

/// Owned sandbox whose `Drop` tears the mount down. `ops` is held by every
/// blocking capture/cleanup pair so a force-aborted future cannot delete the
/// merged view while a `spawn_blocking` capture is still reading it.
pub struct IsolationSandbox {
	inner: Arc<SandboxInner>,
}

struct SandboxInner {
	handle: IsolationHandle,
	ops: StdMutex<()>,
	cleaned: AtomicBool,
}

impl std::fmt::Debug for IsolationSandbox {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("IsolationSandbox")
			.field("merged_dir", &self.inner.handle.merged_dir)
			.field("backend", &self.inner.handle.backend)
			.finish()
	}
}

impl IsolationSandbox {
	fn new(handle: IsolationHandle) -> Self {
		Self {
			inner: Arc::new(SandboxInner {
				handle,
				ops: StdMutex::new(()),
				cleaned: AtomicBool::new(false),
			}),
		}
	}

	pub fn merged_dir(&self) -> &Path {
		&self.inner.handle.merged_dir
	}

	pub fn handle(&self) -> &IsolationHandle {
		&self.inner.handle
	}

	/// Explicit teardown. Safe to call more than once.
	pub fn cleanup(&self) -> Option<String> {
		let inner = Arc::clone(&self.inner);
		cleanup_inner(&inner)
	}

	/// Keep the sandbox on disk (uncaptured child work). `Drop` becomes a no-op.
	/// Callers must surface the retained path to the user.
	pub fn retain(&self) {
		self.inner.cleaned.store(true, Ordering::SeqCst);
	}
}

impl Drop for IsolationSandbox {
	fn drop(&mut self) {
		let _ = cleanup_inner(&self.inner);
	}
}

fn cleanup_inner(inner: &SandboxInner) -> Option<String> {
	if inner.cleaned.swap(true, Ordering::SeqCst) {
		return None;
	}
	// Wait for any in-flight capture that already cloned this sandbox.
	let _ops = inner
		.ops
		.lock()
		.unwrap_or_else(|poison| poison.into_inner());
	match cleanup_blocking(&inner.handle) {
		Ok(()) => None,
		Err(error) => {
			eprintln!("warning: {error}");
			Some(error)
		},
	}
}

// ─── git plumbing ───────────────────────────────────────────────────────────

struct GitOutput {
	stdout: Vec<u8>,
	#[allow(dead_code, reason = "kept for diagnostics in error paths")]
	stderr: Vec<u8>,
	code: i32,
}

fn run_git(
	cwd: &Path,
	args: &[&str],
	envs: &[(&str, std::ffi::OsString)],
	allow_exit1: bool,
) -> Result<GitOutput, String> {
	let mut command = Command::new("git");
	command
		.arg("-C")
		.arg(cwd)
		.args(args)
		.env("GIT_TERMINAL_PROMPT", "0")
		.stdin(Stdio::null())
		.stdout(Stdio::piped())
		.stderr(Stdio::piped());
	for (key, value) in envs {
		command.env(key, value);
	}
	let output = command.output().map_err(|error| {
		format!(
			"cannot run git {} in {}: {error}",
			args.join(" "),
			cwd.display()
		)
	})?;
	let code = output.status.code().unwrap_or(-1);
	if !output.status.success() && !(allow_exit1 && code == 1) {
		return Err(format!(
			"git {} failed in {} (exit {code}): {}",
			args.join(" "),
			cwd.display(),
			String::from_utf8_lossy(&output.stderr).trim()
		));
	}
	Ok(GitOutput {
		stdout: output.stdout,
		stderr: output.stderr,
		code,
	})
}

fn git_ok(cwd: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
	Ok(run_git(cwd, args, &[], false)?.stdout)
}

fn git_text(cwd: &Path, args: &[&str]) -> Result<String, String> {
	let bytes = git_ok(cwd, args)?;
	String::from_utf8(bytes)
		.map(|text| text.trim().to_string())
		.map_err(|error| format!("git {} output is not UTF-8: {error}", args.join(" ")))
}

fn split_nul(bytes: &[u8]) -> Result<Vec<String>, String> {
	bytes
		.split(|byte| *byte == 0)
		.filter(|chunk| !chunk.is_empty())
		.map(|chunk| {
			String::from_utf8(chunk.to_vec())
				.map_err(|error| format!("git path is not UTF-8: {error}"))
		})
		.collect()
}

fn temp_path(label: &str) -> PathBuf {
	let seq = TEMP_SEQ.fetch_add(1, Ordering::SeqCst);
	std::env::temp_dir().join(format!(
		"omp-iso-{label}-{}-{seq}-{}",
		std::process::id(),
		now_nanos()
	))
}

fn write_temp(label: &str, bytes: &[u8]) -> Result<PathBuf, String> {
	let path = temp_path(label);
	fs::write(&path, bytes).map_err(|error| format!("write {}: {error}", path.display()))?;
	Ok(path)
}

fn apply_patch(repo: &Path, patch: &[u8], extra: &[&str]) -> Result<(), String> {
	if patch.iter().all(u8::is_ascii_whitespace) {
		return Ok(());
	}
	let path = write_temp("patch", patch)?;
	let path_text = path.to_string_lossy().into_owned();
	let mut args = vec!["apply", "--binary", "--whitespace=nowarn"];
	args.extend_from_slice(extra);
	args.push(path_text.as_str());
	let result = git_ok(repo, &args).map(|_| ());
	let _ = fs::remove_file(&path);
	result
}

fn patch_applies(repo: &Path, patch: &[u8], reverse: bool) -> bool {
	if patch.iter().all(u8::is_ascii_whitespace) {
		return false;
	}
	let Ok(path) = write_temp("check", patch) else {
		return false;
	};
	let path_text = path.to_string_lossy().into_owned();
	let mut args = vec!["apply", "--check", "--binary"];
	if reverse {
		args.push("--reverse");
	}
	args.push(path_text.as_str());
	let applies = git_ok(repo, &args).is_ok();
	let _ = fs::remove_file(&path);
	applies
}

fn resolve_relative(base: &Path, value: &str) -> PathBuf {
	let path = Path::new(value);
	if path.is_absolute() {
		path.to_path_buf()
	} else {
		base.join(path)
	}
}

fn canonical_dir(path: &Path) -> Result<PathBuf, String> {
	fs::canonicalize(path).map_err(|error| format!("canonicalize {}: {error}", path.display()))
}

fn ensure_absolute_dir(path: &Path) -> Result<PathBuf, String> {
	if path.exists() {
		let meta = fs::symlink_metadata(path)
			.map_err(|error| format!("stat {}: {error}", path.display()))?;
		if !meta.is_dir() {
			return Err(format!("{} is not a directory", path.display()));
		}
		return canonical_dir(path);
	}
	fs::create_dir_all(path).map_err(|error| format!("create {}: {error}", path.display()))?;
	canonical_dir(path)
}

fn now_nanos() -> u128 {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map_or(0, |elapsed| elapsed.as_nanos())
}

/// Boot-stable start-time token, byte-compatible with the TS marker reader.
fn process_start_token(pid: u32) -> Option<String> {
	if let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) {
		let rest = stat.rsplit_once(')')?.1;
		let token = rest.split_whitespace().nth(19)?;
		return (!token.is_empty()).then(|| token.to_string());
	}
	let pid_text = pid.to_string();
	let output = Command::new("ps")
		.args(["-o", "lstart=", "-p", pid_text.as_str()])
		.output()
		.ok()?;
	if !output.status.success() {
		return None;
	}
	let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
	(!token.is_empty()).then_some(token)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OwnerMarker<'a> {
	pid: u32,
	id: &'a str,
	#[serde(skip_serializing_if = "Option::is_none")]
	start_token: Option<String>,
}

/// Publish the owner marker atomically before the mount is materialised.
fn write_owner(base_dir: &Path, id: &str) -> Result<(), String> {
	let marker = OwnerMarker {
		pid: std::process::id(),
		id,
		start_token: process_start_token(std::process::id()),
	};
	let bytes = serde_json::to_vec(&marker)
		.map_err(|error| format!("serialize isolation owner marker: {error}"))?;
	let tmp = base_dir.join(format!(
		"{ISOLATION_OWNER_FILE}.tmp-{}",
		std::process::id()
	));
	fs::write(&tmp, bytes).map_err(|error| format!("write {}: {error}", tmp.display()))?;
	let final_path = base_dir.join(ISOLATION_OWNER_FILE);
	fs::rename(&tmp, &final_path).map_err(|error| {
		let _ = fs::remove_file(&tmp);
		format!("publish {}: {error}", final_path.display())
	})
}

fn base_segment(repo_root: &Path, id: &str) -> String {
	let key = format!("{}\0{id}\0{}", repo_root.display(), *PROCESS_NONCE);
	let digest = xxhash_rust::xxh64::xxh64(key.as_bytes(), 0) & 0xF_FFFF_FFFF;
	format!("{}{digest:09x}", WORKTREE_DIR_PREFIX)
}

fn check_disjoint(base: &Path, repo_root: &Path, common_dir: &Path) -> Result<(), String> {
	let overlaps = |a: &Path, b: &Path| a.starts_with(b) || b.starts_with(a);
	if overlaps(base, repo_root) || overlaps(base, common_dir) {
		return Err(format!(
			"refusing isolation sandbox {}: it overlaps the source checkout {} or its git dir {}",
			base.display(),
			repo_root.display(),
			common_dir.display()
		));
	}
	Ok(())
}

// ─── baseline ───────────────────────────────────────────────────────────────

/// Snapshot the parent working tree before any sandbox exists.
pub async fn prepare_context(cwd: PathBuf) -> Result<WorktreeBaseline, String> {
	tokio::task::spawn_blocking(move || prepare_context_blocking(&cwd))
		.await
		.map_err(|error| format!("baseline task failed: {error}"))?
}

fn prepare_context_blocking(cwd: &Path) -> Result<WorktreeBaseline, String> {
	let cwd = canonical_dir(cwd)?;
	let root = match git_text(&cwd, &["rev-parse", "--show-toplevel"]) {
		Ok(root) => canonical_dir(Path::new(&root))?,
		Err(error) => {
			if has_jj_marker(&cwd) {
				return Err(
					"Isolated task execution requires a Git checkout, but this workspace is pure Jujutsu (`.jj/` without a colocated `.git/`). Run `jj git init --colocate` to add a Git checkout, or set `task.isolation.mode: none` to disable task isolation."
						.to_string(),
				);
			}
			return Err(format!(
				"Isolated task execution requires a Git repository, but {} is not inside one: {error}. Set `task.isolation.mode: none` to run tasks without isolation.",
				cwd.display()
			));
		},
	};
	let submodules = submodule_paths(&root)?;
	let mut root_baseline = capture_repo_baseline(&root)?;
	root_baseline.submodules = submodules.iter().cloned().collect();
	root_baseline.submodules.sort();
	let nested = discover_nested_repos(&root, &submodules)
		.into_iter()
		.map(|relative_path| {
			let baseline = capture_repo_baseline(&root.join(&relative_path))?;
			Ok(NestedBaseline {
				relative_path,
				baseline,
			})
		})
		.collect::<Result<Vec<_>, String>>()?;
	Ok(WorktreeBaseline {
		root: root_baseline,
		nested,
	})
}

fn has_jj_marker(start: &Path) -> bool {
	let mut current = Some(start);
	while let Some(dir) = current {
		if dir.join(".jj").is_dir() {
			return true;
		}
		current = dir.parent();
	}
	false
}

fn submodule_paths(repo: &Path) -> Result<HashSet<String>, String> {
	let bytes = git_ok(repo, &["ls-files", "--stage", "-z"])?;
	let mut paths = HashSet::new();
	for entry in bytes.split(|byte| *byte == 0) {
		let Some(tab) = entry.iter().position(|byte| *byte == b'\t') else {
			continue;
		};
		let meta = String::from_utf8_lossy(&entry[..tab]);
		if meta.starts_with("160000 ") {
			let path = String::from_utf8_lossy(&entry[tab + 1..]).into_owned();
			paths.insert(path);
		}
	}
	Ok(paths)
}

fn capture_repo_baseline(repo_root: &Path) -> Result<RepoBaseline, String> {
	let repo_root = canonical_dir(repo_root)?;
	let head_commit = git_text(&repo_root, &["rev-parse", "--verify", "HEAD"]).unwrap_or_default();
	let staged = git_ok(
		&repo_root,
		&["diff", "--binary", "--no-color", "--cached"],
	)?;
	let unstaged = git_ok(&repo_root, &["diff", "--binary", "--no-color"])?;
	let untracked = split_nul(&git_ok(
		&repo_root,
		&["ls-files", "--others", "--exclude-standard", "-z"],
	)?)?;

	let mut untracked_bytes = 0_u64;
	for relative in &untracked {
		if let Ok(meta) = fs::symlink_metadata(repo_root.join(relative))
			&& meta.is_file()
		{
			untracked_bytes = untracked_bytes.saturating_add(meta.len());
		}
	}
	let content_bytes = untracked_bytes
		.saturating_add(staged.len() as u64)
		.saturating_add(unstaged.len() as u64);
	if content_bytes > ISOLATION_BASELINE_MAX_CONTENT_BYTES {
		return Err(format!(
			"Working tree at {} carries {} of uncommitted content, over the {} isolation-snapshot budget. Commit or gitignore the bulk (untracked files that are not ignored are the usual culprit), or set `task.isolation.mode: none` to run tasks without isolation.",
			repo_root.display(),
			format_bytes(content_bytes),
			format_bytes(ISOLATION_BASELINE_MAX_CONTENT_BYTES)
		));
	}

	let mut untracked_patch = Vec::new();
	for relative in &untracked {
		let patch = no_index_patch(&repo_root, relative)?;
		if !patch.is_empty() {
			if !untracked_patch.is_empty() && !untracked_patch.ends_with(b"\n") {
				untracked_patch.push(b'\n');
			}
			untracked_patch.extend_from_slice(&patch);
		}
	}

	Ok(RepoBaseline {
		repo_root,
		head_commit,
		staged,
		unstaged,
		untracked,
		untracked_patch,
		submodules: Vec::new(),
	})
}

fn format_bytes(bytes: u64) -> String {
	const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
	let mut value = bytes as f64;
	let mut unit = 0;
	while value >= 1024.0 && unit + 1 < UNITS.len() {
		value /= 1024.0;
		unit += 1;
	}
	if unit == 0 {
		format!("{bytes} B")
	} else {
		format!("{value:.1} {}", UNITS[unit])
	}
}

fn no_index_patch(repo: &Path, relative: &str) -> Result<Vec<u8>, String> {
	let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
	let output = run_git(
		repo,
		&[
			"diff",
			"--no-index",
			"--binary",
			"--no-color",
			"--",
			null,
			relative,
		],
		&[],
		true,
	)?;
	Ok(if output.code == 1 { output.stdout } else { Vec::new() })
}

fn discover_nested_repos(root: &Path, submodules: &HashSet<String>) -> Vec<String> {
	fn walk(root: &Path, dir: &Path, submodules: &HashSet<String>, out: &mut Vec<String>) {
		let Ok(entries) = fs::read_dir(dir) else {
			return;
		};
		let mut entries = entries.filter_map(Result::ok).collect::<Vec<_>>();
		entries.sort_by_key(|entry| entry.file_name());
		for entry in entries {
			let name = entry.file_name();
			let name = name.to_string_lossy();
			if name == ".git" || name == "node_modules" {
				continue;
			}
			let Ok(file_type) = entry.file_type() else {
				continue;
			};
			if !file_type.is_dir() || file_type.is_symlink() {
				continue;
			}
			let full = entry.path();
			let Ok(relative) = full.strip_prefix(root) else {
				continue;
			};
			let relative = relative.to_string_lossy().replace('\\', "/");
			if full.join(".git").exists() {
				if !submodules.contains(&relative) {
					out.push(relative);
				}
				continue;
			}
			walk(root, &full, submodules, out);
		}
	}
	let mut out = Vec::new();
	walk(root, root, submodules, &mut out);
	out
}

fn write_synthetic_tree(repo: &Path, baseline: &RepoBaseline) -> Result<String, String> {
	let index = temp_path("index");
	if baseline.head_commit.is_empty() {
		run_git(
			repo,
			&["read-tree", "--empty"],
			&[("GIT_INDEX_FILE", index.clone().into_os_string())],
			false,
		)?;
	} else {
		run_git(
			repo,
			&["read-tree", baseline.head_commit.as_str()],
			&[("GIT_INDEX_FILE", index.clone().into_os_string())],
			false,
		)?;
	}
	let envs = [("GIT_INDEX_FILE", index.clone().into_os_string())];
	for patch in [&baseline.staged, &baseline.unstaged, &baseline.untracked_patch] {
		if patch.iter().all(u8::is_ascii_whitespace) {
			continue;
		}
		let path = write_temp("baseline", patch)?;
		let path_text = path.to_string_lossy().into_owned();
		let result = run_git(
			repo,
			&[
				"apply",
				"--cached",
				"--binary",
				"--whitespace=nowarn",
				path_text.as_str(),
			],
			&envs,
			false,
		);
		let _ = fs::remove_file(&path);
		if let Err(err) = result {
			let _ = fs::remove_file(&index);
			let _ = fs::remove_file(index.with_extension("lock"));
			return Err(err);
		}
	}
	let tree = run_git(repo, &["write-tree"], &envs, false).and_then(|output| {
		String::from_utf8(output.stdout)
			.map(|text| text.trim().to_string())
			.map_err(|error| format!("git write-tree output is not UTF-8: {error}"))
	});
	let _ = fs::remove_file(&index);
	let _ = fs::remove_file(index.with_extension("lock"));
	tree
}

fn synthesize_worktree(repo: &Path) -> Result<RepoBaseline, String> {
	let head_commit = git_text(repo, &["rev-parse", "--verify", "HEAD"]).unwrap_or_default();
	let staged = git_ok(repo, &["diff", "--binary", "--no-color", "--cached"])?;
	let unstaged = git_ok(repo, &["diff", "--binary", "--no-color"])?;
	let untracked = split_nul(&git_ok(
		repo,
		&["ls-files", "--others", "--exclude-standard", "-z"],
	)?)?;
	let mut untracked_patch = Vec::new();
	for relative in &untracked {
		let patch = no_index_patch(repo, relative)?;
		if patch.is_empty() {
			continue;
		}
		if !untracked_patch.is_empty() && !untracked_patch.ends_with(b"\n") {
			untracked_patch.push(b'\n');
		}
		untracked_patch.extend_from_slice(&patch);
	}
	Ok(RepoBaseline {
		repo_root: repo.to_path_buf(),
		head_commit,
		staged,
		unstaged,
		untracked,
		untracked_patch,
		submodules: Vec::new(),
	})
}

fn capture_repo_delta(worktree: &Path, baseline: &RepoBaseline) -> Result<Vec<u8>, String> {
	// Build BOTH synthetic trees in the child ODB. Copied backends (APFS,
	// btrfs, reflink, block-clone) have a private object DB with no alternates
	// back to the parent, so a baseline tree created in the parent ODB would be
	// invisible here. The baseline patches are frozen bytes and apply against
	// baseline HEAD, which the child copy/alternates always has.
	let baseline_tree = write_synthetic_tree(worktree, baseline)?;
	let current = synthesize_worktree(worktree)?;
	let current_tree = write_synthetic_tree(worktree, &current)?;
	git_ok(
		worktree,
		&[
			"diff",
			"--binary",
			"--no-color",
			baseline_tree.as_str(),
			current_tree.as_str(),
		],
	)
}

// ─── sandbox creation ───────────────────────────────────────────────────────

/// Materialise a sandbox. The returned value owns cleanup: if the awaiting
/// future is dropped, the `spawn_blocking` task still returns a sandbox whose
/// `Drop` tears the mount down.
pub async fn ensure(
	repo_root: PathBuf,
	id: String,
	policy: IsolationPolicy,
	worktree_base: Option<PathBuf>,
) -> Result<IsolationSandbox, String> {
	let base = worktree_base.ok_or_else(|| {
		"task isolation has no worktree base configured; the CLI must resolve `worktree.base`".to_string()
	})?;
	let sandbox = tokio::task::spawn_blocking(move || {
		ensure_blocking(&repo_root, &id, policy, &base).map(IsolationSandbox::new)
	})
	.await
	.map_err(|error| format!("isolation setup task failed: {error}"))??;
	Ok(sandbox)
}

fn ensure_blocking(
	repo_root: &Path,
	id: &str,
	policy: IsolationPolicy,
	worktree_base: &Path,
) -> Result<IsolationHandle, String> {
	let repo_root = canonical_dir(repo_root)?;
	let common = resolve_relative(
		&repo_root,
		&git_text(&repo_root, &["rev-parse", "--git-common-dir"])?,
	);
	let common = canonical_dir(&common)?;
	let base_root = ensure_absolute_dir(worktree_base)?;
	check_disjoint(&base_root, &repo_root, &common)?;
	let base_dir = base_root.join(base_segment(&repo_root, id));
	check_disjoint(&base_dir, &repo_root, &common)?;
	if base_dir.exists() {
		return Err(format!(
			"isolation sandbox {} already exists; run `omp worktree clear` and retry",
			base_dir.display()
		));
	}
	let merged_dir = base_dir.join(ISOLATION_MOUNT_DIR);

	let resolution = pi_iso::resolve(policy.preferred);
	let mut fallback_reason = resolution.reason;
	let mut candidates = resolution.candidates;
	if candidates.is_empty() {
		candidates.push(BackendKind::Rcopy);
	}

	for candidate in candidates {
		let staging = base_root.join(format!(
			".isolation-claim-{}-{}",
			std::process::id(),
			TEMP_SEQ.fetch_add(1, Ordering::SeqCst)
		));
		if let Err(error) = fs::create_dir(&staging) {
			return Err(format!("claim {}: {error}", staging.display()));
		}
		if let Err(error) = write_owner(&staging, id) {
			let _ = fs::remove_dir_all(&staging);
			return Err(error);
		}
		// Publish the fully-formed base (dir + owner marker) atomically, so the
		// CLI's orphan scan can never observe an unclaimed live sandbox.
		if let Err(error) = fs::rename(&staging, &base_dir) {
			let _ = fs::remove_dir_all(&staging);
			return Err(format!("publish {}: {error}", base_dir.display()));
		}
		match pi_iso::backend(candidate).start(&repo_root, &merged_dir) {
			Ok(()) => {
				if let Err(error) = detach_git_metadata(&merged_dir, &common) {
					if pi_iso::backend(candidate).stop(&merged_dir).is_ok() {
						let _ = fs::remove_dir_all(&base_dir);
					}
					return Err(format!(
						"isolation backend {} started but the sandbox could not be detached from {}: {error}",
						candidate.as_str(),
						repo_root.display()
					));
				}
				return Ok(IsolationHandle {
					base_dir,
					merged_dir,
					backend: candidate,
					fell_back: resolution.fell_back || candidate != resolution.kind,
					fallback_reason,
				});
			},
			Err(IsoError::Unavailable(reason)) => {
				fallback_reason.get_or_insert(reason);
				stop_and_discard(candidate, &merged_dir, &base_dir)?;
			},
			Err(error) => {
				stop_and_discard(candidate, &merged_dir, &base_dir)?;
				return Err(format!(
					"isolation backend {} failed for {}: {error}",
					candidate.as_str(),
					repo_root.display()
				));
			},
		}
	}
	Err(fallback_reason
		.unwrap_or_else(|| "No isolation backend is available.".to_string()))
}

fn stop_and_discard(
	backend: BackendKind,
	merged_dir: &Path,
	base_dir: &Path,
) -> Result<(), String> {
	match pi_iso::backend(backend).stop(merged_dir) {
		Ok(()) => fs::remove_dir_all(base_dir).map_err(|error| {
			format!("remove failed sandbox {}: {error}", base_dir.display())
		}),
		Err(error) => Err(format!(
			"isolation backend {} stop failed after setup error ({error}); sandbox retained at {} for `omp worktree clear`",
			backend.as_str(),
			base_dir.display()
		)),
	}
}

/// Harden a freshly materialised sandbox before any child starts:
/// - reject submodule checkouts whose git metadata would stay shared,
/// - detach every nested repo's linked metadata from its source admin dir,
/// - materialise the frozen baseline trees in the child object DB.
///
/// Failure here is a setup failure (no child ran), so the caller may discard
/// the sandbox; the parent baseline itself is untouched.
pub async fn prepare_sandbox(
	sandbox: &IsolationSandbox,
	baseline: &WorktreeBaseline,
) -> Result<(), String> {
	let inner = Arc::clone(&sandbox.inner);
	let baseline = baseline.clone();
	tokio::task::spawn_blocking(move || prepare_sandbox_blocking(&inner, &baseline))
		.await
		.map_err(|error| format!("isolation preparation task failed: {error}"))?
}

fn prepare_sandbox_blocking(
	inner: &SandboxInner,
	baseline: &WorktreeBaseline,
) -> Result<(), String> {
	let _ops = inner
		.ops
		.lock()
		.unwrap_or_else(|poison| poison.into_inner());
	let merged = &inner.handle.merged_dir;

	for submodule in &baseline.root.submodules {
		let source = baseline.root.repo_root.join(submodule);
		if !source.join(".git").exists() {
			continue;
		}
		let destination = merged.join(submodule);
		if !destination.exists() {
			fs::create_dir_all(&destination).map_err(|error| {
				format!("create submodule sandbox {}: {error}", destination.display())
			})?;
		}
		let admin = git_admin_dir(&source)?;
		let common = git_common_dir(&source)?;
		point_gitdir(&destination, &admin)?;
		detach_git_metadata(&destination, &common)?;
	}

	for entry in &baseline.nested {
		let destination = merged.join(&entry.relative_path);
		if !destination.exists() {
			fs::create_dir_all(&destination).map_err(|error| {
				format!("create nested sandbox {}: {error}", destination.display())
			})?;
		}
		let admin = git_admin_dir(&entry.baseline.repo_root)?;
		let common = git_common_dir(&entry.baseline.repo_root)?;
		point_gitdir(&destination, &admin)?;
		detach_git_metadata(&destination, &common)?;
		write_synthetic_tree(&destination, &entry.baseline)?;
	}
	write_synthetic_tree(merged, &baseline.root)?;
	Ok(())
}

fn git_common_dir(repo: &Path) -> Result<PathBuf, String> {
	let raw = git_text(repo, &["rev-parse", "--git-common-dir"])?;
	canonical_dir(&resolve_relative(repo, &raw))
}

/// Absolute git admin dir for `repo` (`.git` for a normal repo, the worktree
/// admin dir for a linked worktree/submodule).
fn git_admin_dir(repo: &Path) -> Result<PathBuf, String> {
	let raw = git_text(repo, &["rev-parse", "--absolute-git-dir"])?;
	canonical_dir(Path::new(&raw))
}

/// Point `destination/.git` at `admin` without touching `admin` itself. The
/// copied sandbox may carry a directory, a broken relative pointer, or a
/// pointer into the parent; replacing it here lets `detach_git_metadata` copy
/// the admin privately before severing the alternates edge.
fn point_gitdir(destination: &Path, admin: &Path) -> Result<(), String> {
	let entry = destination.join(".git");
	match fs::symlink_metadata(&entry) {
		Ok(meta) if meta.is_dir() => fs::remove_dir_all(&entry)
			.map_err(|error| format!("remove {}: {error}", entry.display()))?,
		Ok(_) => fs::remove_file(&entry)
			.map_err(|error| format!("remove {}: {error}", entry.display()))?,
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
		Err(error) => return Err(format!("stat {}: {error}", entry.display())),
	}
	fs::create_dir_all(destination)
		.map_err(|error| format!("create {}: {error}", destination.display()))?;
	fs::write(&entry, format!("gitdir: {}\n", admin.display()))
		.map_err(|error| format!("write {}: {error}", entry.display()))
}

/// Remove the sandbox. Never recurses into a still-mounted view: a stop error
/// leaves the owned base dir in place for `omp worktree clear`.
pub fn cleanup_blocking(handle: &IsolationHandle) -> Result<(), String> {
	match pi_iso::backend(handle.backend).stop(&handle.merged_dir) {
		Ok(()) => match fs::remove_dir_all(&handle.base_dir) {
			Ok(()) => Ok(()),
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
			Err(error) => Err(format!(
				"isolation sandbox {} retained: {error}",
				handle.base_dir.display()
			)),
		},
		Err(error) => Err(format!(
			"isolation backend {} stop failed ({error}); sandbox retained at {} for `omp worktree clear`",
			handle.backend.as_str(),
			handle.base_dir.display()
		)),
	}
}

// ─── git metadata detach ────────────────────────────────────────────────────

fn registered_admin(git_entry: &Path) -> Result<Option<PathBuf>, String> {
	let text = fs::read_to_string(git_entry)
		.map_err(|error| format!("read {}: {error}", git_entry.display()))?;
	let Some(raw) = text.trim().strip_prefix("gitdir:") else {
		return Ok(None);
	};
	let admin = normalize_path(&git_entry.parent().unwrap_or(Path::new(".")).join(raw.trim()));
	let back = fs::read_to_string(admin.join("gitdir")).unwrap_or_default();
	let back = fs::canonicalize(back.trim()).unwrap_or_else(|_| normalize_path(Path::new(back.trim())));
	let entry = fs::canonicalize(git_entry).unwrap_or_else(|_| normalize_path(git_entry));
	Ok((back == entry).then_some(admin))
}

fn normalize_path(path: &Path) -> PathBuf {
	let mut out = PathBuf::new();
	for component in path.components() {
		match component {
			std::path::Component::CurDir => {},
			std::path::Component::ParentDir => {
				out.pop();
			},
			other => out.push(other.as_os_str()),
		}
	}
	out
}

fn read_prefixed(dir: &Path, prefix: &str) -> Vec<(String, Vec<u8>)> {
	let Ok(entries) = fs::read_dir(dir) else {
		return Vec::new();
	};
	entries
		.filter_map(Result::ok)
		.filter_map(|entry| {
			let name = entry.file_name().to_string_lossy().into_owned();
			if !name.starts_with(prefix) {
				return None;
			}
			let bytes = fs::read(entry.path()).ok()?;
			Some((name, bytes))
		})
		.collect()
}

fn snapshot_refs(merged: &Path) -> Result<Vec<(String, String)>, String> {
	let text = git_text(
		merged,
		&["for-each-ref", "--format=%(refname) %(objectname)"],
	)?;
	Ok(text
		.lines()
		.filter_map(|line| {
			let (name, id) = line.split_once(' ')?;
			Some((name.to_string(), id.to_string()))
		})
		.collect())
}

/// Sever a copied/linked worktree from shared git metadata while borrowing the
/// source object database through `alternates`. Mirrors
/// `pi-vcs::git::detach_git_dir` with the git CLI so pi-agent needs no gix/jj
/// dependency.
fn detach_git_metadata(merged: &Path, source_common: &Path) -> Result<(), String> {
	let entry = merged.join(".git");
	let meta = match fs::symlink_metadata(&entry) {
		Ok(meta) => meta,
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
		Err(error) => return Err(format!("stat {}: {error}", entry.display())),
	};
	let iso_common = resolve_relative(
		merged,
		&git_text(merged, &["rev-parse", "--git-common-dir"])?,
	);
	let iso_common = canonical_dir(&iso_common)?;
	let source_common = canonical_dir(source_common)?;
	if iso_common != source_common {
		return Ok(());
	}

	let git_dir = if meta.is_dir() {
		entry.clone()
	} else {
		let text = fs::read_to_string(&entry)
			.map_err(|error| format!("read {}: {error}", entry.display()))?;
		let raw = text
			.trim()
			.strip_prefix("gitdir:")
			.ok_or_else(|| format!("{} is not a gitdir pointer", entry.display()))?;
		resolve_relative(merged, raw.trim())
	};
	let admin = if meta.is_file() {
		registered_admin(&entry)?
	} else {
		None
	};

	let head = fs::read(git_dir.join("HEAD")).ok();
	let index = fs::read(git_dir.join("index")).ok();
	let shared = read_prefixed(&git_dir, "sharedindex.");
	let sparse = fs::read(git_dir.join("info/sparse-checkout")).ok();
	let shallow = fs::read(source_common.join("shallow")).ok();
	let refs = snapshot_refs(merged)?;
	let configs = [
		"user.name",
		"user.email",
		"core.fileMode",
		"core.splitIndex",
		"core.sparseCheckout",
		"core.sparseCheckoutCone",
		"commit.gpgsign",
	]
	.into_iter()
	.filter_map(|key| {
		git_text(merged, &["config", "--get", key])
			.ok()
			.filter(|value| !value.is_empty())
			.map(|value| (key, value))
	})
	.collect::<Vec<_>>();

	if meta.is_dir() {
		fs::remove_dir_all(&entry)
			.map_err(|error| format!("remove {}: {error}", entry.display()))?;
	} else {
		fs::remove_file(&entry)
			.map_err(|error| format!("remove {}: {error}", entry.display()))?;
	}
	if let Some(admin) = admin
		&& admin.exists()
	{
		fs::remove_dir_all(&admin)
			.map_err(|error| format!("remove worktree admin {}: {error}", admin.display()))?;
	}

	git_ok(merged, &["init", "-q"])?;
	let git_dir = merged.join(".git");
	let objects_info = git_dir.join("objects/info");
	fs::create_dir_all(&objects_info)
		.map_err(|error| format!("create {}: {error}", objects_info.display()))?;
	let mut alternates = vec![source_common.join("objects")];
	if let Ok(chained) = fs::read_to_string(source_common.join("objects/info/alternates")) {
		for line in chained.lines().map(str::trim).filter(|line| !line.is_empty()) {
			let path = Path::new(line);
			alternates.push(if path.is_absolute() {
				path.to_path_buf()
			} else {
				source_common.join("objects").join(path)
			});
		}
	}
	let alternate_text = alternates
		.iter()
		.map(|path| path.to_string_lossy())
		.collect::<Vec<_>>()
		.join("\n")
		+ "\n";
	fs::write(objects_info.join("alternates"), alternate_text)
		.map_err(|error| format!("write alternates: {error}"))?;
	for (name, id) in refs {
		let path = git_dir.join(&name);
		if let Some(parent) = path.parent() {
			fs::create_dir_all(parent)
				.map_err(|error| format!("create {}: {error}", parent.display()))?;
		}
		fs::write(&path, format!("{id}\n"))
			.map_err(|error| format!("write ref {}: {error}", path.display()))?;
	}
	if let Some(head) = head {
		fs::write(git_dir.join("HEAD"), head)
			.map_err(|error| format!("write HEAD: {error}"))?;
	}
	for (key, value) in configs {
		git_ok(merged, &["config", "--local", key, value.as_str()])?;
	}
	if let Some(bytes) = shallow {
		fs::write(git_dir.join("shallow"), bytes)
			.map_err(|error| format!("write shallow: {error}"))?;
	}
	if let Some(bytes) = sparse {
		fs::create_dir_all(git_dir.join("info"))
			.map_err(|error| format!("create info: {error}"))?;
		fs::write(git_dir.join("info/sparse-checkout"), bytes)
			.map_err(|error| format!("write sparse-checkout: {error}"))?;
	}
	for (name, bytes) in shared {
		fs::write(git_dir.join(name), bytes)
			.map_err(|error| format!("write shared index: {error}"))?;
	}
	if let Some(bytes) = index {
		fs::write(git_dir.join("index"), bytes)
			.map_err(|error| format!("write index: {error}"))?;
	} else {
		let _ = git_ok(merged, &["read-tree", "HEAD"]);
	}
	Ok(())
}

// ─── capture ────────────────────────────────────────────────────────────────

fn capture_blocking(
	sandbox: &SandboxInner,
	baseline: &WorktreeBaseline,
	id: &str,
	artifacts_dir: &Path,
	_merge: IsolationMergeMode,
) -> Result<CapturedChanges, String> {
	let _ops = sandbox
		.ops
		.lock()
		.unwrap_or_else(|poison| poison.into_inner());
	let merged = &sandbox.handle.merged_dir;

	let root_patch = capture_repo_delta(merged, &baseline.root)?;
	fs::create_dir_all(artifacts_dir)
		.map_err(|error| format!("create artifacts dir {}: {error}", artifacts_dir.display()))?;
	let patch_path = artifacts_dir.join(format!("{id}.patch"));
	fs::write(&patch_path, &root_patch)
		.map_err(|error| format!("write patch {}: {error}", patch_path.display()))?;

	let mut nested = Vec::new();
	for entry in &baseline.nested {
		let nested_dir = merged.join(&entry.relative_path);
		if !nested_dir.join(".git").exists() {
			continue;
		}
		let patch = capture_repo_delta(&nested_dir, &entry.baseline)?;
		if patch.iter().all(u8::is_ascii_whitespace) {
			continue;
		}
		let sanitized = entry.relative_path.replace(['/', '\\'], "__");
		let nested_path = artifacts_dir.join(format!("{id}.nested.{sanitized}.patch"));
		fs::write(&nested_path, &patch)
			.map_err(|error| format!("write nested patch {}: {error}", nested_path.display()))?;
		nested.push(NestedPatchArtifact {
			relative_path: entry.relative_path.clone(),
			patch,
			patch_path: Some(nested_path),
		});
	}

	let captured = CapturedChanges {
		root_patch,
		patch_path: Some(patch_path),
		nested,
		..CapturedChanges::default()
	};
	Ok(captured)
}

/// Prepare a parent task branch without ever applying baseline WIP to the
/// parent history.
///
/// - Clean baseline: fetch `HEAD` (plus a trailing commit for uncommitted
///   child work) and cherry-pick `baselineHead..branch`.
/// - Dirty baseline with agent commits: create a synthetic baseline commit `B`
///   whose tree is HEAD+WIP in the child ODB, rebase the agent commits onto
///   `B` with git's 3-way merge machinery (WIP-only hunks collapse), then fetch
///   `B..HEAD`; the parent cherry-picks only filtered agent commits.
/// - Dirty baseline without agent commits: no commit history to preserve, so
///   the exact baseline→current delta patch is applied instead.
fn prepare_branch_capture(
	merged: &Path,
	baseline: &RepoBaseline,
	id: &str,
	root_message: Option<&str>,
) -> Result<Option<(String, String)>, String> {
	if baseline.head_commit.is_empty() {
		return Ok(None);
	}
	let parent = &baseline.repo_root;
	let branch = format!("omp/task/{id}");
	let branch_ref = format!("refs/heads/{branch}");
	if git_ok(parent, &["rev-parse", "--verify", "--quiet", branch_ref.as_str()]).is_ok() {
		return Err(format!(
			"task branch {branch} already exists; changes kept in the patch artifact"
		));
	}
	let head = git_text(merged, &["rev-parse", "--verify", "HEAD"]).unwrap_or_default();
	if head.is_empty() {
		return Ok(None);
	}
	let committed = head != baseline.head_commit;
	let dirty_baseline = !baseline.is_clean();
	if dirty_baseline && !committed {
		return Ok(None);
	}

	// Gate on the real worktree, never the net baseline→final patch: a child
	// may commit A then leave an uncommitted revert, leaving net delta empty
	// while HEAD still carries A.
	ensure_trailing_commit(merged, root_message)?;

	let mut base_sha = baseline.head_commit.clone();
	if committed && dirty_baseline {
		let synthetic = create_baseline_commit(merged, baseline, id)?;
		rebase_onto_baseline(merged, &synthetic, &baseline.head_commit)?;
		base_sha = synthetic;
	}

	let current_head = git_text(merged, &["rev-parse", "HEAD"])?;
	if current_head == baseline.head_commit {
		return Ok(None);
	}
	fetch_task_branch(parent, merged, &branch)?;
	Ok(Some((branch, base_sha)))
}

fn ensure_trailing_commit(merged: &Path, message: Option<&str>) -> Result<bool, String> {
	let status = git_ok(merged, &["status", "--porcelain"])?;
	if status.is_empty() {
		return Ok(false);
	}
	git_ok(merged, &["add", "-A"])?;
	let message = message.unwrap_or("omp: apply isolated changes");
	git_ok(
		merged,
		&["-c", "commit.gpgsign=false", "commit", "--no-verify", "-m", message],
	)
	.or_else(|_| {
		git_ok(
			merged,
			&[
				"-c",
				"user.name=omp",
				"-c",
				"user.email=omp@localhost",
				"-c",
				"commit.gpgsign=false",
				"commit",
				"--no-verify",
				"-m",
				message,
			],
		)
	})?;
	Ok(true)
}

fn create_baseline_commit(
	merged: &Path,
	baseline: &RepoBaseline,
	id: &str,
) -> Result<String, String> {
	let tree = write_synthetic_tree(merged, baseline)?;
	let message = format!("omp: isolation baseline {id}");
	git_text(
		merged,
		&[
			"-c",
			"user.name=omp",
			"-c",
			"user.email=omp@localhost",
			"commit-tree",
			tree.as_str(),
			"-p",
			baseline.head_commit.as_str(),
			"-m",
			message.as_str(),
		],
	)
}

fn rebase_onto_baseline(
	merged: &Path,
	baseline_commit: &str,
	baseline_head: &str,
) -> Result<(), String> {
	let result = git_ok(
		merged,
		&[
			"-c",
			"user.name=omp",
			"-c",
			"user.email=omp@localhost",
			"-c",
			"commit.gpgsign=false",
			"rebase",
			"--onto",
			baseline_commit,
			baseline_head,
			"HEAD",
		],
	);
	if let Err(error) = result {
		let _ = git_ok(merged, &["rebase", "--abort"]);
		return Err(format!(
			"filtering agent commits over the dirty baseline failed: {error}"
		));
	}
	Ok(())
}

fn fetch_task_branch(parent: &Path, merged: &Path, branch: &str) -> Result<(), String> {
	let merged_text = merged.to_string_lossy().into_owned();
	let refspec = format!("HEAD:refs/heads/{branch}");
	git_ok(
		parent,
		&[
			"fetch",
			"--no-tags",
			"--no-write-fetch-head",
			merged_text.as_str(),
			refspec.as_str(),
		],
	)
	.map(|_| ())
}

// ─── merge ──────────────────────────────────────────────────────────────────

fn merge_blocking(
	repo_root: &Path,
	captured: &CapturedChanges,
	mode: IsolationMergeMode,
	_id: &str,
	commits: IsolationCommitStyle,
	messages: &CommitMessages,
	cancel: &CancellationToken,
) -> (MergeOutcome, bool) {
	if cancel.is_cancelled() {
		return (
			MergeOutcome {
				applied: false,
				had_changes: false,
				summary: "\n\n<system-notification>Isolation merge cancelled before any parent mutation.</system-notification>"
					.to_string(),
				merged_branch_for_nested: false,
			},
			true,
		);
	}
	let outcome = match mode {
		IsolationMergeMode::Patch => merge_patch(repo_root, captured, messages, commits),
		IsolationMergeMode::Branch => merge_branch(repo_root, captured, messages, commits),
	};
	(outcome, false)
}

fn merge_patch(
	repo_root: &Path,
	captured: &CapturedChanges,
	messages: &CommitMessages,
	commits: IsolationCommitStyle,
) -> MergeOutcome {
	let root_empty = captured.root_patch.iter().all(u8::is_ascii_whitespace);
	let nested_empty = captured
		.nested
		.iter()
		.all(|nested| nested.patch.iter().all(u8::is_ascii_whitespace));
	if root_empty && nested_empty {
		return MergeOutcome {
			applied: true,
			had_changes: false,
			summary: "\n\nNo changes to apply.".to_string(),
			merged_branch_for_nested: false,
		};
	}
	let already_applied = !root_empty && patch_applies(repo_root, &captured.root_patch, true);
	let forward_applies = !root_empty && patch_applies(repo_root, &captured.root_patch, false);
	let mut had_changes = false;
	let applied = if root_empty {
		true
	} else if already_applied && !forward_applies {
		true
	} else if forward_applies {
		match apply_patch(repo_root, &captured.root_patch, &[]) {
			Ok(()) => {
				had_changes = true;
				true
			},
			Err(_) => false,
		}
	} else {
		false
	};

	if applied {
		let warnings = apply_nested(repo_root, &captured.nested, messages, commits);
		let mut summary = if root_empty {
			"\n\nNo root changes; nested repository patches applied.".to_string()
		} else if had_changes {
			"\n\nApplied patches: yes".to_string()
		} else {
			"\n\nNo changes to apply.".to_string()
		};
		for warning in warnings {
			summary.push_str(&format!("\n\n<system-notification>{warning}</system-notification>"));
		}
		MergeOutcome {
			applied,
			had_changes: had_changes || !nested_empty,
			summary,
			merged_branch_for_nested: !nested_empty,
		}
	} else {
		let patch_list = captured
			.patch_path
			.as_ref()
			.map_or_else(String::new, |path| {
				format!("\n\nPatch artifact:\n- {}", path.display())
			});
		MergeOutcome {
			applied: false,
			had_changes: false,
			summary: format!(
				"\n\n<system-notification>Patches were not applied and must be handled manually.</system-notification>{patch_list}"
			),
			merged_branch_for_nested: false,
		}
	}
}

fn merge_branch(
	repo_root: &Path,
	captured: &CapturedChanges,
	messages: &CommitMessages,
	commits: IsolationCommitStyle,
) -> MergeOutcome {
	if let Some(error) = &captured.error
		&& captured.branch.is_none()
	{
		let patch_list = captured
			.patch_path
			.as_ref()
			.map_or_else(String::new, |path| {
				format!("\nPatch artifact:\n- {}", path.display())
			});
		return MergeOutcome {
			applied: false,
			had_changes: false,
			summary: format!(
				"\n\n<system-notification>Branch merge failed while capturing the task branch: {error}\nTask outputs are preserved but changes were not applied.{patch_list}</system-notification>"
			),
			merged_branch_for_nested: false,
		};
	}
	let Some(branch) = &captured.branch else {
		if !captured.root_patch.is_empty() {
			let mut outcome = merge_patch(repo_root, captured, messages, commits);
			if let Some(reason) = &captured.branch_fallback_reason {
				outcome.summary = format!(
					"\n\n<system-notification>{reason}</system-notification>{}",
					outcome.summary
				);
			}
			return outcome;
		}
		let nested_only = !captured.nested.is_empty();
		let mut warnings = Vec::new();
		if nested_only {
			warnings = apply_nested(repo_root, &captured.nested, messages, commits);
		}
		let mut summary = if nested_only {
			"\n\nNo root changes to apply; nested repository patches captured.".to_string()
		} else {
			"\n\nNo changes to apply.".to_string()
		};
		for warning in warnings {
			summary.push_str(&format!("\n\n<system-notification>{warning}</system-notification>"));
		}
		return MergeOutcome {
			applied: true,
			had_changes: nested_only,
			summary,
			merged_branch_for_nested: nested_only,
		};
	};
	let base = captured.base_sha.as_deref().unwrap_or_default();
	if base.is_empty() {
		return MergeOutcome {
			applied: false,
			had_changes: false,
			summary: format!(
				"\n\n<system-notification>Branch {branch} has no base commit to cherry-pick from; the branch remains for manual resolution.</system-notification>"
			),
			merged_branch_for_nested: false,
		};
	}
	let range = format!("{base}..{branch}");
	let dirty = git_ok(repo_root, &["status", "--porcelain"])
		.map(|bytes| !bytes.is_empty())
		.unwrap_or(false);
	let stashed = dirty
		&& git_ok(
			repo_root,
			&[
				"stash",
				"push",
				"--include-untracked",
				"-m",
				"omp-task-merge",
			],
		)
		.is_ok();
	if dirty && !stashed {
		return MergeOutcome {
			applied: false,
			had_changes: false,
			summary: format!(
				"\n\n<system-notification>Could not stash the parent worktree; branch {branch} was not merged and remains for manual resolution.</system-notification>"
			),
			merged_branch_for_nested: false,
		};
	}
	if let Err(error) = git_ok(repo_root, &["cherry-pick", range.as_str()]) {
		let _ = git_ok(repo_root, &["cherry-pick", "--abort"]);
		if stashed {
			let _ = git_ok(repo_root, &["stash", "pop", "--index"]);
		}
		return MergeOutcome {
			applied: false,
			had_changes: false,
			summary: format!(
				"\n\n<system-notification>Branch merge failed: {branch}.\nConflict: {error}\nThe unmerged branch remains for manual resolution.</system-notification>"
			),
			merged_branch_for_nested: false,
		};
	}
	let mut warnings = Vec::new();
	if stashed && git_ok(repo_root, &["stash", "pop", "--index"]).is_err() {
		warnings.push(
			"Pre-existing parent changes could not be auto-restored after the cherry-pick; stash entry preserved. Run `git stash pop` and resolve manually."
				.to_string(),
		);
	}
	warnings.extend(apply_nested(repo_root, &captured.nested, messages, commits));
	let _ = git_ok(repo_root, &["branch", "-D", branch]);
	let mut summary = format!("\n\nMerged branch: {branch}");
	for warning in warnings {
		summary.push_str(&format!("\n\n<system-notification>{warning}</system-notification>"));
	}
	MergeOutcome {
		applied: true,
		had_changes: true,
		summary,
		merged_branch_for_nested: true,
	}
}

fn apply_nested(
	repo_root: &Path,
	nested: &[NestedPatchArtifact],
	messages: &CommitMessages,
	commits: IsolationCommitStyle,
) -> Vec<String> {
	let mut warnings = Vec::new();
	for (index, artifact) in nested.iter().enumerate() {
		if artifact.patch.iter().all(u8::is_ascii_whitespace) {
			continue;
		}
		let repo = repo_root.join(&artifact.relative_path);
		if !repo.join(".git").exists() {
			warnings.push(format!(
				"Nested repository `{}` is missing; its patch was not applied.",
				artifact.relative_path
			));
			continue;
		}
		let dirty = git_ok(&repo, &["status", "--porcelain"])
			.map(|bytes| !bytes.is_empty())
			.unwrap_or(false);
		let stash_message = format!(
			"omp isolation pre-merge {}",
			artifact.relative_path
		);
		let stashed = dirty
			&& git_ok(
				&repo,
				&[
					"stash",
					"push",
					"--include-untracked",
					"-m",
					stash_message.as_str(),
				],
			)
			.is_ok();
		if dirty && !stashed {
			warnings.push(format!(
				"Could not stash pre-existing changes in nested repo `{}`; patch not applied.",
				artifact.relative_path
			));
			continue;
		}
		let message = messages
			.nested
			.get(index)
			.and_then(Clone::clone)
			.unwrap_or_else(|| match commits {
				IsolationCommitStyle::Generic => format!(
					"omp: apply isolated changes in {}",
					artifact.relative_path
				),
				IsolationCommitStyle::Ai => String::new(),
			});
		if message.is_empty() {
			if stashed {
				let _ = git_ok(&repo, &["stash", "pop", "--index"]);
			}
			warnings.push(format!(
				"No generated commit message was available for nested repo `{}`; its patch was not applied.",
				artifact.relative_path
			));
			continue;
		}
		let applied = apply_patch(&repo, &artifact.patch, &[]).is_ok();
		if !applied {
			if stashed {
				let _ = git_ok(&repo, &["stash", "pop", "--index"]);
			}
			warnings.push(format!(
				"Patch for nested repo `{}` did not apply; pre-existing state restored.",
				artifact.relative_path
			));
			continue;
		}
		let _ = git_ok(&repo, &["add", "-A"]);
		let committed = git_ok(
			&repo,
			&[
				"-c",
				"commit.gpgsign=false",
				"commit",
				"--no-verify",
				"-m",
				message.as_str(),
			],
		)
		.or_else(|_| {
			git_ok(
					&repo,
					&[
						"-c",
						"user.name=omp",
						"-c",
						"user.email=omp@localhost",
						"-c",
						"commit.gpgsign=false",
						"commit",
						"--no-verify",
						"-m",
						message.as_str(),
					],
				)
			})
			.is_ok();
		if !committed {
			warnings.push(format!(
				"Applied the patch to nested repo `{}` but could not commit it.",
				artifact.relative_path
			));
		}
		if stashed && git_ok(&repo, &["stash", "pop", "--index"]).is_err() {
			warnings.push(format!(
				"Pre-existing dirty state in nested repo `{}` could not be auto-restored after the agent commit; stash entry preserved.",
				artifact.relative_path
			));
		}
	}
	warnings
}

// ─── finalize ───────────────────────────────────────────────────────────────

async fn generate_commit_messages(
	captured: &CapturedChanges,
	provider: &std::sync::Arc<dyn ModelProvider>,
	merge: IsolationMergeMode,
) -> Result<CommitMessages, String> {
	let mut messages = CommitMessages::default();
	// Patch mode never commits the root delta; only nested repositories get
	// commits, so an AI root subject would be a wasted provider call.
	if merge == IsolationMergeMode::Branch
		&& !captured.root_patch.iter().all(u8::is_ascii_whitespace)
	{
		messages.root = Some(generate_one_message(provider, &captured.root_patch).await?);
	}
	for artifact in &captured.nested {
		if artifact.patch.iter().all(u8::is_ascii_whitespace) {
			messages.nested.push(None);
			continue;
		}
		messages
			.nested
			.push(Some(generate_one_message(provider, &artifact.patch).await?));
	}
	Ok(messages)
}

async fn generate_one_message(
	provider: &std::sync::Arc<dyn ModelProvider>,
	diff: &[u8],
) -> Result<String, String> {
	let diff = String::from_utf8_lossy(diff);
	let truncated = diff.chars().take(24_000).collect::<String>();
	let messages = [
		Message::system(
			"Write one git commit subject (max 72 characters) that summarizes this diff. Return only the subject: no quotes, no markdown, no trailing period, no prefix.",
		),
		Message::user(truncated),
	];
	let response = provider
		.complete(&messages, &[], &ToolChoice::Auto)
		.await
		.map_err(|error| format!("commit message generation failed: {error}"))?;
	let subject = response
		.content
		.lines()
		.map(str::trim)
		.find(|line| !line.is_empty())
		.unwrap_or_default()
		.trim_matches('"')
		.trim();
	if subject.is_empty() {
		return Err("commit message generation returned an empty subject".to_string());
	}
	Ok(subject.chars().take(72).collect())
}

/// Capture, optionally merge, and clean up. `succeeded` must be `false` for a
/// failed or aborted run: its sandbox is torn down without capturing changes.
pub async fn finalize(
	sandbox: &IsolationSandbox,
	baseline: &WorktreeBaseline,
	policy: IsolationPolicy,
	id: &str,
	artifacts_dir: PathBuf,
	succeeded: bool,
	cancel: &CancellationToken,
	commit_provider: Option<Arc<dyn ModelProvider>>,
) -> IsolationFinalize {
	if !succeeded || cancel.is_cancelled() {
		let cancelled = cancel.is_cancelled();
		let cleanup_error = sandbox.cleanup();
		return IsolationFinalize {
			report: None,
			summary: String::new(),
			cleanup_error,
			cancelled,
		};
	}
	let inner = Arc::clone(&sandbox.inner);
	let baseline_owned = baseline.clone();
	let id_owned = id.to_string();
	let artifacts = artifacts_dir.clone();
	let merge_mode = policy.merge;
	let captured = tokio::task::spawn_blocking(move || {
		capture_blocking(&inner, &baseline_owned, &id_owned, &artifacts, merge_mode)
	})
	.await;

	let mut finalize = IsolationFinalize::default();
	let mut captured = match captured {
		Ok(Ok(captured)) => captured,
		Ok(Err(error)) => {
			finalize.report = Some(IsolationReport {
				backend: Some(sandbox.handle().backend.as_str().to_string()),
				error: Some(error.clone()),
				..IsolationReport::default()
			});
			finalize.summary = format!(
				"\n\n<system-notification>Isolation capture failed: {error}\nThe sandbox and child work were retained at {} for manual recovery.</system-notification>",
				sandbox.handle().base_dir.display()
			);
			sandbox.retain();
			if let Some(report) = finalize.report.as_mut() {
				report.summary = Some(finalize.summary.trim().to_string());
			}
			return finalize;
		},
		Err(error) => {
			let message = format!("isolation capture task failed: {error}");
			finalize.report = Some(IsolationReport {
				backend: Some(sandbox.handle().backend.as_str().to_string()),
				error: Some(message.clone()),
				..IsolationReport::default()
			});
			finalize.summary = format!(
				"\n\n<system-notification>Isolation capture failed: {message}\nThe sandbox and child work were retained at {} for manual recovery.</system-notification>",
				sandbox.handle().base_dir.display()
			);
			sandbox.retain();
			if let Some(report) = finalize.report.as_mut() {
				report.summary = Some(finalize.summary.trim().to_string());
			}
			return finalize;
		},
	};

	let mut commit_messages = CommitMessages::default();
	if policy.commits == IsolationCommitStyle::Ai
		&& (policy.apply || policy.merge == IsolationMergeMode::Branch)
	{
		let Some(provider) = commit_provider.clone() else {
			finalize.summary = "\n\n<system-notification>task.isolation.commits: ai requires a resolved child model; no commit message was generated and no merge was applied.</system-notification>".to_string();
			finalize.report = Some(IsolationReport {
				patch_path: captured
					.patch_path
					.as_ref()
					.map(|path| path.display().to_string()),
				changes_applied: Some(false),
				backend: Some(sandbox.handle().backend.as_str().to_string()),
				error: Some("missing child model for ai commit messages".to_string()),
				summary: Some(finalize.summary.trim().to_string()),
				..IsolationReport::default()
			});
			sandbox.retain();
			return finalize;
		};
		match generate_commit_messages(&captured, &provider, policy.merge).await {
			Ok(mut messages) => {
				if policy.merge == IsolationMergeMode::Branch && messages.root.is_none() {
					let merged = sandbox.handle().merged_dir.clone();
					let trailing = tokio::task::spawn_blocking(move || {
						let head = git_text(&merged, &["rev-parse", "--verify", "HEAD"])
							.unwrap_or_default();
						if head.is_empty() {
							return Ok::<_, String>(Vec::new());
						}
						let baseline = RepoBaseline {
							repo_root: merged.clone(),
							head_commit: head,
							staged: Vec::new(),
							unstaged: Vec::new(),
							untracked: Vec::new(),
							untracked_patch: Vec::new(),
							submodules: Vec::new(),
						};
						capture_repo_delta(&merged, &baseline)
					})
					.await;
					let trailing_patch = match trailing {
						Ok(Ok(patch)) if !patch.iter().all(u8::is_ascii_whitespace) => Some(patch),
						Ok(Ok(_)) => None,
						Ok(Err(error)) => {
							finalize.summary = format!(
								"\n\n<system-notification>AI commit message generation failed: {error}\nNo merge was applied; the sandbox was retained at {}.</system-notification>",
								sandbox.handle().base_dir.display()
							);
							finalize.report = Some(IsolationReport {
								patch_path: captured
									.patch_path
									.as_ref()
									.map(|path| path.display().to_string()),
								changes_applied: Some(false),
								backend: Some(sandbox.handle().backend.as_str().to_string()),
								error: Some(error),
								summary: Some(finalize.summary.trim().to_string()),
								..IsolationReport::default()
							});
							sandbox.retain();
							return finalize;
						},
						Err(error) => {
							let error = format!("trailing diff task failed: {error}");
							finalize.summary = format!(
								"\n\n<system-notification>AI commit message generation failed: {error}\nNo merge was applied; the sandbox was retained at {}.</system-notification>",
								sandbox.handle().base_dir.display()
							);
							finalize.report = Some(IsolationReport {
								patch_path: captured
									.patch_path
									.as_ref()
									.map(|path| path.display().to_string()),
								changes_applied: Some(false),
								backend: Some(sandbox.handle().backend.as_str().to_string()),
								error: Some(error),
								summary: Some(finalize.summary.trim().to_string()),
								..IsolationReport::default()
							});
							sandbox.retain();
							return finalize;
						},
					};
					if let Some(patch) = trailing_patch {
						match generate_one_message(&provider, &patch).await {
							Ok(message) => messages.root = Some(message),
							Err(error) => {
								finalize.summary = format!(
									"\n\n<system-notification>AI commit message generation failed: {error}\nNo merge was applied; the sandbox was retained at {}.</system-notification>",
									sandbox.handle().base_dir.display()
								);
								finalize.report = Some(IsolationReport {
									patch_path: captured
										.patch_path
										.as_ref()
										.map(|path| path.display().to_string()),
									changes_applied: Some(false),
									backend: Some(sandbox.handle().backend.as_str().to_string()),
									error: Some(error),
									summary: Some(finalize.summary.trim().to_string()),
									..IsolationReport::default()
								});
								sandbox.retain();
								return finalize;
							},
						}
					}
				}
				commit_messages = messages;
			},
			Err(error) => {
				finalize.summary = format!(
					"\n\n<system-notification>AI commit message generation failed: {error}\nNo merge was applied; the sandbox and child work were retained at {}.</system-notification>",
					sandbox.handle().base_dir.display()
				);
				finalize.report = Some(IsolationReport {
					patch_path: captured
						.patch_path
						.as_ref()
						.map(|path| path.display().to_string()),
					changes_applied: Some(false),
					backend: Some(sandbox.handle().backend.as_str().to_string()),
					error: Some(error),
					summary: Some(finalize.summary.trim().to_string()),
					..IsolationReport::default()
				});
				sandbox.retain();
				return finalize;
			},
		}
	}

	if policy.merge == IsolationMergeMode::Branch {
		let merged = sandbox.handle().merged_dir.clone();
		let baseline_root = baseline.root.clone();
		let id_owned = id.to_string();
		let root_message = commit_messages.root.clone();
		let branch = tokio::task::spawn_blocking(move || {
			prepare_branch_capture(&merged, &baseline_root, &id_owned, root_message.as_deref())
		})
		.await;
		match branch {
			Ok(Ok(Some((branch, base_sha)))) => {
				captured.branch = Some(branch);
				captured.base_sha = Some(base_sha);
			},
			Ok(Ok(None)) => {
				if !baseline.root.is_clean()
					&& !captured.root_patch.iter().all(u8::is_ascii_whitespace)
				{
					captured.branch_fallback_reason = Some(
						"No agent commits needed preservation and the parent worktree was dirty; applied the captured child-delta patch instead of creating a branch."
							.to_string(),
					);
				}
			},
			Ok(Err(error)) => captured.error = Some(error),
			Err(error) => captured.error = Some(format!("branch capture task failed: {error}")),
		}
	}

	let mut report = IsolationReport {
		patch_path: captured
			.patch_path
			.as_ref()
			.map(|path| path.display().to_string()),
		branch_name: captured.branch.clone(),
		base_sha: captured.base_sha.clone(),
		nested_patches: captured
			.nested
			.iter()
			.map(|nested| NestedRepoPatch {
				relative_path: nested.relative_path.clone(),
				patch: String::from_utf8_lossy(&nested.patch).into_owned(),
			})
			.collect(),
		changes_applied: None,
		backend: Some(sandbox.handle().backend.as_str().to_string()),
		error: captured.error.clone(),
		summary: None,
	};
	if cancel.is_cancelled() {
		let artifact = report
			.patch_path
			.clone()
			.map(|path| format!("patch artifact {path}"))
			.or_else(|| {
				report
					.branch_name
					.clone()
					.map(|branch| format!("task branch {branch}"))
			})
			.unwrap_or_else(|| format!("sandbox {}", sandbox.handle().base_dir.display()));
		finalize.summary = format!(
			"\n\n<system-notification>Isolation merge cancelled; captured artifacts were kept at {artifact}.</system-notification>"
		);
		finalize.cancelled = true;
		finalize.report = Some(report);
		finalize.cleanup_error = sandbox.cleanup();
		return finalize;
	}

	let mut keep_sandbox = false;
	if !policy.apply {
		let mut summary = String::from("\n\nIsolated changes retained; merge skipped (apply=false).");
		if let Some(path) = &report.patch_path {
			summary.push_str(&format!("\nPatch artifact:\n- {path}"));
		}
		if let Some(branch) = &report.branch_name {
			summary.push_str(&format!(
				"\nTask branch: {branch} (base {})",
				report.base_sha.as_deref().unwrap_or("")
			));
		}
		for nested in &captured.nested {
			if let Some(path) = &nested.patch_path {
				summary.push_str(&format!(
					"\nNested patch ({}):\n- {}",
					nested.relative_path,
					path.display()
				));
			}
		}
		finalize.summary = summary;
	} else if let Some(error) = &captured.error
		&& captured.branch.is_none()
		&& policy.merge == IsolationMergeMode::Branch
	{
		finalize.summary = format!(
			"\n\n<system-notification>Branch merge failed while capturing the task branch: {error}\nTask outputs and the full sandbox were retained at {} for manual resolution.</system-notification>",
			sandbox.handle().base_dir.display()
		);
		report.changes_applied = Some(false);
		keep_sandbox = true;
	} else {
		let repo_root = baseline.root.repo_root.clone();
		let captured_for_merge = captured.clone();
		let id_owned = id.to_string();
		let commits = policy.commits;
		let messages = commit_messages.clone();
		let cancel_for_merge = cancel.clone();
		let outcome = tokio::task::spawn_blocking(move || {
			// The guard lives inside the blocking worker: a cancelled async
			// waiter cannot release it while the git mutation is still running.
			let _merge = MERGE_LOCK
				.lock()
				.unwrap_or_else(|poison| poison.into_inner());
			merge_blocking(
				&repo_root,
				&captured_for_merge,
				merge_mode,
				&id_owned,
				commits,
				&messages,
				&cancel_for_merge,
			)
		})
		.await;
		match outcome {
			Ok((outcome, cancelled_before_merge)) => {
				finalize.summary = outcome.summary;
				if cancelled_before_merge {
					finalize.cancelled = true;
					report.changes_applied = None;
				} else {
					report.changes_applied = Some(outcome.applied);
				}
			},
			Err(error) => {
				report.changes_applied = Some(false);
				finalize.summary = format!(
					"\n\n<system-notification>Merge phase failed: {error}\nTask outputs are preserved but changes were not applied.</system-notification>"
				);
			},
		}
	}
	if cancel.is_cancelled() {
		finalize.cancelled = true;
		finalize
			.summary
			.push_str("\n\n<system-notification>Isolation finalize cancelled.</system-notification>");
	}

	finalize.report = Some(report);
	if keep_sandbox {
		sandbox.retain();
	} else {
		finalize.cleanup_error = sandbox.cleanup();
	}
	if let Some(error) = &finalize.cleanup_error {
		finalize.summary.push_str(&format!(
			"\n\n<system-notification>Isolation cleanup failed: {error}</system-notification>"
		));
	}
	if let Some(report) = finalize.report.as_mut()
		&& !finalize.summary.is_empty()
	{
		report.summary = Some(finalize.summary.trim().to_string());
	}
	finalize
}

#[cfg(test)]
mod tests {
	use std::process::Command;

	use super::*;

	struct TempDir(PathBuf);

	impl TempDir {
		fn new(label: &str) -> Self {
			let path = std::env::temp_dir().join(format!(
				"omp-iso-test-{label}-{}-{}",
				std::process::id(),
				now_nanos()
			));
			fs::create_dir_all(&path).expect("temp dir");
			Self(path)
		}
	}

	impl Drop for TempDir {
		fn drop(&mut self) {
			let _ = fs::remove_dir_all(&self.0);
		}
	}

	fn git(dir: &Path, args: &[&str]) -> String {
		let output = Command::new("git")
			.arg("-C")
			.arg(dir)
			.args(args)
			.output()
			.expect("git");
		assert!(
			output.status.success(),
			"git {args:?}: {}",
			String::from_utf8_lossy(&output.stderr)
		);
		String::from_utf8_lossy(&output.stdout).trim().to_string()
	}

	fn repo(dir: &Path) {
		git(dir, &["init", "-q"]);
		git(dir, &["config", "user.email", "test@example.com"]);
		git(dir, &["config", "user.name", "test"]);
		fs::write(dir.join("tracked.txt"), "base\n").expect("write");
		git(dir, &["add", "tracked.txt"]);
		git(dir, &["commit", "-qm", "base"]);
	}

	fn capture_branch_for_test(
		sandbox: &IsolationSandbox,
		baseline: &WorktreeBaseline,
		id: &str,
		artifacts: &Path,
	) -> CapturedChanges {
		let mut captured = capture_blocking(
			&sandbox.inner,
			baseline,
			id,
			artifacts,
			IsolationMergeMode::Branch,
		)
		.expect("capture");
		let branch = prepare_branch_capture(sandbox.merged_dir(), &baseline.root, id, None)
			.expect("branch");
		if let Some((branch, base_sha)) = branch {
			captured.branch = Some(branch);
			captured.base_sha = Some(base_sha);
		}
		captured
	}

	#[test]
	fn baseline_delta_excludes_dirty_parent_and_keeps_binary() {
		let temp = TempDir::new("delta");
		let parent = temp.0.join("repo");
		fs::create_dir_all(&parent).expect("repo dir");
		repo(&parent);
		fs::write(parent.join("tracked.txt"), "base\ndirty\n").expect("dirty");
		fs::write(parent.join("untracked.txt"), "parent wip\n").expect("wip");

		let baseline = prepare_context_blocking(&parent).expect("baseline");
		let base = temp.0.join("wt");
		fs::create_dir_all(&base).expect("wt");
		// Force the always-available backend so this test needs no mount privileges.
		let policy = IsolationPolicy {
			merge: IsolationMergeMode::Patch,
			apply: true,
			commits: IsolationCommitStyle::Generic,
			preferred: Some(BackendKind::Rcopy),
		};
		let handle =
			ensure_blocking(&parent, "task-1", policy, &base).expect("sandbox");
		let sandbox = IsolationSandbox::new(handle);

		fs::write(sandbox.merged_dir().join("tracked.txt"), "base\ndirty\nagent\n")
			.expect("agent edit");
		fs::write(sandbox.merged_dir().join("agent.bin"), [0, 1, 2, 3]).expect("binary");
		let artifacts = temp.0.join("artifacts");
		let captured = capture_blocking(
			&sandbox.inner,
			&baseline,
			"task-1",
			&artifacts,
			IsolationMergeMode::Patch,
		)
		.expect("capture");

		let patch = String::from_utf8_lossy(&captured.root_patch);
		assert!(patch.contains("+agent"), "{patch}");
		assert!(patch.contains("agent.bin"), "{patch}");
		assert!(!patch.contains("+parent wip"), "{patch}");
		assert!(
			fs::read(parent.join("tracked.txt")).expect("parent") == b"base\ndirty\n",
			"parent must stay unchanged before merge"
		);

		let outcome = merge_patch(&parent, &captured, &CommitMessages::default(), IsolationCommitStyle::Generic);
		assert!(outcome.applied);
		assert_eq!(
			fs::read_to_string(parent.join("tracked.txt")).expect("merged"),
			"base\ndirty\nagent\n"
		);
		assert_eq!(fs::read(parent.join("agent.bin")).expect("bin"), [0, 1, 2, 3]);
		assert!(sandbox.cleanup().is_none());
		assert!(!sandbox.handle().base_dir.exists());
	}

	#[test]
	fn cancel_cleanup_removes_owned_sandbox_only() {
		let temp = TempDir::new("cancel");
		let parent = temp.0.join("repo");
		fs::create_dir_all(&parent).expect("repo dir");
		repo(&parent);
		let baseline = prepare_context_blocking(&parent).expect("baseline");
		let base = temp.0.join("wt");
		fs::create_dir_all(&base).expect("wt");
		let policy = IsolationPolicy {
			merge: IsolationMergeMode::Patch,
			apply: true,
			commits: IsolationCommitStyle::Generic,
			preferred: Some(BackendKind::Rcopy),
		};
		let handle =
			ensure_blocking(&parent, "task-2", policy, &base).expect("sandbox");
		let sandbox = IsolationSandbox::new(handle);
		let owned = sandbox.handle().base_dir.clone();
		drop(sandbox);
		assert!(!owned.exists(), "sandbox base removed");
		assert!(parent.join("tracked.txt").exists(), "source checkout intact");
		assert!(!baseline.root.head_commit.is_empty());
	}

	#[test]
	fn branch_mode_preserves_agent_commits_and_applies_them() {
		let temp = TempDir::new("branch");
		let parent = temp.0.join("repo");
		fs::create_dir_all(&parent).expect("repo dir");
		repo(&parent);
		let baseline = prepare_context_blocking(&parent).expect("baseline");
		let base = temp.0.join("wt");
		fs::create_dir_all(&base).expect("wt");
		let policy = IsolationPolicy {
			merge: IsolationMergeMode::Branch,
			apply: true,
			commits: IsolationCommitStyle::Generic,
			preferred: Some(BackendKind::Rcopy),
		};
		let sandbox = IsolationSandbox::new(
			ensure_blocking(&parent, "task-4", policy, &base).expect("sandbox"),
		);
		fs::write(sandbox.merged_dir().join("tracked.txt"), "base\ncommitted\n")
			.expect("agent");
		git(sandbox.merged_dir(), &["add", "-A"]);
		git(sandbox.merged_dir(), &["commit", "-qm", "agent commit"]);
		// Uncommitted B on top of committed A must become a trailing branch
		// commit, not be lost by the cherry-pick.
		fs::write(sandbox.merged_dir().join("tracked.txt"), "base\ncommitted\nleftover\n")
			.expect("leftover");
		let captured = capture_branch_for_test(
			&sandbox,
			&baseline,
			"task-4",
			&temp.0.join("artifacts"),
		);
		let branch = captured.branch.as_deref().expect("branch");
		assert_eq!(
			git(&parent, &["rev-parse", branch]),
			git(sandbox.merged_dir(), &["rev-parse", "HEAD"])
		);
		let outcome = merge_branch(&parent, &captured, &CommitMessages::default(), IsolationCommitStyle::Generic);
		assert!(outcome.applied, "{}", outcome.summary);
		assert_eq!(
			fs::read_to_string(parent.join("tracked.txt")).expect("merged"),
			"base\ncommitted\nleftover\n"
		);
		let branch_exists = Command::new("git")
			.arg("-C")
			.arg(&parent)
			.args(["rev-parse", "--verify", "--quiet", branch])
			.output()
			.expect("git");
		assert!(
			!branch_exists.status.success(),
			"task branch is deleted after a successful cherry-pick"
		);
		assert!(sandbox.cleanup().is_none());
	}

	#[test]
	fn merge_conflict_keeps_patch_and_parent_intact() {
		let temp = TempDir::new("conflict");
		let parent = temp.0.join("repo");
		fs::create_dir_all(&parent).expect("repo dir");
		repo(&parent);
		let baseline = prepare_context_blocking(&parent).expect("baseline");
		let base = temp.0.join("wt");
		fs::create_dir_all(&base).expect("wt");
		let policy = IsolationPolicy {
			merge: IsolationMergeMode::Patch,
			apply: true,
			commits: IsolationCommitStyle::Generic,
			preferred: Some(BackendKind::Rcopy),
		};
		let sandbox = IsolationSandbox::new(
			ensure_blocking(&parent, "task-3", policy, &base).expect("sandbox"),
		);
		fs::write(sandbox.merged_dir().join("tracked.txt"), "base\nagent\n").expect("agent");
		let captured = capture_blocking(
			&sandbox.inner,
			&baseline,
			"task-3",
			&temp.0.join("artifacts"),
			IsolationMergeMode::Patch,
		)
		.expect("capture");
		// Parent drifts after capture; the patch must not overwrite it.
		fs::write(parent.join("tracked.txt"), "parent conflict\n").expect("parent drift");
		let outcome = merge_patch(&parent, &captured, &CommitMessages::default(), IsolationCommitStyle::Generic);
		assert!(!outcome.applied);
		assert_eq!(
			fs::read_to_string(parent.join("tracked.txt")).expect("parent"),
			"parent conflict\n"
		);
		assert!(
			captured
				.patch_path
				.as_ref()
				.is_some_and(|path| path.exists()),
			"patch artifact retained"
		);
		assert!(sandbox.cleanup().is_none());
	}

	#[test]
	fn nested_repo_metadata_is_detached_and_parent_untouched() {
		let temp = TempDir::new("nested");
		let parent = temp.0.join("repo");
		fs::create_dir_all(&parent).expect("repo dir");
		repo(&parent);
		let nested = parent.join("nested");
		fs::create_dir_all(&nested).expect("nested dir");
		repo(&nested);
		fs::write(nested.join("inner.txt"), "inner\n").expect("inner");
		git(&nested, &["add", "inner.txt"]);
		git(&nested, &["commit", "-qm", "inner"]);
		let parent_head = git(&nested, &["rev-parse", "HEAD"]);
		let baseline = prepare_context_blocking(&parent).expect("baseline");
		assert_eq!(baseline.nested.len(), 1);

		let base = temp.0.join("wt");
		fs::create_dir_all(&base).expect("wt");
		let policy = IsolationPolicy {
			merge: IsolationMergeMode::Patch,
			apply: false,
			commits: IsolationCommitStyle::Generic,
			preferred: Some(BackendKind::Rcopy),
		};
		let sandbox = IsolationSandbox::new(
			ensure_blocking(&parent, "task-5", policy, &base).expect("sandbox"),
		);
		prepare_sandbox_blocking(&sandbox.inner, &baseline).expect("prepare");
		let child_nested = sandbox.merged_dir().join("nested");
		fs::write(child_nested.join("inner.txt"), "inner\nchild\n").expect("child");
		git(&child_nested, &["add", "inner.txt"]);
		git(&child_nested, &["commit", "-qm", "child commit"]);

		assert_eq!(
			git(&nested, &["rev-parse", "HEAD"]),
			parent_head,
			"parent nested HEAD must not move"
		);
		assert!(
			git(&nested, &["status", "--porcelain"]).is_empty(),
			"parent nested index must stay clean"
		);
		assert!(sandbox.cleanup().is_none());
	}

	#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
	async fn merge_lock_survives_dropped_async_waiter() {
		let (acquired_tx, mut acquired_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
		let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
		let held = tokio::task::spawn_blocking(move || {
			let _guard = MERGE_LOCK
				.lock()
				.unwrap_or_else(|poison| poison.into_inner());
			let _ = acquired_tx.send(());
			let _ = release_rx.recv();
		});
		tokio::time::timeout(std::time::Duration::from_secs(5), acquired_rx.recv())
			.await
			.expect("worker acquired lock")
			.expect("signal");
		// Dropping the async waiter (simulating a cancelled finalize) must not
		// release the blocking lock while the mutation still runs.
		drop(held);

		let (second_tx, mut second_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
		let second = tokio::task::spawn_blocking(move || {
			let _guard = MERGE_LOCK
				.lock()
				.unwrap_or_else(|poison| poison.into_inner());
			let _ = second_tx.send(());
		});
		assert!(
			tokio::time::timeout(std::time::Duration::from_millis(150), second_rx.recv())
				.await
				.is_err(),
			"second merge entered while the first worker still held the lock"
		);
		let _ = release_tx.send(());
		tokio::time::timeout(std::time::Duration::from_secs(5), second_rx.recv())
			.await
			.expect("second merge after release")
			.expect("signal");
		second.await.expect("second worker");
	}

	#[test]
	fn branch_committed_then_uncommitted_revert_keeps_net_zero() {
		let temp = TempDir::new("revert");
		let parent = temp.0.join("repo");
		fs::create_dir_all(&parent).expect("repo dir");
		repo(&parent);
		let baseline = prepare_context_blocking(&parent).expect("baseline");
		let base = temp.0.join("wt");
		fs::create_dir_all(&base).expect("wt");
		let policy = IsolationPolicy {
			merge: IsolationMergeMode::Branch,
			apply: true,
			commits: IsolationCommitStyle::Generic,
			preferred: Some(BackendKind::Rcopy),
		};
		let sandbox = IsolationSandbox::new(
			ensure_blocking(&parent, "task-6", policy, &base).expect("sandbox"),
		);
		fs::write(sandbox.merged_dir().join("tracked.txt"), "base\nagent\n").expect("agent");
		git(sandbox.merged_dir(), &["add", "-A"]);
		git(sandbox.merged_dir(), &["commit", "-qm", "agent commit"]);
		fs::write(sandbox.merged_dir().join("tracked.txt"), "base\n").expect("revert");
		let captured = capture_branch_for_test(
			&sandbox,
			&baseline,
			"task-6",
			&temp.0.join("artifacts"),
		);
		assert!(
			captured.root_patch.iter().all(u8::is_ascii_whitespace),
			"net delta is empty"
		);
		assert!(captured.branch.is_some(), "committed history preserved");
		let outcome = merge_branch(&parent, &captured, &CommitMessages::default(), IsolationCommitStyle::Generic);
		assert!(outcome.applied, "{}", outcome.summary);
		assert_eq!(
			fs::read_to_string(parent.join("tracked.txt")).expect("parent"),
			"base\n"
		);
		assert!(sandbox.cleanup().is_none());
	}

	#[test]
	fn cancelled_merge_never_touches_parent() {
		let temp = TempDir::new("cancelmerge");
		let parent = temp.0.join("repo");
		fs::create_dir_all(&parent).expect("repo dir");
		repo(&parent);
		let baseline = prepare_context_blocking(&parent).expect("baseline");
		let base = temp.0.join("wt");
		fs::create_dir_all(&base).expect("wt");
		let policy = IsolationPolicy {
			merge: IsolationMergeMode::Patch,
			apply: true,
			commits: IsolationCommitStyle::Generic,
			preferred: Some(BackendKind::Rcopy),
		};
		let sandbox = IsolationSandbox::new(
			ensure_blocking(&parent, "task-7", policy, &base).expect("sandbox"),
		);
		fs::write(sandbox.merged_dir().join("tracked.txt"), "base\nagent\n").expect("agent");
		let captured = capture_blocking(
			&sandbox.inner,
			&baseline,
			"task-7",
			&temp.0.join("artifacts"),
			IsolationMergeMode::Patch,
		)
		.expect("capture");
		let cancel = CancellationToken::new();
		cancel.cancel();
		let (outcome, cancelled) =
			merge_blocking(&parent, &captured, IsolationMergeMode::Patch, "task-7", IsolationCommitStyle::Generic, &CommitMessages::default(), &cancel);
		assert!(cancelled);
		assert!(!outcome.applied);
		assert_eq!(
			fs::read_to_string(parent.join("tracked.txt")).expect("parent"),
			"base\n"
		);
		assert!(sandbox.cleanup().is_none());
	}

	#[test]
	fn submodule_metadata_is_private_and_parent_untouched() {
		let temp = TempDir::new("submodule");
		let source = temp.0.join("source");
		fs::create_dir_all(&source).expect("source dir");
		repo(&source);
		let parent = temp.0.join("repo");
		fs::create_dir_all(&parent).expect("repo dir");
		repo(&parent);
		let sub_url = source.to_string_lossy().into_owned();
		git(
			&parent,
			&[
				"-c",
				"protocol.file.allow=always",
				"submodule",
				"add",
				sub_url.as_str(),
				"sub",
			],
		);
		git(&parent, &["commit", "-qm", "add submodule"]);
		let parent_sub_head = git(&parent.join("sub"), &["rev-parse", "HEAD"]);
		let baseline = prepare_context_blocking(&parent).expect("baseline");
		assert_eq!(baseline.root.submodules, vec!["sub".to_string()]);

		let base = temp.0.join("wt");
		fs::create_dir_all(&base).expect("wt");
		let policy = IsolationPolicy {
			merge: IsolationMergeMode::Patch,
			apply: false,
			commits: IsolationCommitStyle::Generic,
			preferred: Some(BackendKind::Rcopy),
		};
		let sandbox = IsolationSandbox::new(
			ensure_blocking(&parent, "task-8", policy, &base).expect("sandbox"),
		);
		prepare_sandbox_blocking(&sandbox.inner, &baseline).expect("prepare");
		let child_sub = sandbox.merged_dir().join("sub");
		fs::write(child_sub.join("child.txt"), "child\n").expect("child");
		git(&child_sub, &["add", "-A"]);
		git(
			&child_sub,
			&[
				"-c",
				"user.name=test",
				"-c",
				"user.email=test@example.com",
				"commit",
				"-qm",
				"child submodule",
			],
		);
		assert_eq!(
			git(&parent.join("sub"), &["rev-parse", "HEAD"]),
			parent_sub_head,
			"parent submodule HEAD must not move"
		);
		assert!(
			git(&parent.join("sub"), &["status", "--porcelain"]).is_empty(),
			"parent submodule index must stay clean"
		);
		assert!(sandbox.cleanup().is_none());
	}

	#[test]
	fn policy_none_rejects_requested_isolation() {
		let settings = TaskIsolationSettings::default();
		let request = IsolationRequest {
			isolated: Some(true),
			..IsolationRequest::default()
		};
		assert!(resolve_policy(&settings, &request, true).is_err());
		let request = IsolationRequest::default();
		assert_eq!(resolve_policy(&settings, &request, true).expect("none"), None);
	}

	#[test]
	fn invalid_mode_values_are_rejected() {
		assert!(IsolationMode::parse("bogus").is_err());
		assert_eq!(IsolationMode::parse("worktree").expect("legacy"), IsolationMode::Rcopy);
		assert!(IsolationMergeMode::parse("squash").is_err());
		assert!(IsolationCommitStyle::parse("ai").is_ok());
	}
}
