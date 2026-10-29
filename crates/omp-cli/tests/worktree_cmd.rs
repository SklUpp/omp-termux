//! Behavioral checks for `omp worktree` scanning and sweeping.
//!
//! Fixtures are real git repositories: a pass means the moved checkout keeps
//! working (`git worktree list` follows it, `git status` still sees the dirty
//! and untracked files) instead of merely that files appeared somewhere.

use std::{
	fs,
	path::{Path, PathBuf},
	process::Command,
	time::{SystemTime, UNIX_EPOCH},
};

use omp_cli::worktree_cmd::{
	clear_worktrees, list_worktrees, resolve_base, scan_worktrees, ClearAction, ClearOptions,
	WorktreeKind,
};

struct Fixture {
	root: PathBuf,
}

impl Fixture {
	fn new(tag: &str) -> Self {
		let nanos = SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.expect("clock")
			.as_nanos();
		let root = std::env::temp_dir().join(format!("omp-worktree-{tag}-{nanos}"));
		fs::create_dir_all(root.join("base")).expect("fixture dirs");
		fs::create_dir_all(root.join("parent")).expect("fixture dirs");
		Self { root }
	}

	fn base(&self) -> PathBuf {
		self.root.join("base")
	}

	fn parent(&self) -> PathBuf {
		self.root.join("parent")
	}

	fn run_git(&self, cwd: &Path, args: &[&str]) -> String {
		let output = Command::new("git")
			.args([
				"-c",
				"user.name=omp-test",
				"-c",
				"user.email=omp@example.invalid",
				"-c",
				"commit.gpgsign=false",
			])
			.arg("-C")
			.arg(cwd)
			.args(args)
			.env("GIT_TERMINAL_PROMPT", "0")
			.env("GIT_CONFIG_GLOBAL", "/dev/null")
			.env("GIT_CONFIG_SYSTEM", "/dev/null")
			.output()
			.expect("spawn git");
		assert!(
			output.status.success(),
			"git {args:?} failed: {}",
			String::from_utf8_lossy(&output.stderr)
		);
		String::from_utf8_lossy(&output.stdout).into_owned()
	}

	fn init_parent(&self) {
		let parent = self.parent();
		self.run_git(&parent, &["init", "-q", "-b", "main"]);
		fs::write(parent.join("tracked.txt"), "base\n").expect("seed file");
		self.run_git(&parent, &["add", "-A"]);
		self.run_git(&parent, &["commit", "-qm", "init"]);
	}

	fn add_worktree(&self, name: &str, branch: &str) -> PathBuf {
		let path = self.base().join(name);
		let path_text = path.to_str().expect("utf8 path").to_string();
		self.run_git(&self.parent(), &["worktree", "add", "-q", path_text.as_str(), "-b", branch]);
		path
	}

	fn read(&self, path: &Path) -> String {
		fs::read_to_string(path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()))
	}

	fn scan(&self) -> Vec<omp_cli::worktree_cmd::WorktreeEntry> {
		scan_worktrees(&self.base()).expect("scan")
	}
}

impl Drop for Fixture {
	fn drop(&mut self) {
		let _ = fs::remove_dir_all(&self.root);
	}
}

fn options(all: bool) -> ClearOptions {
	ClearOptions { all, dry_run: false, json: false }
}

fn canonical(path: &Path) -> PathBuf {
	fs::canonicalize(path).unwrap_or_else(|err| panic!("canonicalize {}: {err}", path.display()))
}

fn trash_dirs(base: &Path) -> Vec<PathBuf> {
	let mut found: Vec<PathBuf> = fs::read_dir(base)
		.expect("read base")
		.flatten()
		.map(|entry| entry.path())
		.filter(|path| {
			path.file_name()
				.is_some_and(|name| name.to_string_lossy().starts_with(".trash-"))
		})
		.collect();
	found.sort();
	found
}

#[test]
fn registered_checkout_moves_with_dirty_and_untracked_files() {
	let fixture = Fixture::new("move");
	fixture.init_parent();
	let checkout = fixture.add_worktree("pr1", "feat");
	fs::write(checkout.join("tracked.txt"), "base\nlocal edit\n").expect("dirty file");
	fs::write(checkout.join("untracked.txt"), "scratch\n").expect("untracked file");

	let entries = fixture.scan();
	assert_eq!(entries.len(), 1, "expected one entry: {entries:?}");
	let entry = &entries[0];
	assert_eq!(entry.kind, WorktreeKind::PrCheckout);
	assert!(entry.registered, "checkout should be registered: {entry:?}");
	assert_eq!(entry.branch.as_deref(), Some("feat"));
	assert!(entry.orphan_reason.is_none(), "live: {entry:?}");
	assert!(entry.blocked.is_none(), "unblocked: {entry:?}");
	let admin = entry.admin_dir.clone().expect("admin dir");

	let mut out = Vec::new();
	let report = clear_worktrees(&fixture.base(), &options(true), &mut out).expect("clear");
	assert_eq!(report.relocated(), 1, "report: {report:?}");
	let outcome = &report.outcomes[0];
	assert_eq!(outcome.action, ClearAction::Moved, "outcome: {outcome:?}");
	assert!(!report.exit_failure());
	let destination = outcome.destination.clone().expect("destination");
	assert!(!checkout.exists(), "old checkout path should be gone");
	assert!(
		destination.starts_with(canonical(&fixture.base())),
		"inside base"
	);

	// Files, including uncommitted work, travelled with the directory.
	assert_eq!(
		fixture.read(&destination.join("tracked.txt")),
		"base\nlocal edit\n"
	);
	assert_eq!(fixture.read(&destination.join("untracked.txt")), "scratch\n");

	// Both pointer files name each other again, and git agrees.
	let dot_git = fixture.read(&destination.join(".git"));
	assert_eq!(dot_git.trim(), format!("gitdir: {}", admin.display()));
	let back = fixture.read(&admin.join("gitdir"));
	assert_eq!(back.trim(), destination.join(".git").display().to_string());
	let listed = fixture.run_git(&fixture.parent(), &["worktree", "list", "--porcelain"]);
	assert!(
		listed.contains(&destination.display().to_string()),
		"registration should follow the checkout: {listed}"
	);
	assert!(
		!listed.contains(&checkout.display().to_string()),
		"old path should be gone from the registration: {listed}"
	);
	let status = fixture.run_git(&destination, &["status", "--porcelain"]);
	assert!(status.contains("tracked.txt"), "dirty state lost: {status}");
	assert!(status.contains("untracked.txt"), "untracked file lost: {status}");
	let branch = fixture.run_git(&destination, &["rev-parse", "--abbrev-ref", "HEAD"]);
	assert_eq!(branch.trim(), "feat");

	// Trash is never rescanned, so a second run is a no-op.
	assert!(fixture.scan().is_empty(), "trash must be excluded from scans");
	let mut second = Vec::new();
	let again = clear_worktrees(&fixture.base(), &options(true), &mut second).expect("second clear");
	assert!(again.outcomes.is_empty(), "repeated clear: {again:?}");
	assert!(!again.exit_failure());
	assert_eq!(trash_dirs(&fixture.base()).len(), 1, "no extra trash dirs");
}

#[test]
fn orphaned_checkout_is_quarantined_and_other_registration_survives() {
	let fixture = Fixture::new("orphan");
	fixture.init_parent();
	let doomed = fixture.add_worktree("pr1", "feat");
	let survivor = fixture.add_worktree("pr2", "other");
	fs::write(doomed.join("payload.txt"), "keep me\n").expect("payload");

	let admin = fixture
		.scan()
		.into_iter()
		.find(|entry| entry.path == canonical(&doomed))
		.and_then(|entry| entry.admin_dir)
		.expect("admin dir");
	fs::remove_dir_all(&admin).expect("drop registration");
	let survivor_admin = fixture.parent().join(".git/worktrees/pr2");
	assert!(survivor_admin.is_dir());

	let mut out = Vec::new();
	let report = clear_worktrees(&fixture.base(), &options(false), &mut out).expect("clear");
	assert_eq!(report.relocated(), 1, "report: {report:?}");
	assert_eq!(report.refused(), 0, "report: {report:?}");
	let destination = report.outcomes[0].destination.clone().expect("destination");
	assert_eq!(fixture.read(&destination.join("payload.txt")), "keep me\n");
	assert!(survivor.is_dir(), "live checkout must not be touched");
	assert!(survivor_admin.is_dir(), "unrelated registration must survive");
	assert!(
		fs::read_to_string(survivor.join(".git")).is_ok(),
		"surviving checkout must keep its pointer"
	);
	let listed = fixture.run_git(&fixture.parent(), &["worktree", "list", "--porcelain"]);
	assert!(listed.contains(&survivor.display().to_string()), "{listed}");
}

#[cfg(unix)]
#[test]
fn symlinked_entries_and_pointers_are_never_followed() {
	use std::os::unix::fs::symlink;

	let fixture = Fixture::new("symlink");
	let outside = fixture.root.join("outside");
	fs::create_dir_all(outside.join("precious")).expect("outside dir");
	fs::write(outside.join("precious/keep.txt"), "outside\n").expect("outside file");
	symlink(&outside, fixture.base().join("escape")).expect("symlink");
	fs::create_dir_all(fixture.base().join("legacy")).expect("legacy dir");
	symlink(&outside, fixture.base().join("legacy/nested")).expect("nested symlink");

	let entries = fixture.scan();
	assert!(
		entries
			.iter()
			.all(|entry| !entry.path.starts_with(&outside)),
		"symlinks must not become entries: {entries:?}"
	);
	assert!(
		entries
			.iter()
			.all(|entry| entry.path != canonical(&fixture.base()).join("escape")),
		"a symlinked top-level entry is skipped: {entries:?}"
	);

	let mut out = Vec::new();
	let report = clear_worktrees(&fixture.base(), &options(true), &mut out).expect("clear");
	// Only the `legacy` container is a real directory; it is quarantined, and the
	// symlinked child travels inside it without the target being touched.
	assert!(
		report.outcomes.iter().all(|outcome| !outcome.path.starts_with(&outside)),
		"outside tree must not be a target: {report:?}"
	);
	assert!(
		outside.join("precious/keep.txt").is_file(),
		"outside tree must be untouched"
	);

	// A checkout whose `.git` is a symlink is classified but never moved.
	let weird = fixture.base().join("weird");
	fs::create_dir_all(&weird).expect("weird dir");
	fs::write(outside.join("gitfile"), "gitdir: /nonexistent\n").expect("pointer file");
	symlink(outside.join("gitfile"), weird.join(".git")).expect("git symlink");
	let entries = fixture.scan();
	let entry = entries
		.iter()
		.find(|entry| entry.path == canonical(&weird))
		.expect("weird entry");
	assert!(entry.blocked.is_some(), "must be blocked: {entry:?}");
	let mut out = Vec::new();
	let report = clear_worktrees(&fixture.base(), &options(false), &mut out).expect("clear");
	assert_eq!(report.refused(), 1, "report: {report:?}");
	assert!(report.exit_failure(), "a refusal is not success");
	assert!(weird.is_dir(), "refused entry stays in place");
	assert!(
		outside.join("gitfile").is_file(),
		"symlink target must be untouched"
	);
}

#[test]
fn pointer_outside_worktrees_admin_is_refused() {
	let fixture = Fixture::new("pointer");
	let bogus = fixture.base().join("bogus");
	fs::create_dir_all(&bogus).expect("bogus dir");
	fs::write(bogus.join(".git"), format!("gitdir: {}/elsewhere/thing\n", fixture.root.display()))
		.expect("pointer");
	fs::write(bogus.join("payload.txt"), "stay\n").expect("payload");

	let entry = fixture
		.scan()
		.into_iter()
		.find(|entry| entry.path == bogus)
		.expect("entry");
	assert!(entry.blocked.is_some(), "blocked: {entry:?}");
	assert!(!entry.registered);

	let mut out = Vec::new();
	let report = clear_worktrees(&fixture.base(), &options(false), &mut out).expect("clear");
	assert_eq!(report.refused(), 1, "report: {report:?}");
	assert!(report.exit_failure());
	assert_eq!(fixture.read(&bogus.join("payload.txt")), "stay\n");
}

#[test]
fn live_isolation_owner_is_protected_while_a_dead_one_is_reclaimed() {
	let fixture = Fixture::new("owner");
	let live = fixture.base().join("live-sandbox");
	fs::create_dir_all(live.join("m")).expect("live sandbox");
	fs::write(live.join("m/state.txt"), "running\n").expect("live payload");
	fs::write(
		live.join(".omp-isolation-owner.json"),
		format!("{{\"pid\":{},\"id\":\"t1\"}}", std::process::id()),
	)
	.expect("owner marker");

	let mut child = Command::new("true").spawn().expect("spawn true");
	let dead_pid = child.id();
	child.wait().expect("reap");
	let dead = fixture.base().join("dead-sandbox");
	fs::create_dir_all(dead.join("m")).expect("dead sandbox");
	fs::write(dead.join("m/state.txt"), "crashed\n").expect("dead payload");
	fs::write(
		dead.join(".omp-isolation-owner.json"),
		format!("{{\"pid\":{dead_pid},\"id\":\"t2\"}}"),
	)
	.expect("dead marker");

	let entries = fixture.scan();
	let live_entry = entries
		.iter()
		.find(|entry| entry.path == canonical(&live))
		.expect("live entry");
	assert!(live_entry.live_owner, "live: {live_entry:?}");
	assert!(live_entry.orphan_reason.is_none());
	let dead_entry = entries
		.iter()
		.find(|entry| entry.path == canonical(&dead))
		.expect("dead entry");
	assert!(!dead_entry.live_owner, "dead: {dead_entry:?}");
	assert!(dead_entry.orphan_reason.is_some());

	let mut out = Vec::new();
	let report = clear_worktrees(&fixture.base(), &options(true), &mut out).expect("clear");
	assert_eq!(report.refused(), 1, "report: {report:?}");
	assert_eq!(report.relocated(), 1, "report: {report:?}");
	assert!(report.exit_failure(), "protected path is not success");
	let refused = report
		.outcomes
		.iter()
		.find(|outcome| outcome.action == ClearAction::Refused)
		.expect("refusal");
	assert_eq!(refused.path, canonical(&live), "exact protected path must be reported");
	assert!(
		refused
			.error
			.as_deref()
			.is_some_and(|error| error.contains("live task")),
		"actionable refusal: {refused:?}"
	);
	assert!(live.join("m/state.txt").is_file(), "live work preserved");
	let moved = report
		.outcomes
		.iter()
		.find(|outcome| outcome.action == ClearAction::Quarantined)
		.expect("quarantine");
	let destination = moved.destination.clone().expect("destination");
	assert_eq!(fixture.read(&destination.join("m/state.txt")), "crashed\n");
}

#[test]
fn trash_collision_never_overwrites_earlier_quarantine() {
	let fixture = Fixture::new("collision");
	let entry = fixture.base().join("leftover");
	fs::create_dir_all(&entry).expect("leftover dir");
	fs::write(entry.join("note.txt"), "first\n").expect("first payload");

	let mut out = Vec::new();
	let first = clear_worktrees(&fixture.base(), &options(true), &mut out).expect("first clear");
	let first_destination = first.outcomes[0]
		.destination
		.clone()
		.expect("first destination");
	assert_eq!(fixture.read(&first_destination.join("note.txt")), "first\n");

	fs::create_dir_all(&entry).expect("second leftover");
	fs::write(entry.join("note.txt"), "second\n").expect("second payload");
	let mut out = Vec::new();
	let second = clear_worktrees(&fixture.base(), &options(true), &mut out).expect("second clear");
	let second_destination = second.outcomes[0]
		.destination
		.clone()
		.expect("second destination");
	assert_ne!(first_destination, second_destination, "names must not repeat");
	assert_eq!(fixture.read(&first_destination.join("note.txt")), "first\n");
	assert_eq!(fixture.read(&second_destination.join("note.txt")), "second\n");

	// A concurrent run's empty reservation owns its name; it must be preserved
	// and the new entry parked after it.
	let foreign = second_destination.with_file_name("leftover-3");
	fs::create_dir(&foreign).expect("foreign reservation");
	fs::create_dir_all(&entry).expect("third leftover");
	fs::write(entry.join("note.txt"), "third\n").expect("third payload");
	let mut out = Vec::new();
	let third = clear_worktrees(&fixture.base(), &options(true), &mut out).expect("third clear");
	let third_destination = third.outcomes[0]
		.destination
		.clone()
		.expect("third destination");
	assert_eq!(third_destination, foreign.with_file_name("leftover-4"));
	assert!(foreign.is_dir(), "the foreign empty reservation is preserved");
	assert!(
		fs::read_dir(&foreign)
			.expect("foreign")
			.next()
			.is_none(),
		"the foreign reservation stays empty and untouched"
	);
	assert_eq!(fixture.read(&first_destination.join("note.txt")), "first\n");
	assert_eq!(fixture.read(&second_destination.join("note.txt")), "second\n");
	assert_eq!(fixture.read(&third_destination.join("note.txt")), "third\n");
	assert_eq!(trash_dirs(&fixture.base()).len(), 1, "one trash dir only");
}

#[test]
fn locked_worktree_is_refused_with_exact_path_and_registration_intact() {
	let fixture = Fixture::new("locked");
	fixture.init_parent();
	let checkout = fixture.add_worktree("pr1", "feat");
	let admin = fixture
		.scan()
		.into_iter()
		.find(|entry| entry.path == canonical(&checkout))
		.and_then(|entry| entry.admin_dir)
		.expect("admin dir");
	fs::write(admin.join("locked"), "in use by agent\n").expect("lock file");
	let leftovers = fixture.base().join("leftover");
	fs::create_dir_all(&leftovers).expect("leftover dir");
	fs::write(leftovers.join("junk.txt"), "junk\n").expect("junk");

	let entry = fixture
		.scan()
		.into_iter()
		.find(|entry| entry.path == canonical(&checkout))
		.expect("locked entry");
	assert!(
		entry
			.blocked
			.as_deref()
			.is_some_and(|reason| reason.contains("locked")),
		"blocked: {entry:?}"
	);

	let mut out = Vec::new();
	let report = clear_worktrees(&fixture.base(), &options(true), &mut out).expect("clear");
	assert_eq!(report.refused(), 1, "report: {report:?}");
	assert_eq!(report.relocated(), 1, "partial success must still report: {report:?}");
	assert!(report.exit_failure());
	let refused = report
		.outcomes
		.iter()
		.find(|outcome| outcome.action == ClearAction::Refused)
		.expect("refusal");
	assert_eq!(refused.path, canonical(&checkout));
	assert!(refused.destination.is_none());
	assert!(checkout.is_dir(), "locked worktree stays in place");
	assert_eq!(
		fixture.read(&admin.join("gitdir")).trim(),
		checkout.join(".git").display().to_string(),
		"registration must not be rewritten"
	);
	assert_eq!(fixture.read(&checkout.join(".git")).trim(), format!("gitdir: {}", admin.display()));
}

#[test]
fn dry_run_reports_without_moving() {
	let fixture = Fixture::new("dry");
	let entry = fixture.base().join("leftover");
	fs::create_dir_all(&entry).expect("leftover dir");
	fs::write(entry.join("note.txt"), "keep\n").expect("payload");

	let mut out = Vec::new();
	let report = clear_worktrees(
		&fixture.base(),
		&ClearOptions { all: true, dry_run: true, json: false },
		&mut out,
	)
	.expect("dry run");
	assert!(report.outcomes.is_empty(), "dry run touches nothing: {report:?}");
	assert!(entry.join("note.txt").is_file(), "no move");
	let text = String::from_utf8(out).expect("utf8");
	assert!(text.contains("would"), "plan text: {text}");
	assert!(trash_dirs(&fixture.base()).is_empty(), "no trash dir created");
}

#[test]
fn json_reports_carry_per_entry_results() {
	let fixture = Fixture::new("json");
	let entry = fixture.base().join("leftover");
	fs::create_dir_all(&entry).expect("leftover dir");
	fs::write(entry.join("note.txt"), "keep\n").expect("payload");

	let mut out = Vec::new();
	list_worktrees(&fixture.base(), true, &mut out).expect("list json");
	let listed: serde_json::Value = serde_json::from_slice(&out).expect("list json parses");
	let array = listed.as_array().expect("array");
	assert_eq!(array.len(), 1, "{listed}");
	assert_eq!(array[0]["kind"], "stray");
	let expected_path = canonical(&entry).display().to_string();
	assert_eq!(array[0]["path"].as_str(), Some(expected_path.as_str()));
	assert!(array[0]["orphanReason"].is_string());

	let mut out = Vec::new();
	clear_worktrees(
		&fixture.base(),
		&ClearOptions { all: true, dry_run: false, json: true },
		&mut out,
	)
	.expect("clear json");
	let cleared: serde_json::Value = serde_json::from_slice(&out).expect("clear json parses");
	assert_eq!(cleared["removed"], 1, "{cleared}");
	assert_eq!(cleared["failed"], 0, "{cleared}");
	assert_eq!(cleared["refused"], 0, "{cleared}");
	let results = cleared["results"].as_array().expect("results");
	assert_eq!(results[0]["ok"], true);
	assert_eq!(results[0]["action"], "quarantined");
	assert!(
		results[0]["destination"]
			.as_str()
			.is_some_and(|path| Path::new(path).is_dir()),
		"{cleared}"
	);
}

#[test]
fn resolve_base_contract() {
	let absolute = std::env::temp_dir().join("omp-worktree-base");
	// `getWorktreesDir` lets the environment variable win over the configured
	// base, so the explicit-path assertions only hold when it is unset.
	if std::env::var_os("OMP_WORKTREE_DIR").is_some() {
		assert!(resolve_base(Some(&absolute)).is_ok(), "environment base resolves");
		return;
	}
	assert_eq!(resolve_base(Some(&absolute)).expect("absolute base"), absolute);
	assert!(
		resolve_base(Some(Path::new("relative/base"))).is_err(),
		"a relative base cannot be bound to a launch cwd"
	);
	assert!(
		resolve_base(None)
			.expect("default base")
			.ends_with("wt"),
		"the default base is <config root>/wt"
	);
}
