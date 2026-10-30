//! `ssh://[user@]host[:port]/abs-path` read handler over the native OpenSSH
//! client.
//!
//! Mirrors `packages/coding-agent/src/internal-urls/ssh-protocol.ts` and
//! `packages/coding-agent/src/ssh/file-transfer.ts` for the read-only slice:
//! one remote UTF-8 text file (≤ 1 MiB) or one-level directory listing. The
//! argv is built from discrete arguments — never a local shell — host/user/port
//! are validated (no leading `-`, no password, no empty user/port), the remote
//! path is single-quoted for POSIX `sh`, and the remote must answer a POSIX
//! capability probe (`uname -s` + `sh -c`) before any `head`/`ls` runs.
//!
//! Host-key policy is the user's own: no `StrictHostKeyChecking` override is
//! ever added, so an unknown host fails exactly as the user's config says. The
//! child inherits the parent environment (user `PATH`, agent socket, config),
//! with no hardcoded prefix.
//!
//! Special files (FIFO/socket/device) are refused by a POSIX `test` preflight,
//! but that preflight and the read are separate remote commands against the
//! same pathname. It is therefore a best-effort check/open guard inherited from
//! upstream, not a descriptor-level guarantee: a remote that swaps the checked
//! file for a special file in between can still make the read open it, bounded
//! by the 30 s deadline and the byte cap. Descriptor-safe classification would
//! need a remote helper (e.g. `python3`) that ordinary POSIX remotes may not
//! have, so it is deliberately not a runtime prerequisite.
//!
//! Deferred (retained scope, not claimed): writes/remote edit/glob, ControlMaster
//! reuse, key-permission checks, and any `sshfs`-style `sourcePath`.

use std::{
	collections::BTreeSet,
	ffi::OsString,
	path::{Path, PathBuf},
	process::Stdio,
	sync::Mutex,
	time::Duration,
};

use async_trait::async_trait;
use serde_json::Value;
use tokio::process::Command;

use super::internal_urls::{InternalResource, InternalUrl, ProtocolHandler, ResolveContext};
use crate::CancellationToken;

/// Registered scheme.
pub const SSH_SCHEME: &str = "ssh";

/// Largest remote text file `ssh://` materializes (mirrors TS `SSH_TEXT_MAX_BYTES`).
const SSH_TEXT_MAX_BYTES: usize = 1024 * 1024;

/// Stderr kept from one remote invocation; enough for a diagnostic, bounded so a
/// hostile remote cannot flood the tool result.
const SSH_STDERR_MAX_BYTES: usize = 8 * 1024;

/// Per-operation ceiling (mirrors TS `DEFAULT_TIMEOUT_MS`).
const SSH_TIMEOUT: Duration = Duration::from_secs(30);

/// Ceiling for reaping a child after its process group was killed; a
/// SIGKILLed leader does not linger, so this only guards a wedged wait,
/// not the boundary.
const SSH_REAP_TIMEOUT: Duration = Duration::from_secs(5);

/// One resolved destination: either a configured `ssh.json` entry or an opaque
/// destination OpenSSH resolves itself (plain `~/.ssh/config` alias).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshTarget {
	pub host:     String,
	pub username: Option<String>,
	pub port:     Option<u16>,
	pub key_path: Option<PathBuf>,
}

impl SshTarget {
	/// `[user@]host` handed to OpenSSH.
	fn destination(&self) -> String {
		match &self.username {
			Some(user) => format!("{user}@{}", self.host),
			None => self.host.clone(),
		}
	}
}

/// One ssh invocation plan: destination, port/key flags, plus extra argv/env an
/// embedder or the live fixture can prepend (e.g. `-F <config>`).
struct SshConnection {
	destination: String,
	port:        Option<u16>,
	key_path:    Option<PathBuf>,
	options:     Vec<OsString>,
	env:         Vec<(OsString, OsString)>,
}

impl SshConnection {
	fn from_target(target: &SshTarget) -> Self {
		Self {
			destination: target.destination(),
			port:        target.port,
			key_path:    target.key_path.clone(),
			options:     Vec::new(),
			env:         Vec::new(),
		}
	}

	/// Full `ssh` argv. `remote` is one argument, so no local shell ever parses
	/// it; `-n` detaches stdin and `BatchMode=yes` forbids interactive auth
	/// prompts. No `StrictHostKeyChecking` flag: the user's policy stands.
	fn argv(&self, remote: &str) -> Vec<OsString> {
		let mut argv: Vec<OsString> = vec![
			"ssh".into(),
			"-n".into(),
			"-o".into(),
			"BatchMode=yes".into(),
		];
		if let Some(port) = self.port {
			argv.push("-p".into());
			argv.push(port.to_string().into());
		}
		if let Some(key) = &self.key_path {
			argv.push("-i".into());
			argv.push(key.as_os_str().to_owned());
		}
		argv.extend(self.options.iter().cloned());
		argv.push("--".into());
		argv.push(self.destination.clone().into());
		argv.push(remote.into());
		argv
	}

	/// Cache identity for the POSIX probe; never leaves this module or a log.
	fn cache_key(&self) -> String {
		let mut key = format!(
			"{}|{}|{}",
			self.destination,
			self.port.map_or_else(String::new, |port| port.to_string()),
			self
				.key_path
				.as_ref()
				.map_or_else(String::new, |path| path.display().to_string())
		);
		for option in &self.options {
			key.push('|');
			key.push_str(&option.to_string_lossy());
		}
		key
	}
}

/// One entry of a remote directory listing.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RemoteEntry {
	name:         String,
	is_directory: bool,
}

/// Remote path classification used for dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteKind {
	File,
	Directory,
	/// FIFO, socket or device: reading would block or stream forever.
	Other,
	Missing,
}

/// Built-in `ssh://` read handler.
pub struct SshProtocolHandler {
	/// Targets that already answered the POSIX probe.
	probes:  Mutex<BTreeSet<String>>,
	/// Extra argv/env for fixtures or embedders; empty in production.
	options: Vec<OsString>,
	env:     Vec<(OsString, OsString)>,
}

impl Default for SshProtocolHandler {
	fn default() -> Self {
		Self::new()
	}
}

impl SshProtocolHandler {
	#[must_use]
	pub fn new() -> Self {
		Self {
			probes:  Mutex::new(BTreeSet::new()),
			options: Vec::new(),
			env:     Vec::new(),
		}
	}

	/// Fixture/embedder hook: prepend `options` to every `ssh` argv and add
	/// `env` to every child. Production always uses [`SshProtocolHandler::new`].
	#[cfg(test)]
	fn with_launch(options: Vec<OsString>, env: Vec<(OsString, OsString)>) -> Self {
		Self { probes: Mutex::new(BTreeSet::new()), options, env }
	}

	fn connection(&self, target: &SshTarget) -> SshConnection {
		let mut connection = SshConnection::from_target(target);
		connection.options.clone_from(&self.options);
		connection.env.clone_from(&self.env);
		connection
	}

	/// Verify the remote is POSIX with a usable `sh`, once per target. Windows
	/// hosts (cmd/powershell default shell) cannot drive `head`/`ls`.
	async fn ensure_posix(
		&self,
		connection: &SshConnection,
		signal: &CancellationToken,
	) -> Result<(), String> {
		let key = connection.cache_key();
		if self.lock_probes().contains(&key) {
			return Ok(());
		}
		let uname = run_ssh(connection, "uname -s", signal).await?;
		let os = String::from_utf8_lossy(&uname).trim().to_ascii_lowercase();
		if os.contains("mingw") || os.contains("cygwin") || os.contains("msys") || os.contains("windows")
		{
			return Err(format!(
				"ssh://: {} is a Windows host; ssh:// supports POSIX remotes only (head/ls) — use \
				 `bash` with a remote SSH command for Windows hosts",
				connection.destination
			));
		}
		let probe = run_ssh(connection, &remote_command("printf omp-posix-ok"), signal)
			.await
			.map_err(|error| {
				format!(
					"ssh://: {} has no verified POSIX `sh` for ssh:// reads — {error}",
					connection.destination
				)
			})?;
		if String::from_utf8_lossy(&probe).trim() != "omp-posix-ok" {
			return Err(format!(
				"ssh://: {} has no verified POSIX `sh` for ssh:// reads; use `bash` with a remote SSH \
				 command for this host",
				connection.destination
			));
		}
		self.lock_probes().insert(key);
		Ok(())
	}

	fn lock_probes(&self) -> std::sync::MutexGuard<'_, BTreeSet<String>> {
		self.probes.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
	}

	/// Listing of configured hosts and `~/.ssh/config` aliases for bare `ssh://`.
	async fn host_index(&self, url: &InternalUrl, cwd: &Path) -> InternalResource {
		let configured = load_configured_hosts(cwd);
		let content = if configured.is_empty() {
			format_host_index(&ssh_config_hosts())
		} else {
			format_configured_index(&configured)
		};
		let mut resource = InternalResource::text(&url.href, content);
		resource.content_type = "text/markdown".to_string();
		resource.immutable = true;
		resource
	}
}

#[async_trait]
impl ProtocolHandler for SshProtocolHandler {
	fn scheme(&self) -> &str {
		SSH_SCHEME
	}

	async fn resolve(
		&self,
		url: &InternalUrl,
		context: &ResolveContext,
	) -> Result<InternalResource, String> {
		if url.host.is_empty() {
			// A host-less URL carrying a path is malformed, not an index request.
			if !url.path.is_empty() && url.path != "/" {
				return Err(format!(
					"ssh:// requires a host before the path: ssh://<host>{} (host-less ssh://{} is \
					 not valid)",
					url.path, url.path
				));
			}
			return Ok(self.host_index(url, &context.cwd).await);
		}
		let target = resolve_target(&url.host, &context.cwd)?;
		let remote_path = remote_path_from_url(url)?;
		let connection = self.connection(&target);
		self.ensure_posix(&connection, &context.signal).await?;
		// Best-effort preflight, matching the TypeScript handler: classification and
		// read are two remote commands against the same pathname, so a remote
		// directory owner who replaces the checked file with a FIFO/device between
		// them can still make the read open that special file. This check/open race
		// is inherited from upstream and is NOT a safety guarantee; the 30 s
		// deadline and the byte cap only bound the damage. A missing path/short stat
		// failure falls through to the read so the original remote stderr surfaces.
		let kind = stat_remote(&connection, &remote_path, &context.signal).await?;
		match kind {
			RemoteKind::Directory => {
				let entries = list_remote(&connection, &remote_path, &context.signal).await?;
				let mut resource =
					InternalResource::text(&url.href, format_dir_listing(&entries));
				resource.immutable = true;
				resource.is_directory = true;
				Ok(resource)
			},
			RemoteKind::Other => Err(format!(
				"ssh://: {remote_path} is not a regular file (FIFO, socket, or device); ssh:// reads \
				 UTF-8 text files only — use `bash` with a remote SSH command for special files"
			)),
			RemoteKind::File | RemoteKind::Missing => {
				let bytes = read_remote(&connection, &remote_path, &context.signal).await?;
				let content = decode_text(&bytes, &remote_path)?;
				let mut resource = InternalResource::text(&url.href, content);
				resource.content_type = content_type_for(&remote_path).to_string();
				Ok(resource)
			},
		}
	}
}

/// Remote command string: `<script>` run under a POSIX `sh`, single-quoted so
/// the login shell (fish/csh included) never parses the snippet itself.
fn remote_command(script: &str) -> String {
	format!("sh -c {}", quote_posix(script))
}

/// POSIX single-quote a value: `'` becomes `'\''`.
fn quote_posix(value: &str) -> String {
	let mut quoted = String::with_capacity(value.len() + 2);
	quoted.push('\'');
	for character in value.chars() {
		if character == '\'' {
			quoted.push_str("'\\''");
		} else {
			quoted.push(character);
		}
	}
	quoted.push('\'');
	quoted
}

/// Run one `ssh` invocation with bounded stdout/stderr, a 30 s ceiling, and
/// cancellation that kills the process group and attempts to reap the child.
///
/// The ceiling covers pipe draining AND the final wait. On cancellation or
/// timeout the group is signalled while the leader is still unreaped (the
/// PID/PGID cannot have been recycled), and only then is a bounded reap
/// attempted: a wedged wait is abandoned after [`SSH_REAP_TIMEOUT`], so
/// cleanup is best-effort, not a guarantee.
async fn run_ssh(
	connection: &SshConnection,
	remote: &str,
	signal: &CancellationToken,
) -> Result<Vec<u8>, String> {
	let argv = connection.argv(remote);
	let mut command = Command::new(&argv[0]);
	command
		.args(&argv[1..])
		.stdin(Stdio::null())
		.stdout(Stdio::piped())
		.stderr(Stdio::piped())
		.kill_on_drop(true);
	for (key, value) in &connection.env {
		command.env(key, value);
	}
	#[cfg(unix)]
	command.process_group(0);
	let mut child = command.spawn().map_err(|error| {
		if error.kind() == std::io::ErrorKind::NotFound {
			format!(
				"ssh://: native `ssh` client not found on PATH; install OpenSSH to read ssh:// URLs \
				 ({error})"
			)
		} else {
			format!("ssh://: cannot start ssh: {error}")
		}
	})?;
	let pid = child.id();
	let stdout = child
		.stdout
		.take()
		.ok_or_else(|| "ssh://: ssh stdout pipe unavailable".to_string())?;
	let stderr = child
		.stderr
		.take()
		.ok_or_else(|| "ssh://: ssh stderr pipe unavailable".to_string())?;
	let deadline = tokio::time::Instant::now() + SSH_TIMEOUT;
	// Phase 1 — drain both pipes under the shared boundary. The readers are plain
	// futures (never spawned tasks) owning the pipes, so dropping them closes our
	// ends deterministically. Reading before waiting keeps the leader UNREAPED
	// while they run: a `LocalCommand`/`ProxyCommand` descendant that inherits the
	// output descriptors cannot outlive the deadline into an unguarded wait.
	let read_outcome = {
		let reads = async {
			tokio::join!(
				read_capped(stdout, SSH_TEXT_MAX_BYTES + 1),
				read_capped(stderr, SSH_STDERR_MAX_BYTES),
			)
		};
		tokio::select! {
			reads = reads => Ok(reads),
			() = signal.cancelled() => Err("cancelled"),
			() = tokio::time::sleep_until(deadline) => Err("timeout"),
		}
	};
	let (stdout, stderr) = match read_outcome {
		Ok(reads) => reads,
		Err(reason) => {
			// The leader is still unreaped, so its PID/PGID cannot have been
			// recycled: signal the whole group first (that is what reaches
			// descendants holding the pipes), then attempt the bounded reap.
			kill_group(pid);
			let _ = tokio::time::timeout(SSH_REAP_TIMEOUT, child.wait()).await;
			return Err(interrupted_error(reason));
		},
	};
	// Phase 2 — the leader's own exit, inside the SAME deadline, with the same
	// kill-before-any-wait order on cancellation or timeout.
	let waited = {
		let waiting = child.wait();
		tokio::pin!(waiting);
		tokio::select! {
			status = &mut waiting => Ok(status),
			() = signal.cancelled() => Err("cancelled"),
			() = tokio::time::sleep_until(deadline) => Err("timeout"),
		}
	};
	let status = match waited {
		Ok(Ok(status)) => status,
		Ok(Err(error)) => {
			kill_group(pid);
			let _ = tokio::time::timeout(SSH_REAP_TIMEOUT, child.wait()).await;
			return Err(format!("ssh://: ssh wait failed: {error}"));
		},
		Err(reason) => {
			kill_group(pid);
			let _ = tokio::time::timeout(SSH_REAP_TIMEOUT, child.wait()).await;
			return Err(interrupted_error(reason));
		},
	};
	let stdout = stdout.map_err(|error| format!("ssh://: stdout read failed: {error}"))?;
	let stderr = stderr.unwrap_or_default();
	if !status.success() {
		let text = String::from_utf8_lossy(&stderr);
		let text = text.trim();
		return Err(if text.is_empty() {
			format!("ssh://: ssh exited with {status}")
		} else {
			format!("ssh://: {text}")
		});
	}
	Ok(stdout)
}

/// Message for a cancelled or timed-out invocation (the caller already
/// signalled the process group and attempted a bounded reap).
fn interrupted_error(reason: &str) -> String {
	match reason {
		"cancelled" => "ssh:// read cancelled".to_string(),
		_ => format!("ssh://: remote command timed out after {}s", SSH_TIMEOUT.as_secs()),
	}
}

/// Read a pipe keeping at most `cap` bytes while still draining the rest, so a
/// child that writes past the cap never blocks on a full pipe.
async fn read_capped<R>(mut reader: R, cap: usize) -> std::io::Result<Vec<u8>>
where
	R: tokio::io::AsyncRead + Unpin,
{
	use tokio::io::AsyncReadExt;

	let mut kept = Vec::new();
	let mut buffer = [0u8; 8192];
	loop {
		let read = reader.read(&mut buffer).await?;
		if read == 0 {
			return Ok(kept);
		}
		let room = cap.saturating_sub(kept.len());
		if room > 0 {
			kept.extend_from_slice(&buffer[..read.min(room)]);
		}
	}
}

/// SIGKILL the process group created with `process_group(0)` so a cancelled
/// remote command's group members are signalled instead of being left running;
/// members that left the group, or a failed `kill(2)`, are outside this scope.
fn kill_group(pid: Option<u32>) {
	#[cfg(unix)]
	if let Some(pid) = pid {
		let group = i32::try_from(pid).unwrap_or(0);
		if group > 1 {
			// SAFETY: `kill(2)` has no memory effects; the negative pid targets the
			// group this child created, and a dead group is a harmless ESRCH.
			unsafe {
				libc::kill(-group, libc::SIGKILL);
			}
		}
	}
	#[cfg(not(unix))]
	let _ = pid;
}

/// POSIX `test` classification (portable across Linux/BSD/macOS).
///
/// Best-effort only: the following read runs as a separate command against the
/// same pathname, so this is not an atomic check/open guard (see the module
/// doc). It exists to fail fast on the common FIFO/device case, not to
/// guarantee that the later `head` opens a regular file.
async fn stat_remote(
	connection: &SshConnection,
	remote_path: &str,
	signal: &CancellationToken,
) -> Result<RemoteKind, String> {
	let path = quote_posix(remote_path);
	let script = format!(
		"if [ -d {path} ]; then echo directory; elif [ -f {path} ]; then echo file; elif [ -e \
		 {path} ]; then echo other; else echo missing; fi"
	);
	let out = run_ssh(connection, &remote_command(&script), signal).await?;
	Ok(match String::from_utf8_lossy(&out).trim() {
		"directory" => RemoteKind::Directory,
		"file" => RemoteKind::File,
		"other" => RemoteKind::Other,
		_ => RemoteKind::Missing,
	})
}

/// Read at most 1 MiB + 1 so an over-limit file is detectable, and reject it.
async fn read_remote(
	connection: &SshConnection,
	remote_path: &str,
	signal: &CancellationToken,
) -> Result<Vec<u8>, String> {
	let script = format!(
		"head -c {} -- {}",
		SSH_TEXT_MAX_BYTES + 1,
		quote_posix(remote_path)
	);
	let bytes = run_ssh(connection, &remote_command(&script), signal).await?;
	if bytes.len() > SSH_TEXT_MAX_BYTES {
		return Err(format!(
			"ssh://: {remote_path} exceeds the 1 MiB limit; ssh:// supports text files up to 1 MiB — \
			 use an sshfs mount for larger files"
		));
	}
	Ok(bytes)
}

/// One-level listing with `LC_ALL=C ls -1Ap` (dotfiles included, `name/`
/// marks a directory). Entries keep the remote `ls` order; the renderer
/// ([`format_dir_listing`]) owns the directories-first sort, so the
/// rendered contract is self-contained.
async fn list_remote(
	connection: &SshConnection,
	remote_path: &str,
	signal: &CancellationToken,
) -> Result<Vec<RemoteEntry>, String> {
	let script = format!("LC_ALL=C ls -1Ap -- {}", quote_posix(remote_path));
	let bytes = run_ssh(connection, &remote_command(&script), signal).await?;
	if bytes.len() > SSH_TEXT_MAX_BYTES {
		return Err(format!(
			"ssh://: directory listing of {remote_path} exceeds 1 MiB; read individual files instead"
		));
	}
	let text = String::from_utf8_lossy(&bytes);
	Ok(text
		.lines()
		.filter(|line| !line.is_empty())
		.map(|line| match line.strip_suffix('/') {
			Some(name) => RemoteEntry { name: name.to_string(), is_directory: true },
			None => RemoteEntry { name: line.to_string(), is_directory: false },
		})
		.collect())
}

/// Render the listing exactly like the local directory resource: plain `name/`
/// lines, or `(empty directory)`. Sorts directories first, then by name,
/// so every caller sees one order (mirrors TS `buildDirectoryResource`).
fn format_dir_listing(entries: &[RemoteEntry]) -> String {
	if entries.is_empty() {
		return "(empty directory)".to_string();
	}
	let mut ordered: Vec<&RemoteEntry> = entries.iter().collect();
	ordered.sort_by(|a, b| {
		b.is_directory
			.cmp(&a.is_directory)
			.then_with(|| a.name.cmp(&b.name))
	});
	ordered
		.into_iter()
		.map(|entry| {
			if entry.is_directory {
				format!("{}/", entry.name)
			} else {
				entry.name.clone()
			}
		})
		.collect::<Vec<String>>()
		.join("\n")
}

/// Strict UTF-8 text; a NUL or invalid byte is a binary file, not a text read.
fn decode_text(bytes: &[u8], remote_path: &str) -> Result<String, String> {
	if bytes.contains(&0) {
		return Err(binary_error(remote_path));
	}
	std::str::from_utf8(bytes)
		.map(str::to_string)
		.map_err(|_| binary_error(remote_path))
}

fn binary_error(remote_path: &str) -> String {
	format!(
		"ssh://: {remote_path} is a binary or non-UTF-8 file; ssh:// supports UTF-8 text only — use \
		 `bash` with a remote SSH command or an `sshfs` mount"
	)
}

/// Content type from the last path segment's extension.
fn content_type_for(remote_path: &str) -> &'static str {
	let lower = remote_path.to_ascii_lowercase();
	if lower.ends_with(".md") || lower.ends_with(".markdown") {
		"text/markdown"
	} else if lower.ends_with(".json") {
		"application/json"
	} else {
		"text/plain"
	}
}

/// Remote absolute path from the URL: pre-decoded `?`/`#` are unsupported URL
/// delimiters, the remainder is strict percent-decoded and must be absolute.
fn remote_path_from_url(url: &InternalUrl) -> Result<String, String> {
	if url.path.contains('?') {
		return Err(format!(
			"ssh:// does not support URL query strings; percent-encode a literal '?' as %3F in the \
			 path: {}",
			url.href
		));
	}
	if url.path.contains('#') {
		return Err(format!(
			"ssh:// does not support URL fragments; percent-encode a literal '#' as %23 in the path: \
			 {}",
			url.href
		));
	}
	let decoded = percent_decode(&url.path)?;
	if decoded.is_empty() {
		return Err(
			"ssh:// requires an absolute path, e.g. ssh://host/etc/hosts or ssh://host/ for the root \
			 directory"
				.to_string(),
		);
	}
	if !decoded.starts_with('/') {
		return Err("ssh:// requires an absolute remote path starting with '/'".to_string());
	}
	if decoded.contains('\0') {
		return Err("ssh://: remote path contains a NUL byte".to_string());
	}
	Ok(decoded)
}

/// Decode `%XX` escapes strictly; a stray `%` or invalid UTF-8 is an error.
fn percent_decode(input: &str) -> Result<String, String> {
	let bytes = input.as_bytes();
	let mut out = Vec::with_capacity(bytes.len());
	let mut index = 0;
	while index < bytes.len() {
		if bytes[index] == b'%' {
			let Some(hex) = bytes.get(index + 1..index + 3) else {
				return Err("ssh://: invalid percent-escape".to_string());
			};
			let hex = std::str::from_utf8(hex)
				.map_err(|_| "ssh://: invalid percent-escape".to_string())?;
			let value = u8::from_str_radix(hex, 16)
				.map_err(|_| "ssh://: invalid percent-escape".to_string())?;
			out.push(value);
			index += 3;
		} else {
			out.push(bytes[index]);
			index += 1;
		}
	}
	String::from_utf8(out).map_err(|_| "ssh://: percent-escapes are not valid UTF-8".to_string())
}

/// Resolve the raw authority (`[user@]host[:port]`) to a target, rejecting
/// everything OpenSSH would otherwise interpret as an option or password.
fn resolve_target(authority: &str, cwd: &Path) -> Result<SshTarget, String> {
	let target = resolve_authority(authority, cwd)?;
	// Validate the FINAL destination, configured `ssh.json` entries included, with
	// the same rules the URI path uses. Configured values never pass through the
	// URI parsing above, so without this an `ssh.json` host of `user@host` or an
	// option-like `-o…` value could change how OpenSSH parses the destination.
	validate_component(&target.host, "host")?;
	if target.host.contains('@') {
		return Err(
			"ssh://: resolved host contains an embedded userinfo '@'; put the user in `username`, \
			 not in `host`"
				.to_string(),
		);
	}
	if let Some(user) = &target.username {
		validate_component(user, "username")?;
		if user.contains(':') || user.contains('@') {
			return Err("ssh://: resolved username contains an unsupported ':' or '@'".to_string());
		}
	}
	Ok(target)
}

/// Raw authority parsing behind [`resolve_target`].
fn resolve_authority(authority: &str, cwd: &Path) -> Result<SshTarget, String> {
	let (userinfo, host_port) = match authority.split_once('@') {
		Some((user, rest)) if !rest.contains('@') => (Some(user), rest),
		Some(_) => {
			return Err(
				"ssh://: unsupported or malformed authority; use ssh://[user@]host[:1-65535]/<absolute-path>"
					.to_string(),
			);
		},
		None => (None, authority),
	};
	if let Some(user) = userinfo {
		if user.is_empty() {
			return Err("ssh://: empty username; drop the leading '@' or name a user before it".to_string());
		}
		if user.contains(':') {
			return Err(
				"ssh://: password authentication is not supported; ssh:// uses key/agent auth — drop \
				 the ':<password>' from the URL"
					.to_string(),
			);
		}
	}
	let username = match userinfo {
		Some(user) => Some(percent_decode(user)?),
		None => None,
	};
	if let Some(user) = &username {
		validate_component(user, "username")?;
		if user.contains(':') || user.contains('@') {
			return Err("ssh://: username contains an unsupported character".to_string());
		}
	}
	let (raw_host, raw_port) = split_host_port(host_port)?;
	let host = percent_decode(&raw_host)?;
	validate_component(&host, "host")?;
	if host.contains('@') {
		return Err("ssh://: host contains an unsupported '@'".to_string());
	}
	let port = match raw_port {
		Some(port) => Some(parse_port(port)?),
		None => None,
	};

	let configured = load_configured_hosts(cwd);
	if username.is_some() || port.is_some() {
		if configured.iter().any(|entry| entry.name == host) {
			return Err(format!(
				"ssh://: user/port overrides are not allowed for the configured host \"{host}\"; use \
				 ssh://{host}/<path> or an unconfigured hostname"
			));
		}
		return Ok(SshTarget { host, username, port, key_path: None });
	}
	if let Some(entry) = configured.into_iter().find(|entry| entry.name == host) {
		return Ok(SshTarget {
			host:     entry.host,
			username: entry.username,
			port:     entry.port,
			key_path: entry.key_path,
		});
	}
	// Opaque OpenSSH destination: a `~/.ssh/config` alias or any resolvable host.
	Ok(SshTarget {
		host,
		username: None,
		port:     None,
		key_path: None,
	})
}

/// Split `host[:port]`, honoring bracketed IPv6 literals.
fn split_host_port(host_port: &str) -> Result<(String, Option<&str>), String> {
	if let Some(rest) = host_port.strip_prefix('[') {
		let Some((address, tail)) = rest.split_once(']') else {
			return Err("ssh://: unterminated IPv6 literal; use ssh://[addr]:<port>/<path>".to_string());
		};
		return match tail {
			"" => Ok((address.to_string(), None)),
			_ => match tail.strip_prefix(':') {
				Some(port) => Ok((address.to_string(), Some(port))),
				None => Err("ssh://: malformed IPv6 authority".to_string()),
			},
		};
	}
	match host_port.rsplit_once(':') {
		Some((host, port)) => {
			if host.contains(':') {
				return Err(
					"ssh://: unbracketed IPv6 literal; use ssh://[addr]:<port>/<path>".to_string(),
				);
			}
			Ok((host.to_string(), Some(port)))
		},
		None => Ok((host_port.to_string(), None)),
	}
}

/// Validate a port string: digits only, 1–65535, never empty or zero.
fn parse_port(port: &str) -> Result<u16, String> {
	if port.is_empty() {
		return Err("ssh://: empty port; use ssh://host:<1-65535>/<path> or drop the colon".to_string());
	}
	if !port.bytes().all(|byte| byte.is_ascii_digit()) {
		return Err("ssh://: invalid port; use ssh://host:<1-65535>/<path>".to_string());
	}
	let value: u32 = port
		.parse()
		.map_err(|_| "ssh://: invalid port; use ssh://host:<1-65535>/<path>".to_string())?;
	if value == 0 || value > u32::from(u16::MAX) {
		return Err(format!(
			"ssh://: port {value} is not a valid SSH port; use ssh://host:<1-65535>/<path> or omit the \
			 port"
		));
	}
	Ok(u16::try_from(value).unwrap_or(u16::MAX))
}

/// Reject values that start with `-` (an option injection) or contain
/// whitespace/control characters.
fn validate_component(value: &str, label: &str) -> Result<(), String> {
	if value.is_empty() {
		return Err(format!("ssh://: empty {label}"));
	}
	if value.starts_with('-') {
		return Err(format!("ssh://: {label} must not start with '-'"));
	}
	if value
		.chars()
		.any(|character| character.is_whitespace() || character.is_control() || character == '/')
	{
		return Err(format!("ssh://: {label} contains whitespace, a control character, or '/'"));
	}
	Ok(())
}

/// One `ssh.json` host entry (managed/project config).
#[derive(Debug, Clone, PartialEq, Eq)]
struct ConfiguredHost {
	name:        String,
	host:        String,
	username:    Option<String>,
	port:        Option<u16>,
	key_path:    Option<PathBuf>,
	description: Option<String>,
}

/// Configured hosts: `.omp/ssh.json` (project), the user agent dir, then legacy
/// `ssh.json`/`.ssh.json`; the first definition of a name wins.
fn load_configured_hosts(cwd: &Path) -> Vec<ConfiguredHost> {
	let mut candidates = vec![cwd.join(".omp").join("ssh.json")];
	if let Some(agent) = agent_dir() {
		candidates.push(agent.join("ssh.json"));
	}
	candidates.push(cwd.join("ssh.json"));
	candidates.push(cwd.join(".ssh.json"));
	let mut seen: BTreeSet<String> = BTreeSet::new();
	let mut hosts: Vec<ConfiguredHost> = Vec::new();
	for path in candidates {
		for host in read_ssh_json(&path) {
			if seen.insert(host.name.clone()) {
				hosts.push(host);
			}
		}
	}
	hosts
}

/// Parse one `ssh.json`; a missing or invalid file contributes nothing.
fn read_ssh_json(path: &Path) -> Vec<ConfiguredHost> {
	let Ok(text) = std::fs::read_to_string(path) else {
		return Vec::new();
	};
	let Ok(value) = serde_json::from_str::<Value>(&text) else {
		return Vec::new();
	};
	let Some(hosts) = value.get("hosts").and_then(Value::as_object) else {
		return Vec::new();
	};
	let mut entries = Vec::new();
	for (name, raw) in hosts {
		let Some(raw) = raw.as_object() else {
			continue;
		};
		let Some(host) = raw
			.get("host")
			.and_then(Value::as_str)
			.filter(|host| !host.is_empty())
		else {
			continue;
		};
		if name.trim().is_empty() {
			continue;
		}
		let username = raw
			.get("username")
			.and_then(Value::as_str)
			.filter(|user| !user.is_empty())
			.map(str::to_string);
		let port = raw.get("port").and_then(json_port);
		let key_path = raw
			.get("keyPath")
			.or_else(|| raw.get("key"))
			.and_then(Value::as_str)
			.filter(|key| !key.is_empty())
			.map(expand_tilde);
		let description = raw
			.get("description")
			.and_then(Value::as_str)
			.filter(|description| !description.is_empty())
			.map(str::to_string);
		entries.push(ConfiguredHost {
			name: name.clone(),
			host: host.to_string(),
			username,
			port,
			key_path,
			description,
		});
	}
	entries
}

fn json_port(value: &Value) -> Option<u16> {
	let port = match value {
		Value::Number(number) => number.as_u64().and_then(|port| u16::try_from(port).ok()),
		Value::String(text) => text.trim().parse::<u16>().ok(),
		_ => None,
	};
	port.filter(|port| *port > 0)
}

fn expand_tilde(value: &str) -> PathBuf {
	match value.strip_prefix("~/") {
		Some(rest) => home_dir().map_or_else(|| PathBuf::from(value), |home| home.join(rest)),
		None => PathBuf::from(value),
	}
}

/// User agent dir: `PI_CODING_AGENT_DIR`, else `PI_CONFIG_DIR`/`~/.omp` with an
/// optional named profile (`profiles/<profile>/agent`).
fn agent_dir() -> Option<PathBuf> {
	if let Some(dir) = non_empty_env("PI_CODING_AGENT_DIR") {
		return Some(dir);
	}
	let base = non_empty_env("PI_CONFIG_DIR")
		.or_else(|| home_dir().map(|home| home.join(".omp")))?;
	let profile = ["OMP_PROFILE", "PI_PROFILE"]
		.iter()
		.find_map(|key| non_empty_env(key));
	Some(match profile {
		Some(profile) => base.join("profiles").join(profile).join("agent"),
		None => base.join("agent"),
	})
}

fn non_empty_env(key: &str) -> Option<PathBuf> {
	std::env::var_os(key)
		.map(PathBuf::from)
		.filter(|path| !path.as_os_str().is_empty())
}

fn home_dir() -> Option<PathBuf> {
	non_empty_env("HOME").or_else(|| non_empty_env("USERPROFILE"))
}

/// One alias block from `~/.ssh/config` (real discovery, so the bare `ssh://`
/// index never claims "no hosts" when OpenSSH already knows some).
#[derive(Debug, Clone, PartialEq, Eq)]
struct SshConfigHost {
	name:     String,
	hostname: Option<String>,
	user:     Option<String>,
	port:     Option<u16>,
}

fn ssh_config_hosts() -> Vec<SshConfigHost> {
	let Some(home) = home_dir() else {
		return Vec::new();
	};
	let Ok(text) = std::fs::read_to_string(home.join(".ssh").join("config")) else {
		return Vec::new();
	};
	parse_ssh_config(&text)
}

fn parse_ssh_config(text: &str) -> Vec<SshConfigHost> {
	let mut hosts: Vec<SshConfigHost> = Vec::new();
	let mut current: Vec<usize> = Vec::new();
	for line in text.lines() {
		let line = line.trim();
		if line.is_empty() || line.starts_with('#') {
			continue;
		}
		let mut parts = line.split_whitespace();
		let Some(keyword) = parts.next() else {
			continue;
		};
		let values: Vec<&str> = parts.collect();
		let keyword = keyword.to_ascii_lowercase();
		match keyword.as_str() {
			"host" => {
				current.clear();
				for alias in values {
					if alias.chars().any(|character| matches!(character, '*' | '?' | '!')) {
						continue;
					}
					hosts.push(SshConfigHost {
						name:     alias.to_string(),
						hostname: None,
						user:     None,
						port:     None,
					});
					current.push(hosts.len() - 1);
				}
			},
			"match" => current.clear(),
			"hostname" | "user" | "port" => {
				let Some(value) = values.first().copied() else {
					continue;
				};
				for index in &current {
					let Some(entry) = hosts.get_mut(*index) else {
						continue;
					};
					match keyword.as_str() {
						"hostname" => entry.hostname = Some(value.to_string()),
						"user" => entry.user = Some(value.to_string()),
						_ => entry.port = value.trim().parse::<u16>().ok().filter(|port| *port > 0),
					}
				}
			},
			_ => {},
		}
	}
	hosts
}

/// Index for configured `ssh.json` hosts (markdown with per-host links).
fn format_configured_index(hosts: &[ConfiguredHost]) -> String {
	let lines: Vec<String> = hosts
		.iter()
		.map(|host| {
			let address = host_address(
				host.username.as_deref(),
				&host.host,
				host.port,
			);
			let suffix = if address == host.name {
				String::new()
			} else {
				format!(" — `{address}`")
			};
			let description = host
				.description
				.as_ref()
				.map_or_else(String::new, |description| format!(" ({description})"));
			format!(
				"- [{}](ssh://{}/){suffix}{description}",
				host.name,
				encode_component(&host.name)
			)
		})
		.collect();
	format!(
		"# SSH hosts\n\n{} configured host{}:\n\n{}\n",
		hosts.len(),
		if hosts.len() == 1 { "" } else { "s" },
		lines.join("\n")
	)
}

/// Index for `~/.ssh/config` aliases when no `ssh.json` hosts exist.
fn format_host_index(hosts: &[SshConfigHost]) -> String {
	if hosts.is_empty() {
		return "# SSH hosts\n\nNo SSH hosts are configured. Add hosts to an `ssh.json` capability \
		        file (`~/.omp/agent/ssh.json` or `<project>/.omp/ssh.json`), or read \
		        `ssh://<host>/<path>` with any destination OpenSSH can resolve (e.g. a \
		        `~/.ssh/config` alias).\n"
			.to_string();
	}
	let lines: Vec<String> = hosts
		.iter()
		.map(|host| {
			let address = host_address(
				host.user.as_deref(),
				host.hostname.as_deref().unwrap_or(&host.name),
				host.port,
			);
			format!(
				"- [{}](ssh://{}/) — `{address}`",
				host.name,
				encode_component(&host.name)
			)
		})
		.collect();
	format!(
		"# SSH hosts\n\n{} host{} from `~/.ssh/config`:\n\n{}\n",
		hosts.len(),
		if hosts.len() == 1 { "" } else { "s" },
		lines.join("\n")
	)
}

fn host_address(user: Option<&str>, host: &str, port: Option<u16>) -> String {
	format!(
		"{}{}{}",
		user.map_or_else(String::new, |user| format!("{user}@")),
		host,
		port.map_or_else(String::new, |port| format!(":{port}"))
	)
}

/// Percent-encode an authority component leaving unreserved characters intact.
fn encode_component(value: &str) -> String {
	let mut encoded = String::new();
	for byte in value.bytes() {
		if byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b'~') {
			encoded.push(char::from(byte));
		} else {
			encoded.push_str(&format!("%{byte:02X}"));
		}
	}
	encoded
}

#[cfg(test)]
mod tests {
	use std::{
		ffi::OsString,
		fs,
		path::{Path, PathBuf},
		process::{Command as StdCommand, Stdio},
		sync::atomic::{AtomicU64, Ordering},
		time::{Duration, Instant},
	};

	use super::*;

	struct TempDir(PathBuf);

	impl TempDir {
		fn new(label: &str) -> Self {
			static NEXT: AtomicU64 = AtomicU64::new(0);
			let path = std::env::temp_dir().join(format!(
				"omp-ssh-unit-{label}-{}-{}",
				std::process::id(),
				NEXT.fetch_add(1, Ordering::SeqCst)
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

	fn runtime() -> tokio::runtime::Runtime {
		tokio::runtime::Builder::new_current_thread()
			.enable_all()
			.build()
			.expect("runtime")
	}

	fn url(raw: &str) -> InternalUrl {
		InternalUrl::parse(raw).expect("internal url")
	}

	#[test]
	fn whole_host_with_port_is_not_split_into_a_path() {
		let parsed = url("ssh://omp-ssh-test-host:2222");
		assert_eq!(parsed.host, "omp-ssh-test-host:2222");
		assert_eq!(parsed.path, "");
		let error = remote_path_from_url(&parsed).expect_err("no path");
		assert!(error.contains("requires an absolute path"), "{error}");
		let target = resolve_target(&parsed.host, Path::new("/nonexistent-omp-cwd"))
			.expect("target");
		assert_eq!(target.host, "omp-ssh-test-host");
		assert_eq!(target.port, Some(2222));
		assert_eq!(target.destination(), "omp-ssh-test-host");
	}

	#[test]
	fn path_with_selector_chunk_stays_a_path() {
		let parsed = url("ssh://h/file:1-5");
		assert_eq!(parsed.path, "/file:1-5");
		assert_eq!(remote_path_from_url(&parsed).expect("path"), "/file:1-5");
	}

	#[test]
	fn option_and_password_authorities_are_rejected() {
		for bad in [
			"ssh://-oProxyCommand=sh/x",
			"ssh://user@-host/x",
			"ssh://user:secret@host/x",
			"ssh://user:@host/x",
			"ssh://@host/x",
			"ssh://host:/x",
			"ssh://host:0/x",
			"ssh://host:65536/x",
			"ssh://host:abc/x",
			"ssh://host:a:b/x",
			"ssh://a@b@host/x",
		] {
			let parsed = url(bad);
			assert!(
				resolve_target(&parsed.host, Path::new("/nonexistent-omp-cwd")).is_err(),
				"{bad} must be rejected"
			);
		}
	}

	#[test]
	fn bracketed_ipv6_and_user_parse() {
		let parsed = url("ssh://alice@[::1]:2222/x");
		let target = resolve_target(&parsed.host, Path::new("/nonexistent-omp-cwd")).expect("target");
		assert_eq!(target.host, "::1");
		assert_eq!(target.username.as_deref(), Some("alice"));
		assert_eq!(target.port, Some(2222));
		assert_eq!(target.destination(), "alice@::1");
	}

	#[test]
	fn url_suffixes_and_bad_escapes_are_rejected() {
		for (bad, needle) in [
			("ssh://h/path?draft", "query string"),
			("ssh://h/path#frag", "fragments"),
			("ssh://h/bad%ZZ", "percent-escape"),
		] {
			let error = remote_path_from_url(&url(bad)).expect_err(bad);
			assert!(error.contains(needle), "{bad}: {error}");
		}
		assert_eq!(
			remote_path_from_url(&url("ssh://h/a%3Fb")).expect("encoded ?"),
			"/a?b"
		);
	}

	#[test]
	fn quote_posix_escapes_single_quotes() {
		assert_eq!(quote_posix("plain"), "'plain'");
		assert_eq!(quote_posix("a'b"), "'a'\\''b'");
		assert_eq!(
			quote_posix("it's a dir/file"),
			"'it'\\''s a dir/file'"
		);
		assert_eq!(remote_command("printf ok"), "sh -c 'printf ok'");
	}

	#[test]
	fn argv_has_no_known_host_override_and_no_local_shell() {
		let target = SshTarget {
			host:     "h".into(),
			username: Some("u".into()),
			port:     Some(2222),
			key_path: Some(PathBuf::from("/k")),
		};
		let connection = SshConnection::from_target(&target);
		let argv = connection.argv("sh -c 'head -c 5 -- /x'");
		let argv: Vec<String> = argv.iter().map(|arg| arg.to_string_lossy().into_owned()).collect();
		assert_eq!(argv[0], "ssh");
		assert!(argv.contains(&"BatchMode=yes".to_string()));
		assert!(!argv.iter().any(|arg| arg.contains("StrictHostKeyChecking")));
		assert!(!argv.iter().any(|arg| arg == "-c"));
		assert_eq!(argv[argv.len() - 2], "u@h");
		assert_eq!(argv[argv.len() - 1], "sh -c 'head -c 5 -- /x'");
		assert!(argv.windows(2).any(|pair| pair == ["-p", "2222"]));
		assert!(argv.windows(2).any(|pair| pair == ["-i", "/k"]));
	}

	#[test]
	fn configured_hosts_resolve_and_reject_overrides() {
		let dir = TempDir::new("config");
		fs::create_dir_all(dir.0.join(".omp")).expect("config dir");
		fs::write(
			dir.0.join(".omp").join("ssh.json"),
			r#"{"hosts":{"prod":{"host":"10.0.0.1","username":"deploy","port":2222,"keyPath":"~/k"},"info":{"host":"user@evil"},"opt":{"host":"-oProxyCommand=sh"},"baduser":{"host":"10.0.0.2","username":"de:ploy"}}}"#,
		)
		.expect("ssh.json");
		let configured = load_configured_hosts(&dir.0);
		let prod = configured
			.iter()
			.find(|host| host.name == "prod")
			.expect("prod");
		assert_eq!(prod.host, "10.0.0.1");
		assert_eq!(prod.username.as_deref(), Some("deploy"));
		assert_eq!(prod.port, Some(2222));
		let target = resolve_target("prod", &dir.0).expect("target");
		assert_eq!(target.host, "10.0.0.1");
		assert_eq!(target.port, Some(2222));
		assert!(resolve_target("deploy@prod", &dir.0).is_err());
		assert!(resolve_target("prod:22", &dir.0).is_err());
		// Configured entries get the same final validation as URI components: no
		// embedded userinfo, option-like prefix, or ':'/'@' in a username.
		assert!(resolve_target("info", &dir.0).is_err());
		assert!(resolve_target("opt", &dir.0).is_err());
		assert!(resolve_target("baduser", &dir.0).is_err());
		// An unknown host stays an opaque OpenSSH destination.
		let opaque = resolve_target("other", &dir.0).expect("opaque");
		assert_eq!(opaque.host, "other");
		assert_eq!(opaque.port, None);
	}

	#[test]
	fn ssh_config_aliases_are_discovered() {
		let hosts = parse_ssh_config(
			"Host *\n  ServerAliveInterval 30\n\nHost prod\n  HostName 10.0.0.5\n  User \
			 deploy\n  Port 2200\n\nHost a b\n  HostName shared\n\nMatch exec true\n",
		);
		let names: Vec<&str> = hosts.iter().map(|host| host.name.as_str()).collect();
		assert_eq!(names, vec!["prod", "a", "b"]);
		let prod = &hosts[0];
		assert_eq!(prod.hostname.as_deref(), Some("10.0.0.5"));
		assert_eq!(prod.user.as_deref(), Some("deploy"));
		assert_eq!(prod.port, Some(2200));
		assert_eq!(hosts[1].hostname.as_deref(), Some("shared"));
	}

	#[test]
	fn indexes_never_claim_no_hosts_when_hosts_exist() {
		let empty = format_host_index(&[]);
		assert!(empty.contains("No SSH hosts are configured"), "{empty}");
		let discovered = format_host_index(&[SshConfigHost {
			name:     "prod".into(),
			hostname: Some("10.0.0.5".into()),
			user:     Some("deploy".into()),
			port:     Some(2200),
		}]);
		assert!(discovered.contains("[prod](ssh://prod/)"), "{discovered}");
		assert!(discovered.contains("deploy@10.0.0.5:2200"), "{discovered}");
		let configured = format_configured_index(&[ConfiguredHost {
			name:        "box:1".into(),
			host:        "10.0.0.9".into(),
			username:    None,
			port:        None,
			key_path:    None,
			description: Some("lab".into()),
		}]);
		assert!(configured.contains("1 configured host"), "{configured}");
		assert!(configured.contains("box%3A1"), "{configured}");
	}

	#[test]
	fn content_types_and_binary_detection() {
		assert_eq!(content_type_for("/a/NOTE.MD"), "text/markdown");
		assert_eq!(content_type_for("/a/x.json"), "application/json");
		assert_eq!(content_type_for("/a/x.txt"), "text/plain");
		assert_eq!(decode_text(b"ok", "/x").expect("utf8"), "ok");
		assert!(decode_text(b"a\0b", "/x").is_err());
		assert!(decode_text(&[0xff, 0xfe], "/x").is_err());
	}

	#[test]
	fn directory_listing_render_sorts_dirs_first() {
		let entries = vec![
			RemoteEntry { name: "b.txt".into(), is_directory: false },
			RemoteEntry { name: "a".into(), is_directory: true },
			RemoteEntry { name: "a.txt".into(), is_directory: false },
		];
		assert_eq!(format_dir_listing(&entries), "a/\na.txt\nb.txt");
		assert_eq!(format_dir_listing(&[]), "(empty directory)");
	}

	/// Live fixture: needs the real `ssh`, `sshd`, `ssh-keygen` and permission to
	/// bind `127.0.0.1` + run sshd as the current user. No fake `ssh` shim and no
	/// user config is touched — everything lives in one temp dir passed via `-F`.
	#[test]
	#[ignore = "requires real ssh/sshd/ssh-keygen; run with --ignored"]
	fn live_localhost_sshd_read_fixture() {
		for binary in ["ssh", "sshd", "ssh-keygen"] {
			assert!(
				command_available(binary),
				"prerequisite: `{binary}` must be on PATH for the live ssh fixture"
			);
		}
		let fixture = LiveSshd::start();
		let handler = SshProtocolHandler::with_launch(
			vec!["-F".into(), OsString::from(fixture.ssh_config.display().to_string())],
			Vec::new(),
		);
		let scope = super::super::internal_urls::UrlScope::default();
		let context = scope.resolve_context(&fixture.remote, &CancellationToken::new());
		let rt = runtime();

		let plain_url = format!("ssh://fixture{}", fixture.remote.join("plain.txt").display());
		let resource = rt
			.block_on(handler.resolve(&url(&plain_url), &context))
			.expect("read plain file");
		assert_eq!(resource.content, "fixture text\n");
		assert_eq!(resource.content_type, "text/plain");
		assert!(resource.source_path.is_none(), "ssh:// exposes no sourcePath");
		assert!(!resource.is_directory);

		let quoted_url = format!(
			"ssh://fixture{}",
			fixture.remote.join("sub dir").join("it's quoted.txt").display()
		);
		let quoted = rt
			.block_on(handler.resolve(&url(&quoted_url), &context))
			.expect("read quoted path");
		assert_eq!(quoted.content, "quoted name file\n");

		let dir_url = format!("ssh://fixture{}", fixture.remote.join("listing").display());
		let listing = rt
			.block_on(handler.resolve(&url(&dir_url), &context))
			.expect("list dir");
		assert!(listing.is_directory);
		assert!(listing.immutable);
		assert!(listing.source_path.is_none());
		assert_eq!(listing.content, "beta/\nalpha.txt");

		let binary_url = format!("ssh://fixture{}", fixture.remote.join("bin.dat").display());
		let error = rt
			.block_on(handler.resolve(&url(&binary_url), &context))
			.expect_err("binary rejected");
		assert!(error.contains("binary or non-UTF-8"), "{error}");

		let big_url = format!("ssh://fixture{}", fixture.remote.join("big.txt").display());
		let error = rt
			.block_on(handler.resolve(&url(&big_url), &context))
			.expect_err("over limit rejected");
		assert!(error.contains("exceeds the 1 MiB limit"), "{error}");

		let fifo_url = format!("ssh://fixture{}", fixture.remote.join("pipe").display());
		let error = rt
			.block_on(handler.resolve(&url(&fifo_url), &context))
			.expect_err("fifo rejected");
		assert!(error.contains("not a regular file"), "{error}");

		// Closed port: the transport stderr surfaces as an error, never a fallback.
		let closed_url = format!("ssh://127.0.0.1:{}/anything", fixture.closed_port);
		let error = rt
			.block_on(handler.resolve(&url(&closed_url), &context))
			.expect_err("refused connection");
		assert!(error.starts_with("ssh://"), "{error}");

		// Invalid targets never reach a spawn.
		for bad in [
			"ssh://-oProxyCommand=sh/x",
			"ssh://host:/x",
			"ssh://user:secret@host/x",
			"ssh://host:0/x",
			"ssh:///etc/hosts",
		] {
			assert!(
				rt.block_on(handler.resolve(&url(bad), &context)).is_err(),
				"{bad}"
			);
		}

		// Cancellation kills the owned process group and reaps the child: drive the
		// transport primitive with a long-lived remote command, then cancel.
		let token = CancellationToken::new();
		let cancel_token = token.clone();
		let connection = SshConnection {
			destination: "fixture".to_string(),
			port:        None,
			key_path:    None,
			options:     vec![
				"-F".into(),
				OsString::from(fixture.ssh_config.display().to_string()),
			],
			env:         Vec::new(),
		};
		let marker = fixture.ssh_config.display().to_string();
		// A current-thread runtime cannot poll the task while this thread polls
		// `/proc`, so drive the cancellation check on a multi-thread runtime.
		let cancel_rt = tokio::runtime::Builder::new_multi_thread()
			.worker_threads(2)
			.enable_all()
			.build()
			.expect("cancel runtime");
		let task = cancel_rt.spawn(async move {
			run_ssh(&connection, "sleep 30", &cancel_token).await
		});
		let deadline = Instant::now() + Duration::from_secs(15);
		let mut pid = None;
		while Instant::now() < deadline {
			pid = find_ssh_child(&marker);
			if pid.is_some() {
				break;
			}
			std::thread::sleep(Duration::from_millis(50));
		}
		let pid = pid.expect("a live ssh child appeared for the fixture config");
		token.cancel();
		let outcome = cancel_rt.block_on(task).expect("join");
		assert!(outcome.is_err(), "cancelled ssh must fail");
		// The child was reaped, not left as a zombie or orphan.
		let deadline = Instant::now() + Duration::from_secs(10);
		while std::path::Path::new(&format!("/proc/{pid}")).exists() && Instant::now() < deadline {
			std::thread::sleep(Duration::from_millis(50));
		}
		assert!(
			!std::path::Path::new(&format!("/proc/{pid}")).exists(),
			"cancelled ssh pid {pid} must be reaped"
		);
	}

	fn command_available(binary: &str) -> bool {
		StdCommand::new(binary)
			.arg("-h")
			.stdout(Stdio::null())
			.stderr(Stdio::null())
			.status()
			.is_ok()
	}

	/// Find a live local `ssh` whose argv carries the fixture config path.
	fn find_ssh_child(marker: &str) -> Option<u32> {
		let entries = fs::read_dir("/proc").ok()?;
		for entry in entries.flatten() {
			let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
				continue;
			};
			if pid == std::process::id() {
				continue;
			}
			let Ok(command_line) = fs::read(entry.path().join("cmdline")) else {
				continue;
			};
			let command_line = String::from_utf8_lossy(&command_line);
			if command_line.contains("ssh") && command_line.contains(marker) {
				return Some(pid);
			}
		}
		None
	}

	/// Ephemeral loopback sshd: throwaway host/client keys, `known_hosts` pinned
	/// in the fixture `ssh_config` (`StrictHostKeyChecking yes`), no user config.
	struct LiveSshd {
		dir:         TempDir,
		child:       std::process::Child,
		remote:      PathBuf,
		ssh_config:  PathBuf,
		closed_port: u16,
	}

	impl LiveSshd {
		fn start() -> Self {
			let dir = TempDir::new("live");
			let port = free_port();
			let closed_port = free_port();
			let client_key = dir.0.join("client_key");
			let host_key = dir.0.join("host_key");
			for key in [&client_key, &host_key] {
				let status = StdCommand::new("ssh-keygen")
					.args(["-q", "-t", "ed25519", "-N", "", "-f"])
					.arg(key)
					.status()
					.expect("ssh-keygen");
				assert!(status.success(), "ssh-keygen failed");
			}
			let authorized_keys = dir.0.join("authorized_keys");
			fs::copy(client_key.with_extension("pub"), &authorized_keys).expect("authorized_keys");
			let known_hosts = dir.0.join("known_hosts");
			let host_pub = fs::read_to_string(host_key.with_extension("pub")).expect("host pub");
			fs::write(
				&known_hosts,
				format!("[127.0.0.1]:{port} {host_pub}"),
			)
			.expect("known_hosts");
			let user = current_user();
			let ssh_config = dir.0.join("ssh_config");
			fs::write(
				&ssh_config,
				format!(
					"Host fixture\n  HostName 127.0.0.1\n  Port {port}\n  User {user}\n  \
					 IdentityFile {}\n  IdentitiesOnly yes\n  UserKnownHostsFile {}\n  \
					 StrictHostKeyChecking yes\n  BatchMode yes\n  LogLevel ERROR\n",
					client_key.display(),
					known_hosts.display()
				),
			)
			.expect("ssh_config");
			let sshd_config = dir.0.join("sshd_config");
			fs::write(
				&sshd_config,
				format!(
					"Port {port}\nListenAddress 127.0.0.1\nHostKey {}\nPidFile {}\n\
					 AuthorizedKeysFile {}\nPasswordAuthentication no\n\
					 KbdInteractiveAuthentication no\nUsePAM no\nStrictModes no\n\
					 AllowTcpForwarding no\nPermitRootLogin no\nLogLevel ERROR\n",
					host_key.display(),
					dir.0.join("sshd.pid").display(),
					authorized_keys.display()
				),
			)
			.expect("sshd_config");
			let sshd_log = dir.0.join("sshd.log");
			let child = StdCommand::new("sshd")
				.args(["-D", "-f"])
				.arg(&sshd_config)
				.arg("-E")
				.arg(&sshd_log)
				.stdin(Stdio::null())
				.stdout(Stdio::null())
				.stderr(Stdio::null())
				.spawn()
				.expect("spawn sshd");
			wait_for_port(port);
			let remote = dir.0.join("remote");
			fs::create_dir_all(remote.join("listing").join("beta")).expect("remote dirs");
			fs::create_dir_all(remote.join("sub dir")).expect("quoted dir");
			fs::write(remote.join("plain.txt"), "fixture text\n").expect("plain");
			fs::write(remote.join("sub dir").join("it's quoted.txt"), "quoted name file\n")
				.expect("quoted");
			fs::write(remote.join("bin.dat"), [0xff, 0x00, 0xfe]).expect("binary");
			fs::write(remote.join("big.txt"), vec![b'a'; SSH_TEXT_MAX_BYTES + 16]).expect("big");
			fs::write(remote.join("listing").join("alpha.txt"), "a\n").expect("alpha");
			fs::write(remote.join("listing").join("beta").join("deep.txt"), "d\n").expect("deep");
			let fifo = std::ffi::CString::new(
				remote.join("pipe").to_string_lossy().as_bytes(),
			)
			.expect("fifo path");
			// SAFETY: NUL-terminated path inside a directory this fixture created.
			let made = unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) };
			assert_eq!(made, 0, "mkfifo");
			Self { dir, child, remote, ssh_config, closed_port }
		}
	}

	impl Drop for LiveSshd {
		fn drop(&mut self) {
			let _ = self.child.kill();
			let _ = self.child.wait();
			let _ = fs::remove_dir_all(&self.dir.0);
		}
	}

	fn free_port() -> u16 {
		let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
		listener.local_addr().expect("addr").port()
	}

	fn wait_for_port(port: u16) {
		let deadline = Instant::now() + Duration::from_secs(15);
		while Instant::now() < deadline {
			if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
				return;
			}
			std::thread::sleep(Duration::from_millis(50));
		}
		panic!("sshd did not listen on 127.0.0.1:{port}");
	}

	fn current_user() -> String {
		let output = StdCommand::new("id").arg("-un").output().expect("id -un");
		String::from_utf8_lossy(&output.stdout).trim().to_string()
	}
}
