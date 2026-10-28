//! `omp ps`: inspect and control daemon-broker supervised processes from
//! outside the harness.
//!
//! Mirrors `packages/coding-agent/src/commands/ps.ts` and the behaviour of
//! `cli/ps-cli.ts` + `cli/ps-data.ts`: scope discovery (project hash dirs and
//! machine-global services), the persisted offline view when no broker is
//! alive, the plain table and `--json` shape, and the `info`/`logs`/`stop`/
//! `kill`/`restart` actions.
//!
//! Scope identity comes only from the native broker (`pi-agent` protocol):
//! `runtime_root`/`scope_key`/`physical_runtime_dir_with` hash dirs, the
//! 0600-authenticated socket client, and private `scope.json`/`meta.json`/
//! `broker.pid` files read through [`protocol::read_private_file`]. `--all`
//! additionally enumerates the private shortened base (`$TMPDIR/omp-broker-<uid>`)
//! for long logical roots, trusting only v1 checkpoints that name this root and
//! hash back to their directory. Listing never spawns a broker: a missing
//! broker is reported as the persisted, downgraded view instead of inventing a
//! second registry.
//!
//! Bounded gaps vs the TS command: the interactive alt-screen monitor is not
//! implemented (`omp ps` always prints the static listing, which is also what
//! upstream does without a TTY), and follow/non-follow logs print the broker's
//! text window rather than rendering terminal rows.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use pi_agent::process_broker::{
	BrokerClient,
	protocol::{self, LeaseRecord, PublicSpec, WireSpec},
};
use pi_agent::tools::process::{
	LogOptions, ProcessSnapshot, ProcessState, RestartPolicy, StartIdentity, is_terminal_state,
};
use serde::{Deserialize, Serialize};

use crate::dirs;

/// Actions accepted by `omp ps`.
pub const ACTIONS: &[&str] = &["list", "info", "logs", "stop", "kill", "restart"];

/// Hard SIGTERM->SIGKILL grace used by `kill`; effectively immediate.
const KILL_GRACE_MS: u64 = 100;
/// Default `stop` grace.
const STOP_GRACE_SECONDS: u64 = 5;
/// Per-request broker budget for log reads.
const LOG_REQUEST_TIMEOUT_MS: u64 = 30_000;
/// Maximum log window (and `--lines` cap) accepted by the broker.
const MAX_LOG_LINES: usize = 1_000;

const TABLE_HEADER: [&str; 7] = ["NAME", "STATE", "PID", "UPTIME", "RESTARTS", "FLAGS", "COMMAND"];

/// One CLI invocation.
pub struct PsRequest {
	pub action: String,
	pub name: Option<String>,
	pub all: bool,
	pub json: bool,
	/// Accepted for parity; the native CLI has no interactive monitor.
	pub plain: bool,
	/// Already resolved against `--cwd` by the caller.
	pub dir: Option<PathBuf>,
	pub global: Option<String>,
	pub follow: bool,
	pub head: bool,
	pub lines: Option<usize>,
	pub grep: Option<String>,
	pub timeout: Option<u64>,
}

/// Run one `omp ps` invocation, writing to `out`.
pub fn run(request: &PsRequest, out: &mut impl Write) -> Result<(), String> {
	let runtime = tokio::runtime::Builder::new_current_thread()
		.enable_all()
		.build()
		.map_err(|err| format!("cannot start the ps runtime: {err}"))?;
	runtime.block_on(run_async(request, out))
}

async fn run_async(request: &PsRequest, out: &mut impl Write) -> Result<(), String> {
	match request.action.as_str() {
		"list" => run_list(request, out).await,
		action @ ("info" | "logs" | "stop" | "kill" | "restart") => {
			let name = request
				.name
				.as_deref()
				.filter(|name| !name.is_empty())
				.ok_or_else(|| {
					format!("{action} requires a process name. Run `omp ps` to list processes.")
				})?;
			run_action(request, action, name, out).await
		},
		other => Err(format!(
			"Unknown ps action '{other}'. Expected: {}",
			ACTIONS.join(", ")
		)),
	}
}

// ---------------------------------------------------------------------------
// Scopes
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScopeKind {
	Project,
	Global,
}

impl ScopeKind {
	const fn as_str(self) -> &'static str {
		match self {
			Self::Project => "project",
			Self::Global => "global",
		}
	}
}

/// One broker scope: a project runtime dir or a machine-global service dir.
struct Scope {
	kind: ScopeKind,
	runtime_dir: PathBuf,
	project_dir: Option<PathBuf>,
	service: Option<String>,
	broker_pid: Option<u32>,
}

/// Persisted `{daemon, spec}` from `<runtime>/daemons/<name>/meta.json`.
#[derive(Deserialize)]
struct MetaRecord {
	daemon: ProcessSnapshot,
	spec: WireSpec,
}

/// One listed daemon.
struct DaemonRow {
	snapshot: ProcessSnapshot,
	command: Option<String>,
	cwd: Option<String>,
	supervised: bool,
}

fn global_runtime_root() -> PathBuf {
	dirs::base_config_root().join("run").join("daemons").join("global")
}

fn validate_service(service: &str) -> Result<(), String> {
	let mut chars = service.chars();
	let valid = chars.next().is_some_and(|first| first.is_ascii_alphanumeric())
		&& chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'));
	if valid {
		Ok(())
	} else {
		Err(format!("Invalid global daemon service name: {service:?}"))
	}
}

fn global_runtime_dir(service: &str) -> Result<PathBuf, String> {
	validate_service(service)?;
	Ok(global_runtime_root().join(service))
}

fn canonical_path(path: &Path) -> PathBuf {
	std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn project_runtime_dir(project: &Path) -> PathBuf {
	let root = protocol::runtime_root();
	protocol::physical_runtime_dir_with(&root, project)
		.unwrap_or_else(|_| protocol::runtime_dir_for_project(project))
}

/// Live broker PID for a scope, from the owner-only `broker.pid` lease.
fn live_broker_pid(runtime_dir: &Path) -> Option<u32> {
	let raw = protocol::read_private_file(&runtime_dir.join(protocol::PID_FILE)).ok()?;
	let lease: LeaseRecord = serde_json::from_str(&raw).ok()?;
	protocol::process_alive(lease.pid, lease.start_time).then_some(lease.pid)
}

/// The single scope selected by `--global`, `--dir`, or the current project.
fn target_scope(request: &PsRequest) -> Result<Scope, String> {
	if let Some(service) = &request.global {
		let runtime_dir = global_runtime_dir(service)?;
		let runtime_dir = canonical_path(&runtime_dir);
		return Ok(Scope {
			kind: ScopeKind::Global,
			broker_pid: live_broker_pid(&runtime_dir),
			runtime_dir,
			project_dir: None,
			service: Some(service.clone()),
		});
	}
	let dir = match &request.dir {
		Some(dir) => dir.clone(),
		None => std::env::current_dir().map_err(|err| err.to_string())?,
	};
	let project_dir = protocol::canonical_project_dir(&dir);
	let runtime_dir = project_runtime_dir(&project_dir);
	Ok(Scope {
		kind: ScopeKind::Project,
		broker_pid: live_broker_pid(&runtime_dir),
		runtime_dir,
		project_dir: Some(project_dir),
		service: None,
	})
}

/// Directory entries, or nothing when the directory does not exist. Any other
/// error (permissions, loop, not-a-directory) is reported instead of being
/// flattened into an empty machine, matching upstream `readdirQuiet`.
fn readdir_quiet(dir: &Path) -> Result<Vec<PathBuf>, String> {
	let unreadable = |err: std::io::Error| format!("cannot read {}: {err}", dir.display());
	let entries = match std::fs::read_dir(dir) {
		Ok(entries) => entries,
		Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
		Err(err) => return Err(unreadable(err)),
	};
	entries
		.map(|entry| entry.map(|entry| entry.path()).map_err(&unreadable))
		.collect()
}

fn is_project_scope_key(name: &str) -> bool {
	name.len() == 16 && name.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// Map a hash-keyed project runtime dir back to its project directory.
///
/// Only trusted when the recorded project hashes back to this exact runtime
/// directory (and, for v1 checkpoints, the recorded logical root matches), so
/// a stale or foreign record can never label another scope.
fn resolve_scope_project_dir(runtime_dir: &Path) -> Result<Option<PathBuf>, String> {
	let Some(name) = runtime_dir.file_name().and_then(|name| name.to_str()) else {
		return Ok(None);
	};
	if let Ok(record) =
		protocol::read_scope_record(&runtime_dir.join(protocol::SCOPE_FILE))
	{
		if let Some(project) = trusted_scope_project(&record, name) {
			return Ok(Some(project));
		}
	}
	// `omp ps` fallback for scopes written by clients that predate
	// `scope.json` (TS presence files record `projectDir`).
	for path in readdir_quiet(&runtime_dir.join(protocol::CLIENTS_DIR))? {
		if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
			continue;
		}
		let Ok(raw) = protocol::read_private_file(&path) else {
			continue;
		};
		let Ok(decoded) = serde_json::from_str::<serde_json::Value>(&raw) else {
			continue;
		};
		let Some(project) = decoded.get("projectDir").and_then(|value| value.as_str()) else {
			continue;
		};
		let project = protocol::canonical_project_dir(Path::new(project));
		if protocol::scope_key(&project) == name {
			return Ok(Some(project));
		}
	}
	Ok(None)
}

fn trusted_scope_project(record: &protocol::ScopeRecord, scope_name: &str) -> Option<PathBuf> {
	if record.project_dir.is_empty() {
		return None;
	}
	let project = protocol::canonical_project_dir(Path::new(&record.project_dir));
	if protocol::scope_key(&project) != scope_name {
		return None;
	}
	if record.version >= 1 && !record.logical_root.is_empty() {
		if canonical_path(Path::new(&record.logical_root)) != canonical_path(&protocol::runtime_root())
		{
			return None;
		}
	}
	Some(project)
}

/// Runtime dir name used for a shortened scope (mirrors
/// `protocol::short_runtime_dir`, which is create-on-demand).
fn shortened_scope_name(logical_root: &Path, project: &Path) -> String {
	protocol::scope_key(Path::new(&format!("{}\n{}", logical_root.display(), project.display())))
}

/// Existing shortened scopes for *this* logical runtime root.
///
/// Only the private `$TMPDIR/omp-broker-<uid>` base is scanned, and a scope is
/// trusted only when its v1 checkpoint (read through the private-file contract)
/// names this root and a project that hashes back to the directory name.
fn discover_shortened_scopes() -> Result<Vec<Scope>, String> {
	let logical_root = protocol::runtime_root();
	let Some(base) = protocol::short_runtime_base() else {
		return Ok(Vec::new());
	};
	let mut scopes = Vec::new();
	for path in readdir_quiet(&base)? {
		let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
			continue;
		};
		if !path.is_dir()
			|| !is_project_scope_key(name)
			|| protocol::verify_owner(&path).is_err()
		{
			continue;
		}
		let Ok(record) = protocol::read_scope_record(&path.join(protocol::SCOPE_FILE)) else {
			continue;
		};
		if record.version < protocol::SCOPE_VERSION || record.project_dir.is_empty() {
			continue;
		}
		if canonical_path(Path::new(&record.logical_root)) != canonical_path(&logical_root) {
			continue;
		}
		let project = protocol::canonical_project_dir(Path::new(&record.project_dir));
		if shortened_scope_name(&logical_root, &project) != name {
			continue;
		}
		scopes.push(Scope {
			kind: ScopeKind::Project,
			broker_pid: live_broker_pid(&path),
			project_dir: Some(project),
			runtime_dir: path,
			service: None,
		});
	}
	Ok(scopes)
}

/// One row per project: a live shortened broker replaces a dead logical
/// duplicate; a preserved logical scope is never doubled.
fn merge_shortened_scopes(scopes: &mut Vec<Scope>) -> Result<(), String> {
	for scope in discover_shortened_scopes()? {
		let Some(project) = scope.project_dir.clone() else {
			continue;
		};
		match scopes.iter().position(|existing| existing.project_dir.as_ref() == Some(&project)) {
			Some(index) => {
				if scopes[index].broker_pid.is_none() && scope.broker_pid.is_some() {
					scopes[index] = scope;
				}
			},
			None => scopes.push(scope),
		}
	}
	Ok(())
}

/// Every scope on this machine: hash-keyed project scopes, shortened scopes
/// for long logical roots, plus machine-global services.
fn discover_scopes() -> Result<Vec<Scope>, String> {
	let mut scopes = Vec::new();
	for path in readdir_quiet(&protocol::runtime_root())? {
		let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
			continue;
		};
		if !path.is_dir() || !is_project_scope_key(name) {
			continue;
		}
		scopes.push(Scope {
			kind: ScopeKind::Project,
			broker_pid: live_broker_pid(&path),
			project_dir: resolve_scope_project_dir(&path)?,
			runtime_dir: path,
			service: None,
		});
	}
	merge_shortened_scopes(&mut scopes)?;
	for path in readdir_quiet(&global_runtime_root())? {
		if !path.is_dir() {
			continue;
		}
		scopes.push(Scope {
			kind: ScopeKind::Global,
			broker_pid: live_broker_pid(&path),
			service: path.file_name().map(|name| name.to_string_lossy().into_owned()),
			project_dir: None,
			runtime_dir: canonical_path(&path),
		});
	}
	Ok(scopes)
}

// ---------------------------------------------------------------------------
// Snapshot collection
// ---------------------------------------------------------------------------

fn format_command(spec: &WireSpec) -> String {
	let mut command = spec.application.clone();
	for arg in &spec.args {
		command.push(' ');
		command.push_str(arg);
	}
	command
}

fn read_persisted_daemons(runtime_dir: &Path) -> Result<Vec<MetaRecord>, String> {
	let mut records = Vec::new();
	for path in readdir_quiet(&runtime_dir.join(protocol::DAEMONS_DIR))? {
		if !path.is_dir() {
			continue;
		}
		let Ok(raw) = protocol::read_private_file(&path.join(protocol::META_FILE)) else {
			continue;
		};
		if let Ok(record) = serde_json::from_str::<MetaRecord>(&raw) {
			records.push(record);
		}
	}
	Ok(records)
}

fn process_alive(pid: Option<u32>) -> bool {
	pid.is_some_and(|pid| protocol::process_alive(pid, None))
}

/// Whether a persisted non-terminal record is a live detached survivor after
/// its broker died. The snapshot pins `(pid, start token)`, so only an exact
/// kernel-process match counts and a recycled pid cannot fake a survivor.
/// TS-written `meta.json` has no identity: keep the conservative bare-pid
/// probe for it (upstream behaviour).
fn offline_survivor(detached: bool, daemon: &ProcessSnapshot) -> bool {
	detached
		&& daemon.state != ProcessState::Stopping
		&& daemon
			.start_identity
			.as_ref()
			.map_or_else(|| process_alive(daemon.pid), StartIdentity::still_matches)
}

/// Connect to a scope's broker without spawning one.
async fn existing_client(scope: &Scope) -> Option<std::sync::Arc<BrokerClient>> {
	let connect_dir = scope.project_dir.clone().unwrap_or_else(|| scope.runtime_dir.clone());
	BrokerClient::connect_existing(&connect_dir, &scope.runtime_dir).await.ok()
}

/// Collect daemons for one scope: a live broker is authoritative, a dead one
/// leaves the persisted snapshot downgraded exactly like `ps-data.ts`.
async fn collect_scope(mut scope: Scope) -> Result<ScopeReport, String> {
	let persisted = read_persisted_daemons(&scope.runtime_dir)?;
	if scope.broker_pid.is_some() {
		if let Some(client) = existing_client(&scope).await {
			// Only hash-keyed project scopes need the label; a global scope's
			// project dir is the service runtime dir itself (never reported).
			if scope.kind == ScopeKind::Project && scope.project_dir.is_none() {
				if let Ok(project) = client.ping().await {
					scope.project_dir = Some(protocol::canonical_project_dir(Path::new(&project)));
				}
			}
			if let Ok(daemons) = client.list(None).await {
				let rows = daemons
					.into_iter()
					.map(|snapshot| {
						let meta = persisted.iter().find(|meta| meta.daemon.name == snapshot.name);
						DaemonRow {
							command: meta.map(|meta| format_command(&meta.spec)),
							cwd: meta.map(|meta| meta.spec.cwd.clone()),
							supervised: true,
							snapshot,
						}
					})
					.collect();
				return Ok(ScopeReport { scope, daemons: rows });
			}
		}
	}
	let mut rows: Vec<DaemonRow> = persisted
		.into_iter()
		.map(|MetaRecord { mut daemon, spec }| {
			if !is_terminal_state(daemon.state) && !offline_survivor(spec.detached, &daemon) {
				daemon.state = ProcessState::Exited;
				daemon.exit_reason.get_or_insert_with(|| "broker exited".to_string());
			}
			DaemonRow {
				command: Some(format_command(&spec)),
				cwd: Some(spec.cwd),
				supervised: false,
				snapshot: daemon,
			}
		})
		.collect();
	rows.sort_by(compare_rows);
	Ok(ScopeReport { scope, daemons: rows })
}

fn compare_rows(a: &DaemonRow, b: &DaemonRow) -> std::cmp::Ordering {
	let a_terminal = is_terminal_state(a.snapshot.state);
	let b_terminal = is_terminal_state(b.snapshot.state);
	match (a_terminal, b_terminal) {
		(true, false) => std::cmp::Ordering::Greater,
		(false, true) => std::cmp::Ordering::Less,
		_ => a.snapshot.name.cmp(&b.snapshot.name),
	}
}

// ---------------------------------------------------------------------------
// Static list
// ---------------------------------------------------------------------------

async fn run_list(request: &PsRequest, out: &mut impl Write) -> Result<(), String> {
	let scopes = if request.all { discover_scopes()? } else { vec![target_scope(request)?] };
	let mut reports = Vec::with_capacity(scopes.len());
	for scope in scopes {
		reports.push(collect_scope(scope).await?);
	}
	if request.all {
		reports.retain(|report| !report.daemons.is_empty() || report.scope.broker_pid.is_some());
	}
	if request.json {
		let json = serde_json::to_string_pretty(&reports).map_err(|err| err.to_string())?;
		return writeln!(out, "{json}").map_err(|err| err.to_string());
	}
	if reports.is_empty() {
		return writeln!(out, "No daemon broker scopes found.").map_err(|err| err.to_string());
	}
	for (index, report) in reports.iter().enumerate() {
		if index > 0 {
			writeln!(out).map_err(|err| err.to_string())?;
		}
		writeln!(out, "{}", scope_header(&report.scope)).map_err(|err| err.to_string())?;
		if report.daemons.is_empty() {
			writeln!(out, "  no processes").map_err(|err| err.to_string())?;
			continue;
		}
		print_table(&report.daemons, out)?;
	}
	if !request.all {
		writeln!(out, "\nUse --all to include other projects and global services.")
			.map_err(|err| err.to_string())?;
	}
	Ok(())
}

/// stdout's column count when it is a terminal; `None` when piped, so full
/// lines stay intact (upstream gates on `process.stdout.isTTY`).
fn terminal_width() -> Option<usize> {
	if !std::io::stdout().is_terminal() {
		return None;
	}
	#[cfg(unix)]
	{
		let mut size = libc::winsize { ws_row: 0, ws_col: 0, ws_xpixel: 0, ws_ypixel: 0 };
		// SAFETY: `size` is a writable out-param and fd 1 is this process's stdout.
		if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) } == 0
			&& size.ws_col > 0
		{
			return Some(usize::from(size.ws_col));
		}
	}
	None
}

/// Clip a rendered line to `width` columns, marking the cut like upstream
/// `truncateToWidth` (one-cell `…`).
fn truncate_line(line: String, width: usize) -> String {
	if line.chars().count() <= width {
		return line;
	}
	if width == 0 {
		return String::new();
	}
	let mut clipped: String = line.chars().take(width - 1).collect();
	clipped.push('…');
	clipped
}

fn print_table(rows: &[DaemonRow], out: &mut impl Write) -> Result<(), String> {
	let cells: Vec<[String; 7]> = rows.iter().map(table_cells).collect();
	let mut widths = TABLE_HEADER.map(str::len);
	for row in &cells {
		for (column, cell) in row.iter().enumerate() {
			widths[column] = widths[column].max(cell.chars().count());
		}
	}
	let max_width = terminal_width();
	let render = |row: &[String; 7]| -> String {
		let mut line = String::from("  ");
		for (column, cell) in row.iter().enumerate() {
			if column > 0 {
				line.push_str("  ");
			}
			line.push_str(cell);
			for _ in cell.chars().count()..widths[column] {
				line.push(' ');
			}
		}
		let line = line.trim_end().to_string();
		match max_width {
			Some(width) => truncate_line(line, width),
			None => line,
		}
	};
	let header = TABLE_HEADER.map(str::to_string);
	writeln!(out, "{}", render(&header)).map_err(|err| err.to_string())?;
	for row in &cells {
		writeln!(out, "{}", render(row)).map_err(|err| err.to_string())?;
	}
	Ok(())
}

fn table_cells(row: &DaemonRow) -> [String; 7] {
	let snapshot = &row.snapshot;
	let terminal = is_terminal_state(snapshot.state);
	let mut state = snapshot.state.as_str().to_string();
	if terminal {
		if let Some(code) = snapshot.exit_code {
			state.push_str(&format!("({code})"));
		}
	}
	[
		snapshot.name.clone(),
		state,
		if !terminal {
			snapshot.pid.map(|pid| pid.to_string()).unwrap_or_else(|| "-".to_string())
		} else {
			"-".to_string()
		},
		uptime_cell(snapshot),
		snapshot.restart_count.to_string(),
		flags_cell(row),
		collapse_command(row.command.as_deref()),
	]
}

fn uptime_cell(snapshot: &ProcessSnapshot) -> String {
	if is_terminal_state(snapshot.state) {
		return "-".to_string();
	}
	format_duration(protocol::now_ms().saturating_sub(snapshot.started_at))
}

fn flags_cell(row: &DaemonRow) -> String {
	let mut parts: Vec<&str> = Vec::new();
	if row.snapshot.detached {
		parts.push("detached");
	} else if row.snapshot.persist {
		parts.push("persist");
	}
	if !row.supervised && !is_terminal_state(row.snapshot.state) {
		parts.push("unsupervised");
	}
	parts.join(",")
}

fn collapse_command(command: Option<&str>) -> String {
	command
		.map(|command| command.split_whitespace().collect::<Vec<_>>().join(" "))
		.unwrap_or_default()
}

fn scope_header(scope: &Scope) -> String {
	let label = match scope.kind {
		ScopeKind::Global => format!(
			"global {}",
			scope.service.as_deref().unwrap_or_else(|| scope
				.runtime_dir
				.file_name()
				.and_then(|name| name.to_str())
				.unwrap_or("global"))
		),
		ScopeKind::Project => format!(
			"project {}",
			scope
				.project_dir
				.as_ref()
				.map(|dir| dir.display().to_string())
				.unwrap_or_else(|| scope
					.runtime_dir
					.file_name()
					.and_then(|name| name.to_str())
					.unwrap_or("unknown")
					.to_string())
		),
	};
	let broker = match scope.broker_pid {
		Some(pid) => format!("broker pid {pid}"),
		None => "broker not running".to_string(),
	};
	format!("{label} — {broker}")
}

/// The upstream `DaemonSnapshot` fields, projected explicitly: the persisted
/// `startIdentity` token is machine-private and upstream JSON never carries it,
/// so no action may flatten the whole [`ProcessSnapshot`] into output.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SnapshotJson<'a> {
	name: &'a str,
	id: &'a str,
	state: ProcessState,
	#[serde(skip_serializing_if = "Option::is_none")]
	pid: Option<u32>,
	created_at: u64,
	started_at: u64,
	#[serde(skip_serializing_if = "Option::is_none")]
	ready_at: Option<u64>,
	#[serde(skip_serializing_if = "Option::is_none")]
	exited_at: Option<u64>,
	#[serde(skip_serializing_if = "Option::is_none")]
	exit_code: Option<i32>,
	#[serde(skip_serializing_if = "Option::is_none")]
	exit_reason: Option<&'a str>,
	restart_count: u32,
	output_bytes: u64,
	#[serde(skip_serializing_if = "Option::is_none")]
	owner: Option<&'a str>,
	#[serde(skip_serializing_if = "Option::is_none")]
	ready_match: Option<&'a str>,
	#[serde(skip_serializing_if = "Option::is_none")]
	ready_pending: Option<&'a [String]>,
	persist: bool,
	detached: bool,
}

impl<'a> From<&'a ProcessSnapshot> for SnapshotJson<'a> {
	fn from(snapshot: &'a ProcessSnapshot) -> Self {
		Self {
			name: &snapshot.name,
			id: &snapshot.id,
			state: snapshot.state,
			pid: snapshot.pid,
			created_at: snapshot.created_at,
			started_at: snapshot.started_at,
			ready_at: snapshot.ready_at,
			exited_at: snapshot.exited_at,
			exit_code: snapshot.exit_code,
			exit_reason: snapshot.exit_reason.as_deref(),
			restart_count: snapshot.restart_count,
			output_bytes: snapshot.output_bytes,
			owner: snapshot.owner.as_deref(),
			ready_match: snapshot.ready_match.as_deref(),
			ready_pending: snapshot.ready_pending.as_deref(),
			persist: snapshot.persist,
			detached: snapshot.detached,
		}
	}
}

/// One scope's report; `--json` emits the TS shape (snapshot first, then the
/// extra `command`/`cwd`/`supervised` columns).
struct ScopeReport {
	scope: Scope,
	daemons: Vec<DaemonRow>,
}

impl Serialize for ScopeReport {
	fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
		#[derive(Serialize)]
		#[serde(rename_all = "camelCase")]
		struct Row<'a> {
			#[serde(flatten)]
			snapshot: SnapshotJson<'a>,
			#[serde(skip_serializing_if = "Option::is_none")]
			command: &'a Option<String>,
			#[serde(skip_serializing_if = "Option::is_none")]
			cwd: &'a Option<String>,
			supervised: bool,
		}
		#[derive(Serialize)]
		#[serde(rename_all = "camelCase")]
		struct ScopeJson<'a> {
			kind: &'static str,
			#[serde(skip_serializing_if = "Option::is_none")]
			project_dir: &'a Option<PathBuf>,
			#[serde(skip_serializing_if = "Option::is_none")]
			service: &'a Option<String>,
			runtime_dir: &'a Path,
			#[serde(skip_serializing_if = "Option::is_none")]
			broker_pid: &'a Option<u32>,
			daemons: Vec<Row<'a>>,
		}
		let scope = &self.scope;
		ScopeJson {
			kind: scope.kind.as_str(),
			project_dir: &scope.project_dir,
			service: &scope.service,
			runtime_dir: &scope.runtime_dir,
			broker_pid: &scope.broker_pid,
			daemons: self
				.daemons
				.iter()
				.map(|row| Row {
					snapshot: SnapshotJson::from(&row.snapshot),
					command: &row.command,
					cwd: &row.cwd,
					supervised: row.supervised,
				})
				.collect(),
		}
		.serialize(serializer)
	}
}

// ---------------------------------------------------------------------------
// Named actions
// ---------------------------------------------------------------------------

async fn action_client(request: &PsRequest) -> Result<std::sync::Arc<BrokerClient>, String> {
	if let Some(service) = &request.global {
		let runtime_dir = global_runtime_dir(service)?;
		protocol::ensure_private_dir(&runtime_dir)
			.map_err(|err| format!("cannot prepare {}: {err}", runtime_dir.display()))?;
		let runtime_dir = canonical_path(&runtime_dir);
		return BrokerClient::connect_in(&runtime_dir, &runtime_dir).await;
	}
	let dir = match &request.dir {
		Some(dir) => dir.clone(),
		None => std::env::current_dir().map_err(|err| err.to_string())?,
	};
	BrokerClient::connect(&dir).await
}

async fn run_action(
	request: &PsRequest,
	action: &str,
	name: &str,
	out: &mut impl Write,
) -> Result<(), String> {
	let client = action_client(request).await?;
	match action {
		"info" => {
			let result = client.describe(name, None).await?;
			if request.json {
				let json = InfoJson {
					daemon: SnapshotJson::from(&result.daemon),
					spec: &result.spec,
				};
				let json = serde_json::to_string_pretty(&json).map_err(|err| err.to_string())?;
				return writeln!(out, "{json}").map_err(|err| err.to_string());
			}
			writeln!(out, "{}", daemon_label(&result.daemon)).map_err(|err| err.to_string())?;
			let spec = &result.spec;
			writeln!(out, "  command:  {}", format_command_public(spec))
				.map_err(|err| err.to_string())?;
			writeln!(out, "  cwd:      {}", spec.cwd).map_err(|err| err.to_string())?;
			if !is_terminal_state(result.daemon.state) {
				let uptime = protocol::now_ms().saturating_sub(result.daemon.started_at);
				writeln!(out, "  uptime:   {}", format_duration(uptime))
					.map_err(|err| err.to_string())?;
			}
			if let Some(reason) = &result.daemon.exit_reason {
				writeln!(out, "  exit:     {reason}").map_err(|err| err.to_string())?;
			}
			writeln!(
				out,
				"  restarts: {} (policy: {})",
				result.daemon.restart_count,
				restart_policy(spec.restart)
			)
			.map_err(|err| err.to_string())?;
			writeln!(
				out,
				"  pty: {}  persist: {}  detached: {}  owner: {}",
				spec.pty,
				spec.persist,
				spec.detached,
				result.daemon.owner.as_deref().unwrap_or("-")
			)
			.map_err(|err| err.to_string())?;
			Ok(())
		},
		"logs" => run_logs(request, &client, name, out).await,
		"stop" | "kill" => {
			let timeout_ms = if action == "kill" {
				KILL_GRACE_MS
			} else {
				request.timeout.unwrap_or(STOP_GRACE_SECONDS).saturating_mul(1_000)
			};
			let daemon = client.stop(name, timeout_ms, None).await?;
			print_daemon_result(request, if action == "kill" { "Killed" } else { "Stopped" }, &daemon, out)
		},
		"restart" => {
			let daemon = client.restart(name, None).await?;
			print_daemon_result(request, "Restarted", &daemon, out)
		},
		other => Err(format!("Unhandled action {other}")),
	}
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InfoJson<'a> {
	#[serde(flatten)]
	daemon: SnapshotJson<'a>,
	spec: &'a PublicSpec,
}

fn format_command_public(spec: &PublicSpec) -> String {
	let mut command = spec.application.clone();
	for arg in &spec.args {
		command.push(' ');
		command.push_str(arg);
	}
	command
}

fn restart_policy(policy: RestartPolicy) -> &'static str {
	match policy {
		RestartPolicy::No => "no",
		RestartPolicy::OnFailure => "on-failure",
		RestartPolicy::Always => "always",
	}
}

fn print_daemon_result(
	request: &PsRequest,
	verb: &str,
	daemon: &ProcessSnapshot,
	out: &mut impl Write,
) -> Result<(), String> {
	if request.json {
		let json = serde_json::to_string_pretty(&SnapshotJson::from(daemon))
			.map_err(|err| err.to_string())?;
		return writeln!(out, "{json}").map_err(|err| err.to_string());
	}
	writeln!(out, "{verb} {}", daemon_label(daemon)).map_err(|err| err.to_string())
}

fn daemon_label(daemon: &ProcessSnapshot) -> String {
	let pid = daemon.pid.map(|pid| format!(" pid={pid}")).unwrap_or_default();
	let exit = daemon.exit_code.map(|code| format!(" exit={code}")).unwrap_or_default();
	format!("{}: {}{pid}{exit}", daemon.name, daemon.state.as_str())
}

async fn run_logs(
	request: &PsRequest,
	client: &BrokerClient,
	name: &str,
	out: &mut impl Write,
) -> Result<(), String> {
	let lines = request.lines.unwrap_or(100).clamp(1, MAX_LOG_LINES);
	// Follow mode reads the full window so overlap trimming sees a stable
	// sliding tail; the first print is cut to `lines`.
	let first = client
		.logs(
			name,
			LogOptions {
				lines: if request.follow { MAX_LOG_LINES } else { lines },
				head: request.head,
				grep: request.grep.clone(),
				follow: false,
				cursor: None,
				timeout_ms: LOG_REQUEST_TIMEOUT_MS,
			},
			None,
		)
		.await?;
	if !request.follow {
		let text = first.text.strip_suffix('\n').unwrap_or(&first.text);
		if !text.is_empty() {
			writeln!(out, "{text}").map_err(|err| err.to_string())?;
		}
		return writeln!(out, "[{name}: {}]", first.state.as_str()).map_err(|err| err.to_string());
	}
	let window = first.text.strip_suffix('\n').unwrap_or(&first.text);
	let window_lines: Vec<&str> = window.split('\n').collect();
	let initial = window_lines[window_lines.len().saturating_sub(lines)..].join("\n");
	if !initial.is_empty() {
		writeln!(out, "{initial}").map_err(|err| err.to_string())?;
		out.flush().map_err(|err| err.to_string())?;
	}
	let mut previous = first.text;
	let mut cursor = first.cursor;
	let mut state = first.state;
	while !is_terminal_state(state) {
		let next = client
			.logs(
				name,
				LogOptions {
					lines: MAX_LOG_LINES,
					head: false,
					grep: request.grep.clone(),
					follow: true,
					cursor: Some(cursor),
					timeout_ms: LOG_REQUEST_TIMEOUT_MS,
				},
				None,
			)
			.await?;
		// The broker always returns the tail window (the cursor is only a wait
		// watermark), so trim the part already printed.
		let fresh = &next.text[overlap_length(&previous, &next.text)..];
		if !fresh.is_empty() {
			writeln!(out, "{}", fresh.strip_suffix('\n').unwrap_or(fresh))
				.map_err(|err| err.to_string())?;
			out.flush().map_err(|err| err.to_string())?;
		}
		previous = next.text;
		cursor = next.cursor;
		state = next.state;
	}
	writeln!(out, "[{name}: {}]", state.as_str()).map_err(|err| err.to_string())
}

/// Longest suffix of `previous` that is also a prefix of `next`.
fn overlap_length(previous: &str, next: &str) -> usize {
	for k in (1..=previous.len().min(next.len())).rev() {
		if next.is_char_boundary(k) && previous.ends_with(&next[..k]) {
			return k;
		}
	}
	0
}

fn format_duration(ms: u64) -> String {
	const SEC: u64 = 1_000;
	const MIN: u64 = 60 * SEC;
	const HOUR: u64 = 60 * MIN;
	const DAY: u64 = 24 * HOUR;
	if ms == 0 {
		return "0ms".to_string();
	}
	if ms < SEC {
		return format!("{ms}ms");
	}
	if ms < MIN {
		return format!("{:.1}s", ms as f64 / SEC as f64);
	}
	if ms < HOUR {
		let mins = ms / MIN;
		let secs = (ms % MIN) / SEC;
		return if secs > 0 { format!("{mins}m{secs}s") } else { format!("{mins}m") };
	}
	if ms < DAY {
		let hours = ms / HOUR;
		let mins = (ms % HOUR) / MIN;
		return if mins > 0 { format!("{hours}h{mins}m") } else { format!("{hours}h") };
	}
	let days = ms / DAY;
	let hours = (ms % DAY) / HOUR;
	if hours > 0 { format!("{days}d{hours}h") } else { format!("{days}d") }
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn overlap_trims_the_already_printed_tail() {
		assert_eq!(overlap_length("one\ntwo\n", "two\nthree\n"), 4);
		assert_eq!(overlap_length("abc", "xyz"), 0);
		assert_eq!(overlap_length("", "abc"), 0);
	}

	#[test]
	fn durations_match_the_ts_formatter() {
		assert_eq!(format_duration(0), "0ms");
		assert_eq!(format_duration(250), "250ms");
		assert_eq!(format_duration(1_500), "1.5s");
		assert_eq!(format_duration(65_000), "1m5s");
		assert_eq!(format_duration(3_600_000), "1h");
	}

	fn persisted(
		state: ProcessState,
		pid: Option<u32>,
		identity: Option<StartIdentity>,
	) -> ProcessSnapshot {
		ProcessSnapshot {
			name: "cam".to_string(),
			id: "cam-1".to_string(),
			state,
			pid,
			created_at: 1,
			started_at: 2,
			ready_at: None,
			exited_at: None,
			exit_code: None,
			exit_reason: None,
			restart_count: 0,
			output_bytes: 0,
			owner: None,
			ready_match: None,
			ready_pending: None,
			start_identity: identity,
			persist: true,
			detached: true,
		}
	}

	/// A recycled pid must not resurrect a dead detached daemon offline; only
	/// the pinned `(pid, token)` proves the survivor. Legacy records without an
	/// identity keep the bare-pid probe.
	#[test]
	fn stale_start_identity_is_not_an_offline_survivor() {
		let live = std::process::id();
		let stale = StartIdentity { pid: live, token: "not-the-real-token".to_string() };
		let running = || persisted(ProcessState::Running, Some(live), Some(stale.clone()));
		assert!(!offline_survivor(true, &running()), "recycled pid reported as survivor");
		assert!(
			offline_survivor(true, &persisted(ProcessState::Running, Some(live), None)),
			"identity-less record lost the conservative bare-pid probe"
		);
		assert!(!offline_survivor(true, &persisted(ProcessState::Running, None, None)));
		assert!(!offline_survivor(false, &persisted(ProcessState::Running, Some(live), None)));
	}

	/// `--json` on every action is the upstream snapshot shape; the persisted
	/// start-identity token is machine-private and must never be emitted.
	#[test]
	fn json_omits_the_internal_start_identity() {
		let daemon = persisted(
			ProcessState::Running,
			Some(42),
			Some(StartIdentity { pid: 42, token: "secret-token".to_string() }),
		);
		let report = ScopeReport {
			scope: Scope {
				kind: ScopeKind::Project,
				runtime_dir: PathBuf::from("/run/scope"),
				project_dir: None,
				service: None,
				broker_pid: None,
			},
			daemons: vec![DaemonRow {
				snapshot: daemon.clone(),
				command: Some("cam".to_string()),
				cwd: Some("/cam".to_string()),
				supervised: false,
			}],
		};
		let list = serde_json::to_value(&report).expect("list json");
		let row = &list["daemons"][0];
		assert!(row.get("startIdentity").is_none(), "identity leaked into list: {row}");
		assert_eq!(row["createdAt"], 1);
		assert_eq!(row["detached"].as_bool(), Some(true));
		assert_eq!(row["command"], "cam");
		assert_eq!(row["supervised"].as_bool(), Some(false));

		let spec = PublicSpec {
			name: "cam".to_string(),
			application: "cam".to_string(),
			args: Vec::new(),
			cwd: "/cam".to_string(),
			pty: false,
			ready: None,
			restart: RestartPolicy::No,
			persist: true,
			detached: true,
		};
		let info =
			serde_json::to_value(InfoJson { daemon: SnapshotJson::from(&daemon), spec: &spec })
				.expect("info json");
		assert!(info.get("startIdentity").is_none(), "identity leaked into info: {info}");
		assert_eq!(info["restartCount"], 0);
	}

	/// A missing directory is an empty scope; an unreadable one must fail the
	/// listing instead of pretending the machine has no brokers.
	#[test]
	fn readdir_quiet_only_treats_not_found_as_empty() {
		let base = std::env::temp_dir().join(format!("omp-ps-readdir-{}", std::process::id()));
		std::fs::create_dir_all(&base).expect("temp dir");
		assert!(
			readdir_quiet(&base.join("missing")).expect("not found is empty").is_empty(),
			"a missing directory is an empty scope"
		);
		let file = base.join("plain-file");
		std::fs::write(&file, b"x").expect("temp file");
		// ENOTDIR: a file in the path must surface, never read as an empty dir.
		let err = readdir_quiet(&file.join("child")).err().expect("error propagated");
		assert!(err.contains("plain-file"), "error names the offending path: {err}");
		let _ = std::fs::remove_dir_all(&base);
	}

	#[test]
	fn table_lines_are_clipped_to_the_terminal_width() {
		assert_eq!(truncate_line("abcdef".to_string(), 10), "abcdef");
		assert_eq!(truncate_line("abcdef".to_string(), 4), "abc…");
		assert_eq!(truncate_line("abcdef".to_string(), 0), "");
	}
}
