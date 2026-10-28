//! Wire protocol shared by the daemon broker worker and its clients.
//!
//! The TypeScript broker (`packages/coding-agent/src/launch/protocol.ts`) is the
//! contract anchor. One difference is deliberate: the request carries the
//! child's environment overlay in a dedicated field, and replies only ever
//! carry a redacted [`PublicSpec`]. `ProcessSpec` serialization skips `env`
//! (it cannot round-trip secrets), so it is never used as a transport type.
//!
//! Security invariants:
//! - every request is authenticated with the 64-hex token from a 0600 file;
//! - requests are newline-delimited JSON bounded by [`MAX_REQUEST_BYTES`];
//! - environment values are transported only over the authenticated private
//!   socket (and stored in a 0600 `meta.json`), never returned or logged;
//! - Unix socket paths are length-checked for Android/Linux `sun_path`.

use std::collections::{BTreeMap, HashMap};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::tools::process::{ProcessSnapshot, ProcessSpec, ReadySpec, RestartPolicy, Signal};

/// Hidden CLI selector that re-enters the broker worker (`__omp_worker_daemon_broker`).
pub const WORKER_ARG: &str = "__omp_worker_daemon_broker";
/// Canonical project (or synthetic global scope) directory for the worker.
pub const PROJECT_DIR_ENV: &str = "OMP_DAEMON_PROJECT_DIR";
/// Private broker runtime directory for the worker.
pub const RUNTIME_DIR_ENV: &str = "OMP_DAEMON_RUNTIME_DIR";
/// Optional idle-grace override read (and deleted) by the worker.
pub const IDLE_GRACE_ENV: &str = "OMP_DAEMON_IDLE_GRACE_MS";
/// Exact profile-aware runtime root (`<agent dir>/run/daemons`) exported by the
/// CLI after profile resolution so the broker client in `pi-agent` and the CLI
/// agree byte-for-byte, including profile-name normalization.
pub const RUNTIME_ROOT_ENV: &str = "OMP_DAEMON_RUNTIME_ROOT";

pub const TOKEN_FILE: &str = "broker.token";
pub const PID_FILE: &str = "broker.pid";
pub const SOCKET_FILE: &str = "broker.sock";
pub const SCOPE_FILE: &str = "scope.json";
/// Exclusive flock used only while creating/validating `scope.json`.
pub const SCOPE_LOCK_FILE: &str = "scope.lock";
pub const META_FILE: &str = "meta.json";
pub const DAEMONS_DIR: &str = "daemons";
pub const CLIENTS_DIR: &str = "clients";

/// Hard request envelope limit (TypeScript `MAX_REQUEST_BYTES`).
pub const MAX_REQUEST_BYTES: usize = 1024 * 1024;
/// Android/Linux `sun_path` is 108 bytes including the NUL terminator.
pub const MAX_SOCKET_PATH_BYTES: usize = 104;
pub const DEFAULT_IDLE_GRACE_MS: u64 = 3_000;
pub const DAEMON_PTY_COLUMNS: u16 = 120;
pub const DAEMON_PTY_ROWS: u16 = 40;
/// Maximum size accepted for one `meta.json` recovery file.
pub const MAX_META_BYTES: u64 = 4 * 1024 * 1024;

/// Profile-scoped runtime root (`<agent dir>/run/daemons`).
///
/// Mirrors `omp-cli::dirs`/`default_log_root` without depending on the CLI
/// crate: `PI_CONFIG_DIR` else `~/.omp`, a named profile relocated to
/// `<base>/profiles/<profile>/agent`, `PI_CODING_AGENT_DIR` override in default
/// mode. Never ambient `/tmp`.
#[must_use]
pub fn runtime_root() -> PathBuf {
	if let Some(exact) = std::env::var_os(RUNTIME_ROOT_ENV)
		.map(PathBuf::from)
		.filter(|path| !path.as_os_str().is_empty())
	{
		return exact;
	}
	let base = std::env::var_os("PI_CONFIG_DIR")
		.map(PathBuf::from)
		.filter(|path| !path.as_os_str().is_empty())
		.unwrap_or_else(|| {
			// Mirrors `omp-cli::dirs::base_config_root`: `$PI_CONFIG_DIR` else
			// `~/.omp` (never a bare relative `agent` when HOME is missing).
			std::env::var_os("HOME")
				.or_else(|| std::env::var_os("USERPROFILE"))
				.map(PathBuf::from)
				.filter(|path| !path.as_os_str().is_empty())
				.unwrap_or_else(|| PathBuf::from("."))
				.join(".omp")
		});
	let profile = ["OMP_PROFILE", "PI_PROFILE"]
		.iter()
		.find_map(|key| std::env::var(key).ok())
		.filter(|value| !value.is_empty());
	let agent = match profile {
		Some(profile) => base.join("profiles").join(profile).join("agent"),
		None => std::env::var_os("PI_CODING_AGENT_DIR")
			.map(PathBuf::from)
			.filter(|path| !path.as_os_str().is_empty())
			.unwrap_or_else(|| base.join("agent")),
	};
	agent.join("run").join(DAEMONS_DIR)
}

/// Canonicalize like the TS client (`realpath`, unresolved fallback).
#[must_use]
pub fn canonical_project_dir(project_dir: &Path) -> PathBuf {
	let resolved = if project_dir.is_absolute() {
		project_dir.to_path_buf()
	} else {
		std::env::current_dir().unwrap_or_default().join(project_dir)
	};
	std::fs::canonicalize(&resolved).unwrap_or(resolved)
}

/// Project-scoped runtime directory keyed by a stable 16-hex hash.
#[must_use]
pub fn runtime_dir_for_project(project_dir: &Path) -> PathBuf {
	runtime_root().join(scope_key(project_dir))
}

/// Broker scope checkpoint stored next to the token. `version` is
/// serde-defaulted so an old file (project only) reads as legacy.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScopeRecord {
	#[serde(default)]
	pub version: u32,
	#[serde(default)]
	pub logical_root: String,
	#[serde(default)]
	pub root_token: String,
	#[serde(default)]
	pub project_dir: String,
}

pub const SCOPE_VERSION: u32 = 1;

/// Physical runtime directory for `project` under the configured logical root,
/// shortened into a private per-user temp dir when the socket path would not
/// fit `sun_path`. The logical root remains the identity; the short directory
/// is only where the socket/metadata live.
pub fn physical_runtime_dir(project_dir: &Path) -> Result<PathBuf, String> {
	physical_runtime_dir_with(&runtime_root(), project_dir)
}

/// Testable form of [`physical_runtime_dir`] with an explicit logical root.
pub fn physical_runtime_dir_with(
	logical_root: &Path,
	project_dir: &Path,
) -> Result<PathBuf, String> {
	let project = canonical_project_dir(project_dir);
	let logical = logical_root.join(scope_key(&project));
	match endpoint(&logical) {
		Ok(_) => Ok(logical),
		Err(_) => short_runtime_dir(logical_root, &project),
	}
}

fn short_runtime_base() -> Option<PathBuf> {
	let temp = std::env::var_os("TMPDIR")
		.map(PathBuf::from)
		.filter(|path| !path.as_os_str().is_empty())
		.unwrap_or_else(std::env::temp_dir);
	temp.is_absolute().then(|| temp.join(format!("omp-broker-{}", current_uid())))
}

/// Short physical runtime dir: `$TMPDIR/omp-broker-<uid>/<hash>` (0700).
///
/// The hash covers the full logical root plus the canonical project, so two
/// profiles/trees with the same project never share a broker scope.
pub fn short_runtime_dir(logical_root: &Path, project_dir: &Path) -> Result<PathBuf, String> {
	let project = canonical_project_dir(project_dir);
	let base = short_runtime_base().ok_or_else(|| "cannot resolve a private temp dir".to_string())?;
	ensure_private_dir(&base).map_err(|err| {
		format!("cannot prepare private broker runtime base {}: {err}", base.display())
	})?;
	let key = scope_key(Path::new(&format!(
		"{}\n{}",
		logical_root.display(),
		project.display()
	)));
	let dir = base.join(key);
	ensure_private_dir(&dir)
		.map_err(|err| format!("cannot prepare private broker runtime {}: {err}", dir.display()))?;
	endpoint(&dir)?;
	Ok(dir)
}

/// Read, create, or validate the scope checkpoint for a short runtime dir.
///
/// A mismatching logical root/project is a hard error: the hash should make
/// that impossible, and continuing would point the worker at the wrong scope.
pub fn ensure_scope(
	runtime_dir: &Path,
	logical_root: &Path,
	project_dir: &Path,
) -> io::Result<()> {
	let project = canonical_project_dir(project_dir);
	let logical = logical_root.display().to_string();
	let path = runtime_dir.join(SCOPE_FILE);
	// Serialize first-client creation. Without this, two clients can both
	// observe NotFound, and a partially written scope.json makes one of them
	// fall back to a split local table.
	let _lock = LeaseLock::acquire(&runtime_dir.join(SCOPE_LOCK_FILE))?;
	match read_scope_record(&path) {
		Ok(record) => {
			validate_scope_v1(&record, &logical, &project, &path)?;
			Ok(())
		},
		Err(err) if err.kind() == io::ErrorKind::NotFound => {
			let record = ScopeRecord {
				version: SCOPE_VERSION,
				logical_root: logical.clone(),
				root_token: random_hex(32)?,
				project_dir: project.display().to_string(),
			};
			let text = serde_json::to_vec(&record)
				.map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
			// Publish atomically under the lock: readers see a complete v1 file.
			let suffix = random_hex(4)?;
			let temporary = path.with_file_name(format!(
				"scope.{}.{suffix}.tmp",
				std::process::id()
			));
			let result = write_private_new(&temporary, &text)
				.and_then(|()| std::fs::rename(&temporary, &path));
			if result.is_err() {
				let _ = std::fs::remove_file(&temporary);
			}
			result
		},
		Err(err) => Err(err),
	}
}

fn validate_scope_v1(
	record: &ScopeRecord,
	logical: &str,
	project: &Path,
	path: &Path,
) -> io::Result<()> {
	if record.version != SCOPE_VERSION {
		return Err(io::Error::new(
			io::ErrorKind::InvalidData,
			format!("{} has unsupported scope version {}", path.display(), record.version),
		));
	}
	if record.logical_root != logical || record.project_dir != project.display().to_string() {
		return Err(io::Error::new(
			io::ErrorKind::PermissionDenied,
			format!("{} belongs to a different logical broker scope", path.display()),
		));
	}
	if !is_hex_token(&record.root_token) {
		return Err(io::Error::new(
			io::ErrorKind::InvalidData,
			format!("{} has an invalid root token", path.display()),
		));
	}
	Ok(())
}

/// Validate an existing scope checkpoint before the worker binds.
///
/// A shortened physical directory MUST carry an exact v1 checkpoint; legacy
/// v0/missing files are only accepted when the physical dir is the logical
/// runtime dir itself (normal-root brokers predating v1).
pub fn verify_scope(
	runtime_dir: &Path,
	logical_root: &Path,
	project_dir: &Path,
) -> io::Result<()> {
	let project = canonical_project_dir(project_dir);
	let logical = logical_root.display().to_string();
	// Only our short-root base requires a checkpoint; explicit test/worker
	// runtime dirs outside it keep the legacy no-scope behavior.
	let shortened = short_runtime_base()
		.is_some_and(|base| runtime_dir.starts_with(base));
	let path = runtime_dir.join(SCOPE_FILE);
	match read_scope_record(&path) {
		Err(err) if err.kind() == io::ErrorKind::NotFound => {
			if shortened {
				Err(io::Error::new(
					io::ErrorKind::PermissionDenied,
					format!("shortened broker scope {} has no checkpoint", runtime_dir.display()),
				))
			} else {
				Ok(())
			}
		},
		Err(err) => Err(err),
		Ok(record) if record.version == 0 => {
			if shortened {
				return Err(io::Error::new(
					io::ErrorKind::InvalidData,
					format!("shortened broker scope {} has a legacy checkpoint", runtime_dir.display()),
				));
			}
			// Older normal-root checkpoints may omit projectDir, but a present
			// one must still match the caller's project.
			if !record.project_dir.is_empty()
				&& canonical_project_dir(Path::new(&record.project_dir)) != project
			{
				return Err(io::Error::new(
					io::ErrorKind::PermissionDenied,
					format!("{} belongs to a different project", path.display()),
				));
			}
			Ok(())
		},
		Ok(record) => validate_scope_v1(&record, &logical, &project, &path),
	}
}

/// Parse a private scope checkpoint.
pub fn read_scope_record(path: &Path) -> io::Result<ScopeRecord> {
	let text = read_private_file(path)?;
	serde_json::from_str(&text)
		.map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

fn is_hex_token(token: &str) -> bool {
	token.len() == 64 && token.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Cross-process exclusive lock (`flock`): kernel-released on crash, no stale
/// lock file, and works on Termux where `hard_link` can fail with EACCES.
pub(crate) struct LeaseLock {
	file: std::fs::File,
}

impl LeaseLock {
	pub(crate) fn acquire(path: &Path) -> io::Result<Self> {
		use std::os::unix::fs::OpenOptionsExt;
		use std::os::unix::io::AsRawFd;
		let file = std::fs::OpenOptions::new()
			.read(true)
			.write(true)
			.create(true)
			.mode(0o600)
			.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
			.open(path)?;
		verify_owner(path)?;
		// SAFETY: `file` owns a valid fd for the lifetime of `Self`; flock has
		// no memory-safety preconditions.
		if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
			return Err(io::Error::last_os_error());
		}
		Ok(Self { file })
	}
}

impl Drop for LeaseLock {
	fn drop(&mut self) {
		use std::os::unix::io::AsRawFd;
		// SAFETY: the fd is valid until `file` drops at the end of this scope.
		unsafe {
			libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
		}
	}
}

/// Deterministic FNV-1a 64-bit scope key (short path component on Android).
#[must_use]
pub fn scope_key(project_dir: &Path) -> String {
	let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
	for byte in project_dir.to_string_lossy().as_bytes() {
		hash ^= u64::from(*byte);
		hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
	}
	format!("{hash:016x}")
}

/// `runtime_dir/broker.sock`, rejecting paths that cannot fit `sun_path`.
pub fn endpoint(runtime_dir: &Path) -> Result<PathBuf, String> {
	let path = runtime_dir.join(SOCKET_FILE);
	#[cfg(unix)]
	{
		use std::os::unix::ffi::OsStrExt;
		let len = path.as_os_str().as_bytes().len();
		if len > MAX_SOCKET_PATH_BYTES {
			return Err(format!(
				"daemon broker socket path is too long ({len} bytes, max {MAX_SOCKET_PATH_BYTES}): {}. \
				 Set a shorter PI_CONFIG_DIR or use a named profile.",
				path.display()
			));
		}
	}
	Ok(path)
}

#[must_use]
pub fn now_ms() -> u64 {
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|duration| duration.as_millis() as u64)
		.unwrap_or_default()
}

/// Random lowercase hex from `/dev/urandom` (token, ids, instance id).
pub fn random_hex(bytes: usize) -> io::Result<String> {
	let mut buffer = vec![0u8; bytes];
	std::fs::File::open("/dev/urandom")?.read_exact(&mut buffer)?;
	let mut text = String::with_capacity(bytes * 2);
	for byte in buffer {
		use std::fmt::Write as _;
		let _ = write!(text, "{byte:02x}");
	}
	Ok(text)
}

/// Current effective uid (Unix) for directory/file ownership checks.
#[cfg(unix)]
#[must_use]
pub fn current_uid() -> u32 {
	// SAFETY: geteuid has no preconditions and cannot fail.
	unsafe { libc::geteuid() }
}

#[cfg(not(unix))]
#[must_use]
pub const fn current_uid() -> u32 {
	0
}

/// Reject files/dirs owned by another uid (no symlink following).
pub fn verify_owner(path: &Path) -> io::Result<()> {
	#[cfg(unix)]
	{
		use std::os::unix::fs::MetadataExt;
		let metadata = std::fs::symlink_metadata(path)?;
		if metadata.uid() != current_uid() {
			return Err(io::Error::new(
				io::ErrorKind::PermissionDenied,
				format!("{} is owned by uid {} not the current user", path.display(), metadata.uid()),
			));
		}
	}
	Ok(())
}

/// Create (or verify) a private 0700 directory owned by the current user.
pub fn ensure_private_dir(dir: &Path) -> io::Result<()> {
	if dir.exists() {
		let metadata = std::fs::symlink_metadata(dir)?;
		if !metadata.is_dir() {
			return Err(io::Error::new(
				io::ErrorKind::InvalidInput,
				format!("{} is not a directory", dir.display()),
			));
		}
		verify_owner(dir)?;
		#[cfg(unix)]
		{
			use std::os::unix::fs::PermissionsExt;
			if metadata.permissions().mode() & 0o077 != 0 {
				std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
			}
		}
		return Ok(());
	}
	let mut builder = std::fs::DirBuilder::new();
	builder.recursive(true);
	#[cfg(unix)]
	{
		use std::os::unix::fs::DirBuilderExt;
		builder.mode(0o700);
	}
	builder.create(dir)?;
	verify_owner(dir)
}

fn private_open(path: &Path, write: bool, create_new: bool) -> io::Result<std::fs::File> {
	let mut options = std::fs::OpenOptions::new();
	options.read(!write).write(write).create_new(create_new);
	#[cfg(unix)]
	{
		use std::os::unix::fs::OpenOptionsExt;
		options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
	}
	options.open(path)
}

fn read_private(path: &Path) -> io::Result<String> {
	let mut file = private_open(path, false, false)?;
	verify_owner(path)?;
	let mut text = String::new();
	file.read_to_string(&mut text)?;
	Ok(text)
}

/// Read a private file after an explicit regular-file + size check.
pub fn read_private_file(path: &Path) -> io::Result<String> {
	let metadata = std::fs::symlink_metadata(path)?;
	if !metadata.is_file() {
		return Err(io::Error::new(
			io::ErrorKind::InvalidInput,
			format!("{} is not a regular file", path.display()),
		));
	}
	if metadata.len() > MAX_META_BYTES {
		return Err(io::Error::new(
			io::ErrorKind::InvalidData,
			format!("{} exceeds the {MAX_META_BYTES}-byte limit", path.display()),
		));
	}
	#[cfg(unix)]
	{
		use std::os::unix::fs::PermissionsExt;
		let mode = metadata.permissions().mode();
		if mode & 0o077 != 0 {
			return Err(io::Error::new(
				io::ErrorKind::PermissionDenied,
				format!("{} is not private (mode {:o})", path.display(), mode & 0o777),
			));
		}
	}
	read_private(path)
}

/// Write a new 0600 file exactly once (`O_EXCL`, no symlinks), fsynced.
pub fn write_private_new(path: &Path, bytes: &[u8]) -> io::Result<()> {
	let mut file = private_open(path, true, true)?;
	file.write_all(bytes)?;
	file.sync_all()
}

/// Verify a broker socket is owned by this user and inaccessible to others.
pub fn verify_socket(path: &Path) -> io::Result<()> {
	verify_owner(path)?;
	#[cfg(unix)]
	{
		use std::os::unix::fs::PermissionsExt;
		let mode = std::fs::symlink_metadata(path)?.permissions().mode();
		if mode & 0o077 != 0 {
			return Err(io::Error::new(
				io::ErrorKind::PermissionDenied,
				format!("{} is accessible by other users (mode {:o})", path.display(), mode & 0o777),
			));
		}
	}
	Ok(())
}

/// Read-or-create the 64-hex bearer token (0600, `O_EXCL`, no symlinks).
pub fn ensure_token(runtime_dir: &Path) -> io::Result<String> {
	ensure_private_dir(runtime_dir)?;
	let path = runtime_dir.join(TOKEN_FILE);
	for _ in 0..100 {
		match read_private(&path) {
			Ok(raw) => {
				let token = raw.trim();
				if !token.is_empty() {
					#[cfg(unix)]
					{
						use std::os::unix::fs::PermissionsExt;
						// An older broker may have left a wider mode; the token is a
						// bearer credential, so always tighten it back to 0600.
						std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
					}
					return Ok(token.to_string());
				}
			},
			Err(err) if err.kind() == io::ErrorKind::NotFound => {},
			Err(err) => return Err(err),
		}
		match private_open(&path, true, true) {
			Ok(mut file) => {
				let token = random_hex(32)?;
				file.write_all(token.as_bytes())?;
				file.sync_all()?;
				return Ok(token);
			},
			Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {},
			Err(err) => return Err(err),
		}
		std::thread::sleep(Duration::from_millis(10));
	}
	Err(io::Error::new(
		io::ErrorKind::TimedOut,
		format!("timed out initializing daemon broker token in {}", runtime_dir.display()),
	))
}

/// Constant-time token comparison.
#[must_use]
pub fn token_matches(expected: &str, provided: &str) -> bool {
	let expected = expected.as_bytes();
	let provided = provided.as_bytes();
	if expected.len() != provided.len() {
		return false;
	}
	let mut difference = 0u8;
	for (left, right) in expected.iter().zip(provided) {
		difference |= left ^ right;
	}
	difference == 0
}

/// Kernel start time (`/proc/<pid>/stat` field 22) used as safe process identity.
#[must_use]
pub fn process_start_time(pid: u32) -> Option<u64> {
	#[cfg(any(target_os = "linux", target_os = "android"))]
	{
		let raw = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
		let rest = raw.get(raw.rfind(')')? + 2..)?;
		rest.split_whitespace().nth(19)?.parse().ok()
	}
	#[cfg(not(any(target_os = "linux", target_os = "android")))]
	{
		let _ = pid;
		None
	}
}

/// Live check pinned to the recorded start time, never a bare PID probe.
#[must_use]
pub fn process_alive(pid: u32, start_time: Option<u64>) -> bool {
	if let Some(expected) = start_time
		&& process_start_time(pid) != Some(expected)
	{
		return false;
	}
	pi_shell::process::Process::from_pid(pid as i32)
		.is_some_and(|process| process.status() == pi_shell::process::ProcessStatus::Running)
}

/// Lease contents (`broker.pid`), written with `O_EXCL` 0600.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LeaseRecord {
	pub pid: u32,
	#[serde(default)]
	pub start_time: Option<u64>,
	#[serde(default)]
	pub instance_id: String,
}

/// Startup spec transported on the authenticated socket.
///
/// `env` is the child's requested environment overlay; the broker resolves it
/// against its own inherited environment before calling the process manager.
/// Deliberately not `Debug`: it carries environment values.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireSpec {
	pub name: String,
	pub application: String,
	#[serde(default)]
	pub args: Vec<String>,
	#[serde(default)]
	pub env: BTreeMap<String, String>,
	/// When set, `env` is the complete child environment and is used verbatim
	/// (never merged with the broker's ambient environment). Clients built from
	/// a [`ProcessSpec`] always set this; hand-written overlay specs may not.
	#[serde(default)]
	pub full_env: bool,
	pub cwd: String,
	#[serde(default)]
	pub pty: bool,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub ready: Option<ReadySpec>,
	#[serde(default)]
	pub restart: RestartPolicy,
	#[serde(default)]
	pub persist: bool,
	#[serde(default)]
	pub detached: bool,
}

impl WireSpec {
	/// Apply the daemon contract that a detached child is persistent.
	#[must_use]
	pub fn normalized(mut self) -> Self {
		if self.detached {
			self.persist = true;
		}
		self
	}

	/// Resolve the child environment.
	///
	/// `full_env = true` (every [`ProcessSpec`]-derived launch and every recovery
	/// relaunch) uses the transported environment verbatim, so a broker's own
	/// ambient variables never leak into a persisted or re-adopted child.
	/// Hand-written overlay specs merge onto the broker's ambient environment.
	#[must_use]
	pub fn into_process_spec(&self) -> ProcessSpec {
		let env: HashMap<String, String> = if self.full_env {
			self.env.iter().map(|(key, value)| (key.clone(), value.clone())).collect()
		} else {
			let mut env: HashMap<String, String> = std::env::vars_os()
				.filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
				.collect();
			for (key, value) in &self.env {
				env.insert(key.clone(), value.clone());
			}
			env
		};
		ProcessSpec {
			name: self.name.clone(),
			application: self.application.clone(),
			args: self.args.clone(),
			env,
			cwd: self.cwd.clone(),
			pty: self.pty,
			ready: self.ready.clone(),
			restart: self.restart,
			persist: self.persist,
			detached: self.detached,
		}
	}

	/// Recovery spec: the persisted full environment is authoritative.
	#[must_use]
	pub fn from_process_spec(spec: &ProcessSpec) -> Self {
		Self {
			name: spec.name.clone(),
			application: spec.application.clone(),
			args: spec.args.clone(),
			env: spec.env.iter().map(|(key, value)| (key.clone(), value.clone())).collect(),
			// The hub hands us the complete child environment.
			full_env: true,
			cwd: spec.cwd.clone(),
			pty: spec.pty,
			ready: spec.ready.clone(),
			restart: spec.restart,
			persist: spec.persist,
			detached: spec.detached,
		}
	}

	/// Redacted public view (`env` never echoed).
	#[must_use]
	pub fn public(&self) -> PublicSpec {
		PublicSpec {
			name: self.name.clone(),
			application: self.application.clone(),
			args: self.args.clone(),
			cwd: self.cwd.clone(),
			pty: self.pty,
			ready: self.ready.clone(),
			restart: self.restart,
			persist: self.persist,
			detached: self.detached,
		}
	}
}

impl From<&ProcessSpec> for PublicSpec {
	fn from(spec: &ProcessSpec) -> Self {
		Self {
			name: spec.name.clone(),
			application: spec.application.clone(),
			args: spec.args.clone(),
			cwd: spec.cwd.clone(),
			pty: spec.pty,
			ready: spec.ready.clone(),
			restart: spec.restart,
			persist: spec.persist,
			detached: spec.detached,
		}
	}
}

/// Immutable launch spec safe to echo to clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicSpec {
	pub name: String,
	pub application: String,
	#[serde(default)]
	pub args: Vec<String>,
	pub cwd: String,
	#[serde(default)]
	pub pty: bool,
	#[serde(default)]
	pub ready: Option<ReadySpec>,
	#[serde(default)]
	pub restart: RestartPolicy,
	#[serde(default)]
	pub persist: bool,
	#[serde(default)]
	pub detached: bool,
}

/// Wait target (`"ready"` or `"exit"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WaitTarget {
	Ready,
	Exit,
}

/// Typed broker operation (TypeScript `DaemonOperation` plus an owner per call).
///
/// Not `Debug`: `Start` embeds the private [`WireSpec`].
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum Operation {
	Ping,
	Start {
		spec: WireSpec,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		owner: Option<String>,
	},
	List {
		#[serde(default, skip_serializing_if = "Option::is_none")]
		owner: Option<String>,
	},
	Logs {
		name: String,
		#[serde(default = "default_log_lines")]
		lines: usize,
		#[serde(default)]
		head: bool,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		grep: Option<String>,
		#[serde(default)]
		follow: bool,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		cursor: Option<u64>,
		#[serde(rename = "timeoutMs", default = "default_log_timeout")]
		timeout_ms: u64,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		owner: Option<String>,
	},
	Wait {
		name: String,
		#[serde(rename = "for")]
		target: WaitTarget,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		pattern: Option<String>,
		#[serde(rename = "timeoutMs", default = "default_log_timeout")]
		timeout_ms: u64,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		owner: Option<String>,
	},
	Send {
		name: String,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		data: Option<String>,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		signal: Option<Signal>,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		owner: Option<String>,
	},
	Stop {
		name: String,
		#[serde(rename = "timeoutMs", default = "default_stop_timeout")]
		timeout_ms: u64,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		owner: Option<String>,
	},
	Restart {
		name: String,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		owner: Option<String>,
	},
	/// Arm the backend's background deadline for an async job.
	Timeout {
		name: String,
		#[serde(rename = "timeoutMs")]
		timeout_ms: u64,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		owner: Option<String>,
	},
	Describe {
		name: String,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		owner: Option<String>,
	},
	Shutdown,
}

const fn default_log_lines() -> usize {
	1_000
}

const fn default_log_timeout() -> u64 {
	30_000
}

const fn default_stop_timeout() -> u64 {
	2_000
}

impl Operation {
	#[must_use]
	pub fn owner(&self) -> Option<&str> {
		match self {
			Self::Start { owner, .. }
			| Self::List { owner }
			| Self::Logs { owner, .. }
			| Self::Wait { owner, .. }
			| Self::Send { owner, .. }
			| Self::Stop { owner, .. }
			| Self::Restart { owner, .. }
			| Self::Timeout { owner, .. }
			| Self::Describe { owner, .. } => owner.as_deref(),
			Self::Ping | Self::Shutdown => None,
		}
	}

	#[must_use]
	pub fn name(&self) -> Option<&str> {
		match self {
			Self::Logs { name, .. }
			| Self::Wait { name, .. }
			| Self::Send { name, .. }
			| Self::Stop { name, .. }
			| Self::Restart { name, .. }
			| Self::Timeout { name, .. }
			| Self::Describe { name, .. } => Some(name),
			Self::Start { spec, .. } => Some(&spec.name),
			Self::Ping | Self::List { .. } | Self::Shutdown => None,
		}
	}

	/// Client-side request deadline: operation timeout plus the transport grace.
	#[must_use]
	pub fn timeout(&self) -> Duration {
		let timeout_ms = match self {
			Self::Start { spec, .. } => {
				spec.ready.as_ref().map_or(15_000, |ready| ready.timeout_ms) + 5_000
			},
			Self::Logs { timeout_ms, .. }
			| Self::Wait { timeout_ms, .. }
			| Self::Stop { timeout_ms, .. }
			| Self::Timeout { timeout_ms, .. } => timeout_ms.saturating_add(5_000),
			_ => 30_000,
		};
		Duration::from_millis(timeout_ms)
	}

	#[must_use]
	pub const fn is_shutdown(&self) -> bool {
		matches!(self, Self::Shutdown)
	}
}

/// Authenticated request envelope.
///
/// Deliberately not `Debug`: it carries the bearer token.
#[derive(Clone, Deserialize, Serialize)]
pub struct Request {
	pub id: String,
	pub token: String,
	pub operation: Operation,
}

#[derive(Debug, Deserialize)]
pub struct Reply {
	pub id: String,
	pub ok: bool,
	#[serde(default)]
	pub result: Option<Value>,
	#[serde(default)]
	pub error: Option<String>,
}

#[derive(Serialize)]
struct OkEnvelope<'a> {
	id: &'a str,
	ok: bool,
	result: Value,
}

#[derive(Serialize)]
struct ErrorEnvelope<'a> {
	id: &'a str,
	ok: bool,
	error: &'a str,
}

/// Parse one newline-stripped request line.
pub fn parse_request(line: &str) -> Result<Request, String> {
	serde_json::from_str(line).map_err(|err| format!("invalid daemon broker request: {err}"))
}

/// Serialize an authenticated request line (newline included).
pub fn encode_request(id: &str, token: &str, operation: Operation) -> Result<String, String> {
	let request = Request { id: id.to_string(), token: token.to_string(), operation };
	let mut line =
		serde_json::to_string(&request).map_err(|err| format!("cannot encode daemon broker request: {err}"))?;
	line.push('\n');
	Ok(line)
}

/// Parse one newline-stripped reply line.
pub fn parse_reply(line: &str) -> Result<Reply, String> {
	serde_json::from_str(line).map_err(|err| format!("invalid daemon broker reply: {err}"))
}

/// Serialize a successful reply envelope (newline included).
#[must_use]
pub fn encode_ok(id: &str, result: &impl Serialize) -> String {
	let envelope = OkEnvelope {
		id,
		ok: true,
		result: serde_json::to_value(result).unwrap_or(Value::Null),
	};
	let mut line = serde_json::to_string(&envelope)
		.unwrap_or_else(|_| format!(r#"{{"id":"{id}","ok":false,"error":"reply encoding failed"}}"#));
	line.push('\n');
	line
}

/// Serialize a failed reply envelope (newline included). Never include env values.
#[must_use]
pub fn encode_error(id: &str, message: &str) -> String {
	let envelope = ErrorEnvelope { id, ok: false, error: message };
	let mut line = serde_json::to_string(&envelope)
		.unwrap_or_else(|_| r#"{"id":"unknown","ok":false,"error":"reply encoding failed"}"#.to_string());
	line.push('\n');
	line
}

/// Validate the parts the broker must check before reserving a name.
pub fn validate_start(spec: &WireSpec) -> Result<(), String> {
	if !valid_process_name(&spec.name) {
		return Err("Process name must be 1-48 letters, numbers, dots, underscores, or hyphens".to_string());
	}
	if spec.application.trim().is_empty() {
		return Err("start requires application".to_string());
	}
	if spec.detached && spec.pty {
		return Err("A detached process cannot allocate a PTY".to_string());
	}
	let cwd = Path::new(&spec.cwd);
	if !cwd.is_dir() {
		return Err(format!("Process cwd is not a directory: {}", spec.cwd));
	}
	Ok(())
}

fn valid_process_name(name: &str) -> bool {
	let mut chars = name.chars();
	match chars.next() {
		Some(first) if first.is_ascii_alphanumeric() => {},
		_ => return false,
	}
	name.chars().count() <= 48
		&& chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PingReply<'a> {
	pub op: &'static str,
	pub project_dir: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartReply<'a> {
	pub op: &'static str,
	pub daemon: &'a ProcessSnapshot,
	pub ready_timed_out: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListReply<'a> {
	pub op: &'static str,
	pub daemons: &'a [ProcessSnapshot],
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogsReply<'a> {
	pub op: &'static str,
	pub name: &'a str,
	pub text: &'a str,
	pub cursor: u64,
	pub timed_out: bool,
	pub state: crate::tools::process::ProcessState,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WaitReply<'a> {
	pub op: &'static str,
	pub daemon: &'a ProcessSnapshot,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub matched: Option<&'a str>,
	pub timed_out: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotReply<'a> {
	pub op: &'static str,
	pub daemon: &'a ProcessSnapshot,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DescribeReply<'a> {
	pub op: &'static str,
	pub daemon: &'a ProcessSnapshot,
	pub spec: &'a PublicSpec,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TimeoutReply {
	pub op: &'static str,
}

#[derive(Serialize)]
pub struct ShutdownReply {
	pub op: &'static str,
}

#[cfg(test)]
mod tests {
	use super::*;

	fn spec() -> WireSpec {
		WireSpec {
			name: "web".to_string(),
			application: "true".to_string(),
			args: Vec::new(),
			env: BTreeMap::from([("SECRET".to_string(), "value".to_string())]),
			full_env: true,
			cwd: std::env::temp_dir().to_string_lossy().into_owned(),
			pty: false,
			ready: None,
			restart: RestartPolicy::No,
			persist: true,
			detached: false,
		}
	}

	#[test]
	fn public_spec_never_contains_env() {
		let spec = spec();
		let public = serde_json::to_string(&spec.public()).expect("serialize");
		assert!(!public.contains("SECRET"));
		let process = spec.into_process_spec();
		let describe = serde_json::to_string(&PublicSpec::from(&process)).expect("serialize");
		assert!(!describe.contains("SECRET"));
		assert_eq!(describe, public);
	}

	#[test]
	fn wire_spec_round_trips_env_for_recovery() {
		let spec = spec();
		let encoded = serde_json::to_string(&spec).expect("serialize");
		let decoded: WireSpec = serde_json::from_str(&encoded).expect("deserialize");
		assert_eq!(decoded.env.get("SECRET").map(String::as_str), Some("value"));
	}

	#[test]
	fn parses_typed_operations_and_owner() {
		let request = parse_request(
			r#"{"id":"1","token":"t","operation":{"op":"start","spec":{"name":"web","application":"true","cwd":"/tmp","env":{"A":"B"}},"owner":"agent"}}"#,
		)
		.expect("request");
		assert_eq!(request.operation.owner(), Some("agent"));
		assert_eq!(request.operation.name(), Some("web"));
		let wire = serde_json::to_string(&request).expect("serialize");
		assert!(wire.contains("\"env\":{\"A\":\"B\"}"));
	}

	#[test]
	fn token_comparison_is_exact() {
		assert!(token_matches("abc", "abc"));
		assert!(!token_matches("abc", "abd"));
		assert!(!token_matches("abc", "ab"));
	}

	#[test]
	fn rejects_oversized_socket_path() {
		let long = PathBuf::from("/".to_string() + &"x".repeat(MAX_SOCKET_PATH_BYTES));
		assert!(endpoint(&long).is_err());
	}

	#[test]
	fn name_validation_matches_ts() {
		assert!(valid_process_name("a"));
		assert!(valid_process_name("web.api-2"));
		assert!(!valid_process_name(""));
		assert!(!valid_process_name(".web"));
		assert!(!valid_process_name("web/api"));
		assert!(!valid_process_name(&"a".repeat(49)));
	}
	#[test]
	fn concurrent_scope_creation_publishes_one_complete_v1_record() {
		use std::sync::{Arc, Barrier};
		let root = std::env::temp_dir().join(format!(
			"omp-scope-race-{}-{}",
			std::process::id(),
			random_hex(4).unwrap()
		));
		let runtime = root.join("short");
		let logical = root.join("logical");
		let project = root.join("project");
		std::fs::create_dir_all(&runtime).unwrap();
		std::fs::create_dir_all(&project).unwrap();
		let barrier = Arc::new(Barrier::new(8));
		let mut threads = Vec::new();
		for _ in 0..8 {
			let barrier = Arc::clone(&barrier);
			let runtime = runtime.clone();
			let logical = logical.clone();
			let project = project.clone();
			threads.push(std::thread::spawn(move || {
				barrier.wait();
				ensure_scope(&runtime, &logical, &project).expect("concurrent ensure_scope")
			}));
		}
		for thread in threads {
			thread.join().expect("scope thread");
		}
		let record = read_scope_record(&runtime.join(SCOPE_FILE)).expect("complete scope json");
		assert_eq!(record.version, SCOPE_VERSION);
		assert!(is_hex_token(&record.root_token));
		verify_scope(&runtime, &logical, &project).expect("v1 verifies");
		let _ = std::fs::remove_dir_all(&root);
	}

	#[test]
	fn shortened_runtime_without_v1_scope_fails_closed() {
		let root = std::env::temp_dir().join(format!(
			"omp-scope-short-{}-{}",
			std::process::id(),
			random_hex(4).unwrap()
		));
		let logical = root.join("logical");
		let project = root.join("project");
		std::fs::create_dir_all(&project).unwrap();
		let runtime = short_runtime_dir(&logical, &project).expect("short runtime");
		assert!(
			verify_scope(&runtime, &logical, &project).is_err(),
			"shortened dir without checkpoint must fail closed"
		);
		write_private_new(&runtime.join(SCOPE_FILE), br#"{"projectDir":"x"}"#).unwrap();
		assert!(
			verify_scope(&runtime, &logical, &project).is_err(),
			"shortened dir with legacy v0 must fail closed"
		);
		let _ = std::fs::remove_dir_all(&runtime);
		let _ = std::fs::remove_dir_all(&root);
	}
}
