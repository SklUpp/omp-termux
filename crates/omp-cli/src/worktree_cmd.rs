//! `omp worktree` / `omp wt` — list and reclaim agent-managed worktree
//! directories.
//!
//! Upstream anchor: `packages/coding-agent/src/cli/worktree-cli.ts`.
//!
//! Layout under the base directory (default: the XDG-aware data root's `wt`,
//! i.e. `dirs::worktrees_root()`; override `OMP_WORKTREE_DIR` or the
//! `worktree.base` setting):
//!
//! - **PR-checkout worktrees** (`tools/gh.ts`): a checkout whose `.git` is a
//!   *file* pointing at `<parent-repo>/.git/worktrees/<name>/`.
//! - **Task-isolation dirs** (`task/worktree.ts`): a wrapper dir with a compact
//!   `m` subdir (legacy `merged`), an ownership marker naming the live omp
//!   process, or both.
//! - **Legacy nesting**: `<base>/<encoded-project>/<branch-or-id>`.
//!
//! # Safety divergence from upstream
//!
//! Upstream `clear` deletes entries with `fs.rm` and `git worktree remove
//! --force`. This build never deletes user data:
//!
//! - An orphaned git checkout or a plain leftover is **moved** into
//!   `<base>/.trash-<YYYY-MM-DD>/<name>` (collision suffix `-2`, `-3`, …), which
//!   keeps every file the user had, including dirty and untracked ones. The
//!   destination is claimed atomically with `create_dir`, so a concurrent run
//!   or a foreign directory of the same name can only win the name — it is
//!   never replaced or removed, and only this run's own empty reservation is
//!   ever the target of the rename.
//! - A *registered* git checkout is relocated with the same metadata rewrite
//!   `git worktree move` performs: one same-filesystem rename plus the two
//!   pointer files (the checkout's `.git` and the parent's
//!   `.git/worktrees/<name>/gitdir`), so the moved tree stays a usable
//!   registered worktree and dirty/untracked files travel with it. Locked
//!   worktrees, submodule checkouts, and a checkout that contains the current
//!   working directory are refused with the exact path instead. A failed
//!   rewrite restores both pointer files byte-for-byte and rolls the rename
//!   back, claiming the old slot atomically so a directory a concurrent process
//!   created there is never replaced; when the move back is impossible the
//!   error names the real location, the restore state, and that the parent
//!   registration is stale and prunable.
//! - A task-isolation sandbox whose owner process is still alive is refused in
//!   every mode, including `--all`, until that task finishes. Upstream's
//!   `--all` silently deletes a live subagent's worktree; that is exactly the
//!   data loss this command must not reproduce.
//! - Parent-side git bookkeeping is never pruned here (no `worktree prune`,
//!   no admin `rm -rf`, no `--force`, no `unlock`). A checkout whose
//!   registration is still valid is relocated with its metadata rewritten; one
//!   whose administration directory is already gone is quarantined as plain
//!   files, so nothing is left dangling; a registration that disagrees with the
//!   checkout is refused instead of being guessed at.
//!
//! Every path is re-validated before a move: entries must be non-symlink
//! directories whose canonical path is inside the canonical base, `.trash-*`
//! entries are never rescanned, and a `.git` file must point at a `worktrees/`
//! administration directory whose `gitdir` back-pointer names this checkout.
//! Anything else is refused, not guessed at.

use std::{
	fs,
	io::{self, Write},
	path::{Component, Path, PathBuf},
	time::{SystemTime, UNIX_EPOCH},
};

/// Ownership marker written by task isolation before the mount is materialised
/// (`packages/coding-agent/src/task/isolation-ownership.ts`).
pub const ISOLATION_OWNER_FILE: &str = ".omp-isolation-owner.json";

/// Prefix of quarantine directories; never rescanned as worktree entries.
const TRASH_PREFIX: &str = ".trash-";

/// Mount dir names that identify a task-isolation sandbox. `merged` is legacy.
const ISOLATION_MOUNT_DIRS: [&str; 2] = ["m", "merged"];

/// Orphan reason when `.git` names a `worktrees/<name>` dir that no longer
/// exists; nothing is registered, so `clear` may quarantine without git.
const ORPHAN_ADMIN_MISSING: &str = "parent repo no longer tracks this worktree";

/// Upper bound on a `.git` pointer file we are willing to parse.
const DOT_GIT_MAX_BYTES: u64 = 8 * 1024;

/// What the scanner found on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeKind {
	/// Linked git checkout (`.git` file).
	PrCheckout,
	/// Task-isolation sandbox (owner marker and/or `m`/`merged` mount dir).
	TaskIsolation,
	/// Legacy project shell with no recognizable contents.
	Empty,
	/// Anything else directly under the base (or a legacy project dir).
	Stray,
}

impl WorktreeKind {
	/// Stable machine-readable name (`pr-checkout`, `task-isolation`, …).
	#[must_use]
	pub const fn as_str(self) -> &'static str {
		match self {
			Self::PrCheckout => "pr-checkout",
			Self::TaskIsolation => "task-isolation",
			Self::Empty => "empty",
			Self::Stray => "stray",
		}
	}
}

/// One directory discovered under the worktree base.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeEntry {
	/// Absolute path under the base.
	pub path: PathBuf,
	/// Classification.
	pub kind: WorktreeKind,
	/// Parent repository root, when derived from a well-formed pointer.
	pub parent_repo: Option<PathBuf>,
	/// Branch from the parent's `worktrees/<name>/HEAD`, when available.
	pub branch: Option<String>,
	/// Set when the entry is unhealthy; default `clear` targets these.
	pub orphan_reason: Option<String>,
	/// Set when the entry is unsafe to move (symlinked `.git`, escape) even
	/// when it looks orphaned. `clear` refuses these with this explanation.
	pub blocked: Option<String>,
	/// Whether a live process still owns a task-isolation sandbox.
	pub live_owner: bool,
	/// Whether the parent git registration points back at this checkout.
	pub registered: bool,
	/// `<parent>/.git/worktrees/<name>` administration dir named by `.git`,
	/// when the pointer parsed. Populated even when that dir is already gone.
	pub admin_dir: Option<PathBuf>,
}

/// `clear` flags, mirroring upstream `ClearWorktreesOptions`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClearOptions {
	/// Target live entries too. Isolation sandboxes with a live owner are still
	/// refused; registered git checkouts are relocated with a metadata-preserving
	/// move.
	pub all: bool,
	/// Print the plan without touching the filesystem.
	pub dry_run: bool,
	/// Machine-readable output.
	pub json: bool,
}

/// What happened to one target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClearAction {
	/// Moved into the base's `.trash-<date>/`.
	Quarantined,
	/// Relocated with the git metadata rewrite; registration stays valid.
	Moved,
	/// Intentionally left in place (live owner, unsafe pointer).
	Refused,
	/// The move was attempted and failed; `error` states where the entry is now
	/// and which pointer files, if any, were restored.
	Failed,
}

impl ClearAction {
	/// Stable machine-readable name.
	#[must_use]
	pub const fn as_str(self) -> &'static str {
		match self {
			Self::Quarantined => "quarantined",
			Self::Moved => "moved",
			Self::Refused => "refused",
			Self::Failed => "failed",
		}
	}

	const fn is_success(self) -> bool {
		matches!(self, Self::Quarantined | Self::Moved)
	}
}

/// Per-entry result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClearOutcome {
	pub path: PathBuf,
	pub action: ClearAction,
	/// Quarantine destination, when the entry was moved.
	pub destination: Option<PathBuf>,
	/// Failure or refusal explanation.
	pub error: Option<String>,
	/// Honest side notes (parent registration left untouched, relocation
	/// caveats).
	pub notes: Vec<String>,
}

/// Summary returned by [`clear_worktrees`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClearReport {
	pub outcomes: Vec<ClearOutcome>,
	/// Entries seen by the scan (targets or not).
	pub total: usize,
}

impl ClearReport {
	#[must_use]
	pub fn count(&self, action: ClearAction) -> usize {
		self.outcomes.iter().filter(|o| o.action == action).count()
	}

	/// Successful relocations, both quarantined and git-moved.
	#[must_use]
	pub fn relocated(&self) -> usize {
		self
			.outcomes
			.iter()
			.filter(|o| o.action.is_success())
			.count()
	}

	#[must_use]
	pub fn failed(&self) -> usize {
		self.count(ClearAction::Failed)
	}

	#[must_use]
	pub fn refused(&self) -> usize {
		self.count(ClearAction::Refused)
	}

	/// Wiring contract: exit 1 when any target was refused or a move failed, so
	/// a partial cleanup is never reported as success. A run with nothing to do
	/// is 0.
	#[must_use]
	pub fn exit_failure(&self) -> bool {
		self.failed() > 0 || self.refused() > 0
	}
}

// ───────────────────────────────────────────────────────────────────────────
// Base resolution
// ───────────────────────────────────────────────────────────────────────────

/// Resolve the worktree base in the `getWorktreesDir` order:
/// `OMP_WORKTREE_DIR`, then the explicit `worktree.base` value, then the
/// XDG-aware data root (`dirs::worktrees_root()`).
///
/// A relative environment value is ignored and resolution falls through, as
/// upstream does. A relative explicit value is an error: it came from the
/// caller rather than the ambient environment, and a base bound to the launch
/// cwd would let creation and cleanup disagree.
pub fn resolve_base(explicit: Option<&Path>) -> Result<PathBuf, String> {
	if let Some(raw) = std::env::var_os("OMP_WORKTREE_DIR")
		&& let Some(base) = validate_base(&raw.to_string_lossy())
	{
		return Ok(base);
	}
	if let Some(path) = explicit {
		return validate_base(&path.to_string_lossy()).ok_or_else(|| {
			format!(
				"invalid worktree base '{}': expected an absolute path or `~/...`",
				path.display()
			)
		});
	}
	Ok(crate::dirs::worktrees_root())
}

fn validate_base(raw: &str) -> Option<PathBuf> {
	let trimmed = raw.trim();
	if trimmed.is_empty() {
		return None;
	}
	let expanded = if trimmed == "~" {
		crate::dirs::home_dir()?
	} else if let Some(rest) = trimmed.strip_prefix("~/") {
		crate::dirs::home_dir()?.join(rest)
	} else {
		PathBuf::from(trimmed)
	};
	expanded
		.is_absolute()
		.then(|| normalize_lexical(&expanded))
}

// ───────────────────────────────────────────────────────────────────────────
// Scanner
// ───────────────────────────────────────────────────────────────────────────

/// Scan the worktree base and classify every direct (or legacy-nested) entry.
///
/// A missing base is an empty result, matching upstream's `ENOENT` handling.
/// Symlinked entries, and children of symlinked legacy containers, are skipped
/// so a link cannot pull an outside tree into scope.
pub fn scan_worktrees(base: &Path) -> io::Result<Vec<WorktreeEntry>> {
	let base = match fs::canonicalize(base) {
		Ok(path) => path,
		Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
		Err(err) => return Err(err),
	};
	let mut entries = Vec::new();
	for child in fs::read_dir(&base)? {
		let child = child?;
		let name = child.file_name();
		if is_trash_name(&name) {
			continue;
		}
		let path = base.join(&name);
		let Ok(meta) = fs::symlink_metadata(&path) else {
			continue;
		};
		if !meta.is_dir() || meta.file_type().is_symlink() {
			continue; // a symlink is never a worktree entry
		}
		if let Some(entry) = classify_dir(&path) {
			entries.push(entry);
			continue;
		}

		// Legacy nesting: <base>/<encoded-project>/<branch-or-id>.
		let mut nested = 0usize;
		let mut children = 0usize;
		if let Ok(read) = fs::read_dir(&path) {
			for item in read.flatten() {
				children += 1;
				let child_name = item.file_name();
				if is_trash_name(&child_name) {
					continue;
				}
				let child_path = path.join(&child_name);
				let Ok(child_meta) = fs::symlink_metadata(&child_path) else {
					continue;
				};
				if !child_meta.is_dir() || child_meta.file_type().is_symlink() {
					continue;
				}
				if let Some(entry) = classify_dir(&child_path) {
					entries.push(entry);
					nested += 1;
				}
			}
		}
		if nested == 0 {
			entries.push(WorktreeEntry {
				path,
				kind: if children == 0 { WorktreeKind::Empty } else { WorktreeKind::Stray },
				parent_repo: None,
				branch: None,
				orphan_reason: Some(if children == 0 {
					"empty directory".to_string()
				} else {
					"no recognizable worktree contents".to_string()
				}),
				blocked: None,
				live_owner: false,
				registered: false,
				admin_dir: None,
			});
		}
	}
	entries.sort_by(|a, b| a.path.cmp(&b.path));
	Ok(entries)
}

fn is_trash_name(name: &std::ffi::OsStr) -> bool {
	name.to_string_lossy().starts_with(TRASH_PREFIX)
}

fn classify_dir(dir: &Path) -> Option<WorktreeEntry> {
	let dot_git = dir.join(".git");
	let git_meta = fs::symlink_metadata(&dot_git);
	if let Ok(meta) = git_meta {
		if meta.file_type().is_symlink() {
			return Some(WorktreeEntry {
				path: dir.to_path_buf(),
				kind: WorktreeKind::PrCheckout,
				parent_repo: None,
				branch: None,
				orphan_reason: Some("`.git` is a symlink; refusing to interpret it".to_string()),
				blocked: Some(
					"`.git` is a symlink, so this checkout cannot be validated or relocated safely"
						.to_string(),
				),
				live_owner: false,
				registered: false,
				admin_dir: None,
			});
		}
		if meta.is_file() {
			return Some(classify_checkout(dir, &dot_git));
		}
	}

	let has_owner = fs::symlink_metadata(dir.join(ISOLATION_OWNER_FILE)).is_ok();
	let has_mount = ISOLATION_MOUNT_DIRS
		.iter()
		.any(|name| fs::symlink_metadata(dir.join(name)).is_ok_and(|meta| meta.is_dir()));
	if !has_owner && !has_mount {
		return None;
	}
	let (live_owner, owner_pid) = isolation_owner(dir);
	let orphan_reason = (!live_owner).then(|| {
		owner_pid.map_or_else(
			|| "task-isolation leftover (no verifiable owner)".to_string(),
			|pid| format!("task-isolation leftover (recorded owner pid {pid} is gone)"),
		)
	});
	Some(WorktreeEntry {
		path: dir.to_path_buf(),
		kind: WorktreeKind::TaskIsolation,
		parent_repo: None,
		branch: None,
		orphan_reason,
		blocked: None,
		live_owner,
		registered: false,
		admin_dir: None,
	})
}

fn classify_checkout(dir: &Path, dot_git: &Path) -> WorktreeEntry {
	let mut entry = WorktreeEntry {
		path: dir.to_path_buf(),
		kind: WorktreeKind::PrCheckout,
		parent_repo: None,
		branch: None,
		orphan_reason: None,
		blocked: None,
		live_owner: false,
		registered: false,
		admin_dir: None,
	};
	let pointer = match read_dot_git_pointer(dot_git) {
		Ok(pointer) => pointer,
		Err(reason) => {
			entry.orphan_reason = Some(reason);
			return entry;
		}
	};
	let admin = if pointer.is_absolute() {
		pointer
	} else {
		normalize_lexical(&dir.join(pointer))
	};
	if admin.file_name().is_none()
		|| admin
			.parent()
			.and_then(Path::file_name)
			.is_none_or(|name| name != std::ffi::OsStr::new("worktrees"))
	{
		entry.orphan_reason =
			Some("malformed .git pointer (not under a worktrees/ directory)".to_string());
		entry.blocked = Some(
			"`.git` points outside any `worktrees/` administration directory; refusing to touch \
			 parent metadata"
				.to_string(),
		);
		return entry;
	}
	entry.parent_repo = parent_repo_of(&admin);
	entry.branch = read_worktree_branch(&admin.join("HEAD"));
	entry.admin_dir = Some(admin.clone());

	let admin_meta = fs::symlink_metadata(&admin);
	let admin_ok = admin_meta.is_ok_and(|meta| meta.is_dir());
	if !admin_ok {
		entry.orphan_reason = Some(ORPHAN_ADMIN_MISSING.to_string());
		return entry;
	}
	let back_pointer_ok = fs::read_to_string(admin.join("gitdir"))
		.ok()
		.and_then(|text| {
			let recorded = PathBuf::from(text.trim());
			let recorded = if recorded.is_absolute() {
				recorded
			} else {
				admin.join(recorded)
			};
			Some((fs::canonicalize(recorded).ok()?, fs::canonicalize(dot_git).ok()?))
		})
		.is_some_and(|(recorded, this)| recorded == this);
	if !back_pointer_ok {
		entry.orphan_reason = Some(
			"parent registration does not point back at this checkout (stale or foreign)".to_string(),
		);
		entry.blocked = Some(
			"the parent `gitdir` back-pointer disagrees with this checkout; refusing to run git \
			 against a registration that does not name it"
				.to_string(),
		);
		return entry;
	}
	if let Some(parent) = &entry.parent_repo
		&& !parent.is_dir()
	{
		entry.orphan_reason = Some("parent repo missing".to_string());
		return entry;
	}
	if let Some(reason) = read_lock_reason(&admin.join("locked")) {
		entry.blocked = Some(reason);
	}
	if fs::symlink_metadata(dir.join(".gitmodules")).is_ok() {
		entry.blocked = Some(
			"checkout contains submodules (`.gitmodules`); git refuses to relocate these and so \
			 does this command"
				.to_string(),
		);
	}
	entry.registered = true;
	entry
}

/// Refusal text for a git-locked worktree, or `None` when no lock file exists.
///
/// `git worktree move` refuses a locked worktree, and relocating one would leave
/// the parent's lock pointing at a path that no longer exists.
fn read_lock_reason(lock_file: &Path) -> Option<String> {
	if fs::symlink_metadata(lock_file).is_err() {
		return None;
	}
	let reason = fs::read_to_string(lock_file)
		.ok()
		.map(|text| text.trim().to_string())
		.filter(|text| !text.is_empty());
	Some(reason.map_or_else(
		|| "worktree is locked in git (`worktrees/<name>/locked`); unlock it before clearing"
			.to_string(),
		|reason| {
			format!(
				"worktree is locked in git (`worktrees/<name>/locked`: {reason}); unlock it \
				 before clearing"
			)
		},
	))
}

fn read_dot_git_pointer(dot_git: &Path) -> Result<PathBuf, String> {
	let meta = fs::metadata(dot_git).map_err(|err| format!("cannot stat .git file: {err}"))?;
	if meta.len() > DOT_GIT_MAX_BYTES {
		return Err("malformed .git file (unexpectedly large)".to_string());
	}
	let text = fs::read_to_string(dot_git).map_err(|err| format!("cannot read .git file: {err}"))?;
	let raw = text
		.lines()
		.find_map(|line| line.strip_prefix("gitdir:"))
		.map(str::trim)
		.filter(|value| !value.is_empty())
		.ok_or_else(|| "malformed .git file (no gitdir line)".to_string())?;
	Ok(PathBuf::from(raw))
}

fn parent_repo_of(admin: &Path) -> Option<PathBuf> {
	let common = admin.parent()?.parent()?;
	if common.file_name().is_some_and(|name| name == ".git") {
		common.parent().map(Path::to_path_buf)
	} else {
		Some(common.to_path_buf())
	}
}

fn read_worktree_branch(head_file: &Path) -> Option<String> {
	let head = fs::read_to_string(head_file).ok()?;
	let head = head.trim();
	head.strip_prefix("ref: refs/heads/").map(str::to_string)
}

/// Marker contents: `(owner_is_live, recorded_pid)`.
///
/// A missing or malformed marker means no verifiable owner (safe to reclaim),
/// matching `hasLiveIsolationOwner`. `kill(pid, 0)` treats every error except
/// `ESRCH` as alive (a live process can still report `EPERM`); a recorded
/// start-time token that no longer matches the live pid means the pid was
/// recycled, so the sandbox is dead.
fn isolation_owner(dir: &Path) -> (bool, Option<i64>) {
	let Ok(text) = fs::read_to_string(dir.join(ISOLATION_OWNER_FILE)) else {
		return (false, None);
	};
	let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
		return (false, None);
	};
	let Some(pid) = value.get("pid").and_then(serde_json::Value::as_i64) else {
		return (false, None);
	};
	if pid <= 0 {
		return (false, Some(pid));
	}
	if !process_alive(pid) {
		return (false, Some(pid));
	}
	let token = value
		.get("startToken")
		.and_then(serde_json::Value::as_str)
		.filter(|token| !token.is_empty());
	let live =
		token.is_none_or(|token| process_start_token(pid).is_none_or(|current| current == token));
	(live, Some(pid))
}

fn process_alive(pid: i64) -> bool {
	let Ok(pid) = libc::pid_t::try_from(pid) else {
		return false;
	};
	// SAFETY: `kill` with signal 0 performs only the permission/existence check;
	// it does not deliver a signal. `pid` is a plain integer.
	let result = unsafe { libc::kill(pid, 0) };
	if result == 0 {
		return true;
	}
	!io::Error::last_os_error()
		.raw_os_error()
		.is_some_and(|code| code == libc::ESRCH)
}

fn process_start_token(pid: i64) -> Option<String> {
	let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
	let rest = stat.rsplit_once(')')?.1;
	// After `pid (comm)` the fields start at state (field 3); starttime is
	// field 22, i.e. the 20th token here.
	let token = rest.split_whitespace().nth(19)?.to_string();
	(!token.is_empty()).then_some(token)
}

// ───────────────────────────────────────────────────────────────────────────
// List
// ───────────────────────────────────────────────────────────────────────────

/// `omp worktree list` body.
pub fn list_worktrees(base: &Path, json: bool, out: &mut dyn Write) -> io::Result<()> {
	let entries = scan_worktrees(base)?;
	if json {
		let value = serde_json::Value::Array(entries.iter().map(entry_json).collect());
		writeln!(out, "{}", serde_json::to_string_pretty(&value).unwrap_or_default())?;
		return Ok(());
	}
	if entries.is_empty() {
		return writeln!(out, "No agent-managed worktrees found under {}.", base.display());
	}
	let mut live = 0usize;
	let mut orphaned = 0usize;
	let mut blocked = 0usize;
	for entry in &entries {
		let tag = if entry.blocked.is_some() {
			blocked += 1;
			"blocked "
		} else if entry.orphan_reason.is_some() {
			orphaned += 1;
			"orphaned"
		} else {
			live += 1;
			"live    "
		};
		writeln!(out, "{tag}  {}", entry.path.display())?;
		let detail = format_entry_detail(entry);
		if !detail.is_empty() {
			writeln!(out, "          {detail}")?;
		}
	}
	writeln!(
		out,
		"\n{live} live · {orphaned} orphaned · {blocked} blocked · {} total",
		entries.len()
	)
}

fn entry_json(entry: &WorktreeEntry) -> serde_json::Value {
	let mut object = serde_json::Map::new();
	object.insert("path".to_string(), entry.path.display().to_string().into());
	object.insert("kind".to_string(), entry.kind.as_str().into());
	if let Some(parent) = &entry.parent_repo {
		object.insert("parentRepo".to_string(), parent.display().to_string().into());
	}
	if let Some(branch) = &entry.branch {
		object.insert("branch".to_string(), branch.clone().into());
	}
	if let Some(reason) = &entry.orphan_reason {
		object.insert("orphanReason".to_string(), reason.clone().into());
	}
	if let Some(reason) = &entry.blocked {
		object.insert("blocked".to_string(), reason.clone().into());
	}
	if entry.kind == WorktreeKind::TaskIsolation {
		object.insert("liveOwner".to_string(), entry.live_owner.into());
	}
	serde_json::Value::Object(object)
}

fn format_entry_detail(entry: &WorktreeEntry) -> String {
	let mut parts: Vec<String> = Vec::new();
	match entry.kind {
		WorktreeKind::PrCheckout => {
			let repo = entry
				.parent_repo
				.as_ref()
				.and_then(|path| path.file_name())
				.map_or_else(|| "unknown repo".to_string(), |name| name.to_string_lossy().into_owned());
			let branch = entry.branch.clone().unwrap_or_else(|| "unknown branch".to_string());
			parts.push(format!("{repo} · {branch}"));
		},
		WorktreeKind::TaskIsolation => parts.push("task-isolation sandbox".to_string()),
		WorktreeKind::Empty => parts.push("legacy project shell".to_string()),
		WorktreeKind::Stray => parts.push("unrecognized contents".to_string()),
	}
	if let Some(reason) = &entry.orphan_reason {
		parts.push(reason.clone());
	}
	if let Some(reason) = &entry.blocked {
		parts.push(reason.clone());
	}
	parts.join(" — ")
}

// ───────────────────────────────────────────────────────────────────────────
// Clear
// ───────────────────────────────────────────────────────────────────────────

/// `omp worktree clear` body.
///
/// Targets are every entry for `--all`, otherwise only entries with an
/// `orphan_reason`. Nothing is deleted: successful targets are quarantined under
/// `<base>/.trash-<date>/`, registered checkouts with the git metadata rewrite.
pub fn clear_worktrees(
	base: &Path,
	options: &ClearOptions,
	out: &mut dyn Write,
) -> io::Result<ClearReport> {
	let entries = scan_worktrees(base)?;
	let targets: Vec<&WorktreeEntry> = entries
		.iter()
		.filter(|entry| options.all || entry.orphan_reason.is_some())
		.collect();

	if targets.is_empty() {
		if options.json {
			writeln!(out, "{}", serde_json::json!({ "removed": 0, "kept": entries.len() }))?;
		} else {
			let suffix = if options.all { "" } else { "orphaned " };
			writeln!(out, "No {suffix}worktrees to clear.")?;
		}
		return Ok(ClearReport { outcomes: Vec::new(), total: entries.len() });
	}

	if options.dry_run {
		let plan: Vec<serde_json::Value> = targets
			.iter()
			.map(|entry| {
				let (action, destination) = plan_entry(entry, base);
				serde_json::json!({
					"path": entry.path.display().to_string(),
					"action": action.as_str(),
					"destination": destination.as_ref().map(|path| path.display().to_string()),
				})
			})
			.collect();
		if options.json {
			let value = serde_json::json!({
				"wouldRemove": targets.iter().map(|e| e.path.display().to_string()).collect::<Vec<_>>(),
				"plan": plan,
			});
			writeln!(out, "{}", serde_json::to_string_pretty(&value).unwrap_or_default())?;
		} else {
			for item in &plan {
				let path = item["path"].as_str().unwrap_or_default();
				let action = item["action"].as_str().unwrap_or("quarantined");
				match item["destination"].as_str() {
					Some(destination) => writeln!(out, "would {action}  {path} -> {destination}")?,
					None => writeln!(out, "would {action}  {path}")?,
				}
			}
			writeln!(
				out,
				"\n{} dir{} would be relocated.",
				targets.len(),
				if targets.len() == 1 { "" } else { "s" }
			)?;
		}
		return Ok(ClearReport { outcomes: Vec::new(), total: entries.len() });
	}

	let canonical_base = fs::canonicalize(base)?;
	let mut outcomes = Vec::with_capacity(targets.len());
	for entry in targets {
		outcomes.push(execute_entry(entry, &canonical_base));
	}
	let report = ClearReport { outcomes, total: entries.len() };
	print_clear_report(&report, options, out)?;
	Ok(report)
}

fn plan_entry(entry: &WorktreeEntry, base: &Path) -> (ClearAction, Option<PathBuf>) {
	if entry.blocked.is_some() {
		return (ClearAction::Refused, None);
	}
	if entry.kind == WorktreeKind::TaskIsolation && entry.live_owner {
		return (ClearAction::Refused, None);
	}
	if entry.registered && entry.kind == WorktreeKind::PrCheckout {
		return (ClearAction::Moved, trash_destination(base, &entry.path).ok());
	}
	(ClearAction::Quarantined, trash_destination(base, &entry.path).ok())
}

fn execute_entry(entry: &WorktreeEntry, canonical_base: &Path) -> ClearOutcome {
	let mut outcome = ClearOutcome {
		path: entry.path.clone(),
		action: ClearAction::Failed,
		destination: None,
		error: None,
		notes: Vec::new(),
	};
	if let Some(reason) = &entry.blocked {
		outcome.action = ClearAction::Refused;
		outcome.error = Some(reason.clone());
		return outcome;
	}
	// Re-validate under the base right before touching anything: a directory can
	// be swapped for a symlink between scan and move.
	let canonical = match validate_containment(entry, canonical_base) {
		Ok(path) => path,
		Err(reason) => {
			outcome.action = ClearAction::Refused;
			outcome.error = Some(reason);
			return outcome;
		},
	};
	// Relocating the directory this process runs inside leaves the caller's
	// cwd and relative paths dangling; refuse and name the path instead.
	if let Some(cwd) = current_dir_canonical()
		&& cwd.starts_with(&canonical)
	{
		outcome.action = ClearAction::Refused;
		outcome.error = Some(
			"this process is running inside the entry; rerun from outside it so its working \
			 directory is not relocated"
				.to_string(),
		);
		return outcome;
	}
	// Re-check liveness at the last moment: a fresh owner may have appeared.
	if entry.kind == WorktreeKind::TaskIsolation && isolation_owner(&entry.path).0 {
		outcome.action = ClearAction::Refused;
		outcome.error = Some(
			"a live task still owns this isolation sandbox; stop that task (or wait for it to \
			 finish) and rerun"
				.to_string(),
		);
		return outcome;
	}

	let Some(name) = entry.path.file_name().map(std::ffi::OsStr::to_os_string) else {
		outcome.error = Some(format!("{} has no file name", entry.path.display()));
		return outcome;
	};
	let trash = canonical_base.join(format!("{TRASH_PREFIX}{}", today_utc()));
	if let Err(err) = fs::create_dir_all(&trash) {
		outcome.error = Some(format!("cannot create {}: {err}", trash.display()));
		return outcome;
	}
	let reservation = match Reservation::create(&trash, &name) {
		Ok(reservation) => reservation,
		Err(err) => {
			outcome.error = Some(err);
			return outcome;
		},
	};
	let destination = reservation.path().to_path_buf();

	if entry.registered && entry.kind == WorktreeKind::PrCheckout {
		let Some(admin) = entry.admin_dir.as_deref() else {
			reservation.release();
			outcome.error = Some(
				"registered checkout has no parsed administration dir; refusing to move it"
					.to_string(),
			);
			return outcome;
		};
		return match move_registered_checkout(&canonical, &reservation, admin) {
			Ok(()) => ClearOutcome {
				path: entry.path.clone(),
				action: ClearAction::Moved,
				destination: Some(destination),
				error: None,
				notes: vec![
					"relocated with git's worktree metadata rewrite; parent registration follows \
					 the checkout and uncommitted files travelled with it"
						.to_string(),
				],
			},
			Err(err) => {
				// Removes only this run's reservation, and only while it is still
				// empty: if the failed move left the checkout there, `remove_dir`
				// refuses and the data stays.
				reservation.release();
				ClearOutcome {
					error: Some(format!("worktree move failed: {err}")),
					destination: None,
					action: ClearAction::Failed,
					path: entry.path.clone(),
					notes: Vec::new(),
				}
			},
		};
	}

	match fs::rename(&canonical, reservation.path()) {
		Ok(()) => {
			if entry.kind == WorktreeKind::PrCheckout
				&& entry
					.admin_dir
					.as_ref()
					.is_some_and(|admin| admin.is_dir())
				&& let Some(parent) = &entry.parent_repo
			{
				outcome.notes.push(format!(
					"parent registration under {} was left untouched (no prune); `git worktree \
					 list` may still show the old path",
					parent.display()
				));
			}
			outcome.action = ClearAction::Quarantined;
			outcome.destination = Some(destination);
			outcome
		},
		Err(err) => {
			reservation.release();
			outcome.error = Some(format!("rename into quarantine failed: {err}"));
			outcome
		},
	}
}

fn current_dir_canonical() -> Option<PathBuf> {
	let cwd = std::env::current_dir().ok()?;
	fs::canonicalize(cwd).ok()
}

/// Canonical path of the entry, after proving it is still a real directory
/// inside the base and outside any quarantine directory.
fn validate_containment(entry: &WorktreeEntry, canonical_base: &Path) -> Result<PathBuf, String> {
	let meta = fs::symlink_metadata(&entry.path)
		.map_err(|err| format!("cannot re-stat entry: {err}"))?;
	if meta.file_type().is_symlink() {
		return Err("entry became a symlink; refusing to move it".to_string());
	}
	if !meta.is_dir() {
		return Err("entry is no longer a directory; refusing to move it".to_string());
	}
	let canonical = fs::canonicalize(&entry.path)
		.map_err(|err| format!("cannot canonicalize entry: {err}"))?;
	if canonical == canonical_base || !canonical.starts_with(canonical_base) {
		return Err("entry resolves outside the worktree base; refusing to move it".to_string());
	}
	if canonical
		.components()
		.any(|component| component.as_os_str().to_string_lossy().starts_with(TRASH_PREFIX))
	{
		return Err("entry lives inside a quarantine directory; refusing to move it".to_string());
	}
	Ok(canonical)
}

/// Stat-only candidate for the dry-run plan. A real run reserves the name
/// atomically with [`Reservation::create`], so this never claims it.
fn trash_destination(canonical_base: &Path, entry: &Path) -> Result<PathBuf, String> {
	let Some(name) = entry.file_name() else {
		return Err(format!("{} has no file name", entry.display()));
	};
	let trash = canonical_base.join(format!("{TRASH_PREFIX}{}", today_utc()));
	for attempt in 1..=10_000u32 {
		let candidate = if attempt == 1 {
			trash.join(name)
		} else {
			let mut suffixed = name.to_os_string();
			suffixed.push(format!("-{attempt}"));
			trash.join(suffixed)
		};
		if fs::symlink_metadata(&candidate).is_err() {
			return Ok(candidate);
		}
	}
	Err(format!("no free quarantine name for {}", entry.display()))
}

/// An empty directory this run created under the trash root.
///
/// Ownership is carried by the value, never inferred from the filesystem: a
/// foreign empty directory that happens to carry the candidate name loses the
/// atomic `create_dir` race (EEXIST) and is skipped, not replaced or removed.
struct Reservation {
	path: PathBuf,
}

impl Reservation {
	/// Atomically claim `<trash>/<name>` or `<trash>/<name>-N`.
	fn create(trash: &Path, name: &std::ffi::OsStr) -> Result<Self, String> {
		for attempt in 1..=10_000u32 {
			let candidate = if attempt == 1 {
				trash.join(name)
			} else {
				let mut suffixed = name.to_os_string();
				suffixed.push(format!("-{attempt}"));
				trash.join(suffixed)
			};
			match fs::create_dir(&candidate) {
				Ok(()) => return Ok(Self { path: candidate }),
				Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {},
				Err(err) => return Err(format!("cannot reserve {}: {err}", candidate.display())),
			}
		}
		Err(format!("no free quarantine name for {}", name.to_string_lossy()))
	}

	fn path(&self) -> &Path {
		&self.path
	}

	/// Give back an unused reservation. `remove_dir` refuses a non-empty
	/// directory, so content that was moved in is never deleted.
	fn release(self) {
		let _ = fs::remove_dir(&self.path);
	}
}

/// Relocate a registered checkout exactly as `git worktree move` does: one
/// same-filesystem rename into the reservation this run created, then the
/// checkout's `.git` pointer and the parent's `worktrees/<name>/gitdir`
/// back-pointer are rewritten to the new path. No `--force`, no prune, no other
/// registration touched, no file deleted: a failed rewrite restores both pointer
/// files verbatim and rolls the rename back, and the error states exactly what
/// was and was not restored.
///
/// No lock is needed for concurrent runs: the only shared state is this
/// checkout's own administration dir, and the rename is the single atomic
/// winner — a second run finds the source gone and reports a failure instead of
/// interleaving a rewrite.
fn move_registered_checkout(
	from: &Path,
	reservation: &Reservation,
	admin: &Path,
) -> Result<(), String> {
	move_registered_checkout_with(from, reservation, admin, &|dot_git, admin, back_pointer| {
		fs::write(dot_git, format!("gitdir: {}\n", admin.display()))
			.and_then(|()| fs::write(back_pointer, format!("{}\n", dot_git.display())))
	})
}

/// [`move_registered_checkout`] with an injectable pointer rewrite, so the
/// failure path can be exercised without a full disk.
fn move_registered_checkout_with(
	from: &Path,
	reservation: &Reservation,
	admin: &Path,
	rewrite: &dyn Fn(&Path, &Path, &Path) -> io::Result<()>,
) -> Result<(), String> {
	let to = reservation.path();
	if !admin.is_dir() {
		return Err(format!("administration dir {} is missing", admin.display()));
	}
	let back_pointer = admin.join("gitdir");
	let previous_back = fs::read(&back_pointer)
		.map_err(|err| format!("cannot read {}: {err}", back_pointer.display()))?;
	// Kept so a failed rewrite can restore the pointer verbatim, including a
	// relative `gitdir:` written by `git worktree repair`.
	let original_dot_git = fs::read(from.join(".git"))
		.map_err(|err| format!("cannot read {}/.git: {err}", from.display()))?;
	fs::rename(from, to)
		.map_err(|err| format!("rename {} -> {} failed: {err}", from.display(), to.display()))?;
	let dot_git = to.join(".git");
	if let Err(err) = rewrite(&dot_git, admin, &back_pointer) {
		let dot_git_restored = fs::write(&dot_git, &original_dot_git).is_ok();
		let back_restored = fs::write(&back_pointer, &previous_back).is_ok();
		let pointers = if dot_git_restored && back_restored {
			"both pointer files were restored".to_string()
		} else {
			format!(
				"pointer restore is incomplete ({}; {})",
				if dot_git_restored {
					"checkout `.git` restored"
				} else {
					"checkout `.git` NOT restored"
				},
				if back_restored {
					"parent `gitdir` restored"
				} else {
					"parent `gitdir` NOT restored"
				}
			)
		};
		return Err(match restore_location(from, to) {
			Ok(()) => format!(
				"metadata rewrite failed ({err}); the entry was moved back to {} and {pointers}",
				from.display()
			),
			Err(why) => format!(
				"metadata rewrite failed ({err}) and the entry could not be moved back ({why}); \
				 the checkout stays at {} and {pointers}; its parent registration is stale (it \
				 does not reliably name that path, and `git worktree prune` drops it once the \
				 path is gone)",
				to.display()
			),
		});
	}
	Ok(())
}

/// Move the checkout back to its old path without ever replacing a directory a
/// concurrent process may have created there: the old slot is claimed
/// atomically with `create_dir`, and `EEXIST` fails closed instead of letting
/// `rename` swap a foreign empty directory out of the way.
fn restore_location(from: &Path, to: &Path) -> Result<(), String> {
	match fs::create_dir(from) {
		Ok(()) => {},
		Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
			return Err(format!("{} is occupied by another directory", from.display()));
		},
		Err(err) => return Err(format!("cannot reclaim {}: {err}", from.display())),
	}
	match fs::rename(to, from) {
		Ok(()) => Ok(()),
		Err(err) => {
			// Only this run's own empty claim, so no user content can be removed.
			let _ = fs::remove_dir(from);
			Err(format!("rename back failed: {err}"))
		},
	}
}

fn print_clear_report(
	report: &ClearReport,
	options: &ClearOptions,
	out: &mut dyn Write,
) -> io::Result<()> {
	if options.json {
		let results: Vec<serde_json::Value> = report
			.outcomes
			.iter()
			.map(|outcome| {
				serde_json::json!({
					"path": outcome.path.display().to_string(),
					"action": outcome.action.as_str(),
					"ok": outcome.action.is_success(),
					"destination": outcome.destination.as_ref().map(|path| path.display().to_string()),
					"error": outcome.error,
					"notes": outcome.notes,
				})
			})
			.collect();
		let value = serde_json::json!({
			"removed": report.relocated(),
			"quarantined": report.count(ClearAction::Quarantined),
			"moved": report.count(ClearAction::Moved),
			"refused": report.refused(),
			"failed": report.failed(),
			"results": results,
		});
		writeln!(out, "{}", serde_json::to_string_pretty(&value).unwrap_or_default())?;
		return Ok(());
	}

	for outcome in &report.outcomes {
		match outcome.action {
			ClearAction::Moved | ClearAction::Quarantined => {
				writeln!(
					out,
					"{:<12}{} -> {}",
					outcome.action.as_str(),
					outcome.path.display(),
					outcome
						.destination
						.as_ref()
						.map_or_else(String::new, |path| path.display().to_string())
				)?;
			},
			ClearAction::Refused => {
				writeln!(out, "refused     {}", outcome.path.display())?;
				if let Some(error) = &outcome.error {
					writeln!(out, "            {error}")?;
				}
			},
			ClearAction::Failed => {
				writeln!(out, "failed      {}", outcome.path.display())?;
				if let Some(error) = &outcome.error {
					writeln!(out, "            {error}")?;
				}
			},
		}
		for note in &outcome.notes {
			writeln!(out, "            {note}")?;
		}
	}
	writeln!(
		out,
		"\n{} moved · {} quarantined · {} refused · {} failed",
		report.count(ClearAction::Moved),
		report.count(ClearAction::Quarantined),
		report.refused(),
		report.failed()
	)
}

// ───────────────────────────────────────────────────────────────────────────
// Small helpers
// ───────────────────────────────────────────────────────────────────────────

/// Resolve `..`/`.` lexically against an absolute base without touching the
/// filesystem. A `..` above the root is dropped rather than escaping.
fn normalize_lexical(path: &Path) -> PathBuf {
	let mut out = PathBuf::new();
	for component in path.components() {
		match component {
			Component::CurDir => {},
			Component::ParentDir => {
				out.pop();
			},
			other => out.push(other.as_os_str()),
		}
	}
	out
}

fn today_utc() -> String {
	let seconds = SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map_or(0, |elapsed| elapsed.as_secs());
	let days = i64::try_from(seconds / 86_400).unwrap_or(0);
	let (year, month, day) = civil_from_days(days);
	format!("{year:04}-{month:02}-{day:02}")
}

/// Howard Hinnant's `civil_from_days` (days since 1970-01-01 → Y-M-D).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
	let z = days + 719_468;
	let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
	let day_of_era = u64::try_from(z - era * 146_097).unwrap_or(0);
	let year_of_era =
		(day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
	let year = i64::try_from(year_of_era).unwrap_or(0) + era * 400;
	let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
	let month_prime = (5 * day_of_year + 2) / 153;
	let day = u32::try_from(day_of_year - (153 * month_prime + 2) / 5 + 1).unwrap_or(1);
	let month_value = if month_prime < 10 {
		month_prime + 3
	} else {
		month_prime - 9
	};
	let month = u32::try_from(month_value).unwrap_or(1);
	(if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
	use std::{
		ffi::OsStr,
		fs, io,
		path::{Path, PathBuf},
		time::{SystemTime, UNIX_EPOCH},
	};

	use super::{Reservation, move_registered_checkout_with};

	/// Pointer bytes kept deliberately relative, so a restore that re-serialises
	/// instead of copying is visible.
	const ORIGINAL_DOT_GIT: &[u8] = b"gitdir: ../../parent/.git/worktrees/n\n";
	const ORIGINAL_BACK_POINTER: &[u8] = b"/old/location/wt/.git\n";

	struct Scratch(PathBuf);

	impl Scratch {
		fn new(tag: &str) -> Self {
			let nanos = SystemTime::now()
				.duration_since(UNIX_EPOCH)
				.expect("clock")
				.as_nanos();
			let root = std::env::temp_dir().join(format!("omp-worktree-unit-{tag}-{nanos}"));
			fs::create_dir_all(&root).expect("scratch");
			Self(root)
		}

		/// Fake registered checkout: `<root>/parent/.git/worktrees/n` plus
		/// `<root>/wt` whose `.git` and back-pointer carry the original bytes.
		fn checkout(&self) -> (PathBuf, PathBuf) {
			let admin = self.0.join("parent/.git/worktrees/n");
			fs::create_dir_all(&admin).expect("admin");
			let checkout = self.0.join("wt");
			fs::create_dir_all(&checkout).expect("checkout");
			fs::write(checkout.join(".git"), ORIGINAL_DOT_GIT).expect("dot git");
			fs::write(admin.join("gitdir"), ORIGINAL_BACK_POINTER).expect("back pointer");
			(checkout, admin)
		}

		fn trash(&self) -> PathBuf {
			let trash = self.0.join(".trash-2026-10-29");
			fs::create_dir_all(&trash).expect("trash");
			trash
		}
	}

	impl Drop for Scratch {
		fn drop(&mut self) {
			let _ = fs::remove_dir_all(&self.0);
		}
	}

	fn reserve(trash: &Path, name: &str) -> Reservation {
		Reservation::create(trash, OsStr::new(name)).expect("reserve")
	}

	#[test]
	fn failed_rewrite_restores_both_pointer_bytes_and_the_original_location() {
		let scratch = Scratch::new("rollback");
		let (checkout, admin) = scratch.checkout();
		let reservation = reserve(&scratch.trash(), "wt");
		let error = move_registered_checkout_with(
			&checkout,
			&reservation,
			&admin,
			&|dot_git, _admin, _back| {
				// What a write that fails mid-way leaves behind.
				fs::write(dot_git, b"").expect("truncate");
				Err(io::Error::other("injected ENOSPC"))
			},
		)
		.expect_err("injected failure");

		assert!(checkout.is_dir(), "the checkout is back at its original path");
		assert_eq!(
			fs::read(checkout.join(".git")).expect("dot git"),
			ORIGINAL_DOT_GIT.to_vec(),
			"the checkout pointer is restored byte-for-byte"
		);
		assert_eq!(
			fs::read(admin.join("gitdir")).expect("back pointer"),
			ORIGINAL_BACK_POINTER.to_vec(),
			"the parent back-pointer is restored byte-for-byte"
		);
		// The rollback renames the reservation directory itself back into the old
		// slot, so there is nothing left behind for the caller to clean up.
		let reserved = reservation.path().to_path_buf();
		assert!(!reserved.exists(), "the move back consumed the reservation");
		reservation.release();
		assert!(
			!reserved.exists(),
			"release is a no-op once the reservation is gone"
		);
		assert!(error.contains("moved back"), "truthful error: {error}");
		assert!(
			error.contains("both pointer files were restored"),
			"truthful error: {error}"
		);
		assert!(!error.contains("NOT restored"), "no false claim: {error}");
	}

	#[test]
	fn failed_rollback_reports_the_real_location_and_restore_state() {
		let scratch = Scratch::new("blocked-rollback");
		let (checkout, admin) = scratch.checkout();
		let reservation = reserve(&scratch.trash(), "wt");
		let moved_to = reservation.path().to_path_buf();
		let blocker = checkout.clone();
		let error = move_registered_checkout_with(
			&checkout,
			&reservation,
			&admin,
			&|dot_git, admin, _back| {
				fs::write(dot_git, format!("gitdir: {}\n", admin.display())).expect("dot git write");
				// Occupy the old path so the rollback rename cannot succeed.
				fs::create_dir_all(blocker.join("blocker")).expect("blocker");
				Err(io::Error::other("injected EIO"))
			},
		)
		.expect_err("injected failure");

		assert!(moved_to.join(".git").is_file(), "the checkout stays at the reservation");
		assert_eq!(
			fs::read(moved_to.join(".git")).expect("dot git"),
			ORIGINAL_DOT_GIT.to_vec(),
			"the checkout pointer is restored even though the move back failed"
		);
		assert_eq!(
			fs::read(admin.join("gitdir")).expect("back pointer"),
			ORIGINAL_BACK_POINTER.to_vec()
		);
		assert!(
			error.contains("could not be moved back"),
			"truthful error: {error}"
		);
		assert!(error.contains("both pointer files were restored"), "{error}");
		assert!(
			error.contains(&moved_to.display().to_string()),
			"the error names the real location: {error}"
		);
		assert!(
			error.contains("stale") && error.contains("prune"),
			"the error marks the registration stale and prunable: {error}"
		);
	}

	#[test]
	fn foreign_empty_dir_at_the_old_slot_is_never_replaced() {
		let scratch = Scratch::new("old-slot");
		let (checkout, admin) = scratch.checkout();
		fs::write(checkout.join("payload.txt"), b"user data").expect("payload");
		let reservation = reserve(&scratch.trash(), "wt");
		let moved_to = reservation.path().to_path_buf();
		let old_slot = checkout.clone();
		let error = move_registered_checkout_with(
			&checkout,
			&reservation,
			&admin,
			&|dot_git, admin, _back| {
				fs::write(dot_git, format!("gitdir: {}\n", admin.display())).expect("dot git write");
				// A concurrent run recreates the old slot as an empty directory.
				fs::create_dir(&old_slot).expect("claim old slot");
				Err(io::Error::other("injected EIO"))
			},
		)
		.expect_err("injected failure");

		assert!(old_slot.is_dir(), "the foreign directory is still there");
		assert!(
			fs::read_dir(&old_slot)
				.expect("old slot")
				.next()
				.is_none(),
			"the foreign empty directory was not replaced or filled"
		);
		assert!(
			moved_to.join("payload.txt").is_file(),
			"the checkout is not lost: it stays at the reservation"
		);
		assert_eq!(
			fs::read(moved_to.join("payload.txt")).expect("payload"),
			b"user data".to_vec()
		);
		assert_eq!(
			fs::read(moved_to.join(".git")).expect("dot git"),
			ORIGINAL_DOT_GIT.to_vec()
		);
		assert_eq!(
			fs::read(admin.join("gitdir")).expect("back pointer"),
			ORIGINAL_BACK_POINTER.to_vec()
		);
		assert!(
			error.contains("could not be moved back") && error.contains("occupied"),
			"truthful error: {error}"
		);
		assert!(error.contains("stale"), "stale registration named: {error}");
	}

	#[test]
	fn foreign_empty_directory_is_never_claimed_or_removed() {
		let scratch = Scratch::new("reservation");
		let trash = scratch.trash();
		let foreign = trash.join("leftover");
		fs::create_dir(&foreign).expect("foreign empty dir");

		let reservation = reserve(&trash, "leftover");
		assert_eq!(
			reservation.path().to_path_buf(),
			trash.join("leftover-2"),
			"a taken name is skipped atomically"
		);
		assert!(foreign.is_dir(), "the foreign directory is untouched");

		fs::write(reservation.path().join("payload.txt"), b"data").expect("payload");
		let reserved = reservation.path().to_path_buf();
		reservation.release();
		assert!(
			reserved.join("payload.txt").is_file(),
			"release never removes content that was moved in"
		);
	}
}
