//! Unix domain socket transport.

use std::{
	ffi::OsStr,
	fs::{self, DirBuilder, OpenOptions, Permissions},
	io::{self, IoSlice},
	mem,
	os::{
		fd::{AsRawFd, FromRawFd, OwnedFd},
		unix::{
			ffi::OsStrExt,
			fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
			net::UnixStream as StdUnixStream,
		},
	},
	path::{Path, PathBuf},
	time::Duration,
};

use tokio::net::{UnixListener, UnixStream};

use super::{IpcError, endpoint};

/// `listen()` backlog.
const BACKLOG: libc::c_int = 512;

/// Effective uid of this process.
pub fn euid() -> u32 {
	// SAFETY: `geteuid` has no preconditions and cannot fail.
	unsafe { libc::geteuid() }
}

/// Errno-style name for a raw OS error.
pub const fn os_error_code(errno: i32) -> Option<&'static str> {
	Some(match errno {
		libc::ENOENT => "ENOENT",
		libc::EACCES => "EACCES",
		libc::EPERM => "EPERM",
		libc::ECONNREFUSED => "ECONNREFUSED",
		libc::ECONNRESET => "ECONNRESET",
		libc::ECONNABORTED => "ECONNABORTED",
		libc::ENOTCONN => "ENOTCONN",
		libc::EADDRINUSE => "EADDRINUSE",
		libc::EADDRNOTAVAIL => "EADDRNOTAVAIL",
		libc::EPIPE => "EPIPE",
		libc::ETIMEDOUT => "ETIMEDOUT",
		libc::EAGAIN => "EAGAIN",
		libc::EINTR => "EINTR",
		libc::EINVAL => "EINVAL",
		libc::EEXIST => "EEXIST",
		libc::ENOTDIR => "ENOTDIR",
		libc::EISDIR => "EISDIR",
		libc::ELOOP => "ELOOP",
		libc::ENAMETOOLONG => "ENAMETOOLONG",
		libc::EMFILE => "EMFILE",
		libc::ENFILE => "ENFILE",
		libc::ENOMEM => "ENOMEM",
		libc::ENOSPC => "ENOSPC",
		libc::EROFS => "EROFS",
		libc::ENOTSOCK => "ENOTSOCK",
		libc::EPROTOTYPE => "EPROTOTYPE",
		libc::EMSGSIZE => "EMSGSIZE",
		libc::ENOBUFS => "ENOBUFS",
		libc::EBADF => "EBADF",
		libc::EIO => "EIO",
		_ => return None,
	})
}

/// Unix reports a vanished peer as EOF or an explicit error; neither is an
/// orderly close beyond `read() == 0`.
pub const fn is_orderly_eof(_error: &io::Error) -> bool {
	false
}

/// Socket file this server bound, identified by inode so close never unlinks a
/// successor's socket.
pub struct Bound {
	path: PathBuf,
	dev:  u64,
	ino:  u64,
}

/// Unlink the socket file if it is still the one this server bound.
pub fn release(bound: Bound) {
	if let Ok(meta) = fs::symlink_metadata(&bound.path)
		&& meta.dev() == bound.dev
		&& meta.ino() == bound.ino
	{
		let _ = fs::remove_file(&bound.path);
	}
}

/// Accepting side of a bound socket.
pub struct Listener(UnixListener);

impl Listener {
	/// Next connection whose peer runs as our effective uid.
	#[allow(
		clippy::needless_pass_by_ref_mut,
		reason = "shared shape: Windows swaps in its next pending pipe instance"
	)]
	pub async fn accept(&mut self) -> io::Result<Stream> {
		let euid = euid();
		loop {
			let (stream, _) = self.0.accept().await?;
			// Foreign peers are dropped silently; the 0600 socket already keeps
			// them out on any filesystem honoring permissions.
			if stream.peer_cred().is_ok_and(|cred| cred.uid() == euid) {
				return Ok(Stream(stream));
			}
		}
	}
}

/// Bind `endpoint` (see `IpcServer.listen`).
pub async fn listen(endpoint: &str) -> Result<(Listener, Bound), IpcError> {
	let path = PathBuf::from(endpoint);
	let (std_listener, bound) = napi::bindgen_prelude::spawn_blocking(move || bind_locked(path))
		.await
		.map_err(|error| IpcError::new("EIO", format!("bind task failed: {error}")))??;
	let listener = UnixListener::from_std(std_listener)
		.map_err(|error| IpcError::io(&error, "register listener"))?;
	Ok((Listener(listener), bound))
}

fn bind_locked(path: PathBuf) -> Result<(std::os::unix::net::UnixListener, Bound), IpcError> {
	let display = path.display().to_string();
	prepare_parent(&path)?;

	let mut lock_path = path.clone().into_os_string();
	lock_path.push(".bind.lock");
	let lock = OpenOptions::new()
		.read(true)
		.write(true)
		.create(true)
		.truncate(false)
		.mode(0o600)
		.open(&lock_path)
		.map_err(|error| {
			IpcError::io(&error, &format!("open {}", Path::new(&lock_path).display()))
		})?;
	loop {
		match lock.lock() {
			Ok(()) => break,
			Err(error) if error.kind() == io::ErrorKind::Interrupted => {},
			Err(error) => return Err(IpcError::io(&error, "lock bind sidecar")),
		}
	}

	match fs::symlink_metadata(&path) {
		Ok(meta) if !meta.file_type().is_socket() => {
			return Err(IpcError::new("EADDRINUSE", format!("{display} exists and is not a socket")));
		},
		Ok(_) => match StdUnixStream::connect(&path) {
			Ok(_) => {
				return Err(IpcError::new(
					"EADDRINUSE",
					format!("{display} already has a live listener"),
				));
			},
			Err(error)
				if matches!(
					error.kind(),
					io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
				) =>
			{
				match fs::remove_file(&path) {
					Ok(()) => {},
					Err(error) if error.kind() == io::ErrorKind::NotFound => {},
					Err(error) => return Err(IpcError::io(&error, &format!("remove stale {display}"))),
				}
			},
			Err(error) => return Err(IpcError::io(&error, &format!("probe {display}"))),
		},
		Err(error) if error.kind() == io::ErrorKind::NotFound => {},
		Err(error) => return Err(IpcError::io(&error, &format!("stat {display}"))),
	}

	let socket =
		bind_private(&path).map_err(|error| IpcError::io(&error, &format!("bind {display}")))?;
	let meta = fs::symlink_metadata(&path)
		.map_err(|error| IpcError::io(&error, &format!("stat {display}")))?;
	let bound = Bound { path, dev: meta.dev(), ino: meta.ino() };
	// SAFETY: `socket` is a bound AF_UNIX stream socket owned by this frame.
	if unsafe { libc::listen(socket.as_raw_fd(), BACKLOG) } != 0 {
		let error = io::Error::last_os_error();
		release(bound);
		return Err(IpcError::io(&error, &format!("listen {display}")));
	}
	let listener = std::os::unix::net::UnixListener::from(socket);
	if let Err(error) = listener.set_nonblocking(true) {
		release(bound);
		return Err(IpcError::io(&error, "set listener nonblocking"));
	}
	drop(lock);
	Ok((listener, bound))
}

/// Create an `AF_UNIX` stream socket bound at `path` with mode 0600.
///
/// The mode is applied between `bind()` and `listen()`: until `listen()` every
/// connect is refused, so there is no window in which a wider mode is usable.
fn bind_private(path: &Path) -> io::Result<OwnedFd> {
	let address = socket_address(path.as_os_str())?;
	// SAFETY: plain socket(2) call; the result is checked before use.
	let raw = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
	if raw < 0 {
		return Err(io::Error::last_os_error());
	}
	// SAFETY: `raw` is a fresh descriptor owned by nothing else.
	let socket = unsafe { OwnedFd::from_raw_fd(raw) };
	// SAFETY: `socket` is live; FD_CLOEXEC only affects exec inheritance.
	if unsafe { libc::fcntl(socket.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
		return Err(io::Error::last_os_error());
	}
	// SAFETY: `address.0` is a fully initialized sockaddr_un and `address.1`
	// is the exact length bind(2) must read from it.
	let status = unsafe {
		libc::bind(socket.as_raw_fd(), (&raw const address.0).cast::<libc::sockaddr>(), address.1)
	};
	if status != 0 {
		return Err(io::Error::last_os_error());
	}
	if let Err(error) = fs::set_permissions(path, Permissions::from_mode(0o600)) {
		let _ = fs::remove_file(path);
		return Err(error);
	}
	Ok(socket)
}

fn socket_address(path: &OsStr) -> io::Result<(libc::sockaddr_un, libc::socklen_t)> {
	// SAFETY: sockaddr_un is plain old data; all-zero is a valid value.
	let mut address: libc::sockaddr_un = unsafe { mem::zeroed() };
	let bytes = path.as_bytes();
	if bytes.len() >= address.sun_path.len() || bytes.contains(&0) {
		return Err(io::Error::new(
			io::ErrorKind::InvalidInput,
			"socket path does not fit sockaddr_un (use ipcEndpoint)",
		));
	}
	address.sun_family = libc::AF_UNIX as libc::sa_family_t;
	for (slot, byte) in address.sun_path.iter_mut().zip(bytes) {
		*slot = *byte as libc::c_char;
	}
	let len = mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1;
	#[cfg(any(
		target_os = "macos",
		target_os = "ios",
		target_os = "freebsd",
		target_os = "openbsd",
		target_os = "netbsd",
		target_os = "dragonfly"
	))]
	{
		address.sun_len = len as u8;
	}
	Ok((address, len as libc::socklen_t))
}

/// Make sure the socket's parent directory exists (0700 when created). The
/// shared relocation directory must additionally be a real directory owned by
/// us with no group/other access.
fn prepare_parent(path: &Path) -> Result<(), IpcError> {
	let Some(parent) = path.parent().filter(|parent| !parent.is_empty()) else {
		return Ok(());
	};
	let euid = euid();
	if parent == Path::new(&endpoint::relocation_dir(euid)) {
		return ensure_private_dir(parent, euid);
	}
	DirBuilder::new()
		.recursive(true)
		.mode(0o700)
		.create(parent)
		.map_err(|error| IpcError::io(&error, &format!("create {}", parent.display())))
}

/// uid/gid owning Android's system-managed data tree (`/data/data`).
#[cfg(any(target_os = "android", test))]
const ANDROID_SYSTEM_UID: u32 = 1000;
#[cfg(any(target_os = "android", test))]
const ANDROID_SYSTEM_GID: u32 = 1000;

/// Create/reuse the shared relocation directory.
///
/// `dir` is `<base>/omp-<euid>` and the only component this process creates, so
/// it must be a real directory of ours with mode `0700`. On Android the whole
/// lexical base path is validated first (see [`ensure_trusted_base`]).
fn ensure_private_dir(dir: &Path, euid: u32) -> Result<(), IpcError> {
	#[cfg(target_os = "android")]
	{
		let Some(base) = dir.parent().filter(|base| !base.as_os_str().is_empty()) else {
			return Err(relocation_error(dir, "has no parent directory"));
		};
		ensure_trusted_base(base, euid)?;
	}
	match DirBuilder::new().mode(0o700).create(dir) {
		Ok(()) => {},
		Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {},
		Err(error) => return Err(IpcError::io(&error, &format!("create {}", dir.display()))),
	}
	let meta = fs::symlink_metadata(dir)
		.map_err(|error| IpcError::io(&error, &format!("stat {}", dir.display())))?;
	if !meta.file_type().is_dir() {
		return Err(relocation_error(dir, "exists and is not a directory"));
	}
	if meta.uid() != euid {
		return Err(relocation_error(
			dir,
			format!("is owned by uid {} instead of {euid}", meta.uid()),
		));
	}
	// We own it, so any mode other than the required `0700` is repaired: `0500`
	// or `0600` would accept the directory and then fail when the socket file is
	// created, and group/other access is never needed.
	if meta.mode() & 0o777 != 0o700 {
		fs::set_permissions(dir, Permissions::from_mode(0o700))
			.map_err(|error| IpcError::io(&error, &format!("chmod {}", dir.display())))?;
	}
	Ok(())
}

/// `EACCES` naming the relocation directory and the reason it is unusable.
fn relocation_error(dir: &Path, reason: impl std::fmt::Display) -> IpcError {
	IpcError::new(
		"EACCES",
		format!("no usable IPC relocation directory {}: {reason}", dir.display()),
	)
}

/// Validate the base directory of the relocation leaf.
///
/// Every component of the absolute, normalized path is checked lexically with
/// `symlink_metadata`, so nothing is resolved and there is no second path an
/// attacker could swap between validation and use. A symlinked install root is
/// rejected rather than followed. Each component must be owned by us, root, or
/// Android's system uid, and must not be writable by another uid; the base
/// itself must additionally accept a new entry.
#[cfg(any(target_os = "android", test))]
fn ensure_trusted_base(base: &Path, euid: u32) -> Result<(), IpcError> {
	if !base.is_absolute() {
		return Err(relocation_error(base, "is not an absolute path"));
	}
	// SAFETY: `getegid` has no preconditions and cannot fail.
	let egid = unsafe { libc::getegid() };
	let mut path = PathBuf::from("/");
	for component in base.components() {
		match component {
			std::path::Component::RootDir => continue,
			std::path::Component::Normal(part) => path.push(part),
			_ => return Err(relocation_error(base, "is not a normalized path")),
		}
		let meta = fs::symlink_metadata(&path).map_err(|error| {
			relocation_error(base, format!("cannot stat {}: {error}", path.display()))
		})?;
		if !meta.file_type().is_dir() {
			return Err(relocation_error(base, format!("{} is not a real directory", path.display())));
		}
		if !trusted_component(meta.uid(), meta.gid(), meta.mode(), euid, egid) {
			return Err(relocation_error(
				base,
				format!(
					"{} is not trusted (uid {} mode {:o})",
					path.display(),
					meta.uid(),
					meta.mode() & 0o7777
				),
			));
		}
		if path.as_path() == base && !base_accepts_new_entries(meta.uid(), meta.mode(), euid) {
			return Err(relocation_error(base, "is not writable by this uid"));
		}
	}
	Ok(())
}

/// Whether a directory's owner **and** write permissions keep our leaf safe.
///
/// The owner must be us, root, or Android's system uid — any other owner can
/// `chmod` and rename its own entries regardless of mode. Ownership alone is
/// not enough: a non-sticky directory writable by a group or by everyone lets a
/// non-owner replace the leaf after validation. Only the sticky bit makes a
/// write-open directory safe, because then only the entry owner (or root) can
/// rename or remove our leaf, which is the `/tmp` rule. Group write is accepted
/// only for groups that cannot contain another app: our own group and the
/// Android system group (the `/data/data` tree is `0771 system:system`).
#[cfg(any(target_os = "android", test))]
fn trusted_component(uid: u32, gid: u32, mode: u32, euid: u32, egid: u32) -> bool {
	let owner_trusted = uid == euid || uid == 0 || uid == ANDROID_SYSTEM_UID;
	if !owner_trusted {
		return false;
	}
	if mode & 0o1000 != 0 {
		return true;
	}
	if mode & 0o002 != 0 {
		return false;
	}
	if mode & 0o020 != 0 {
		return gid == egid || gid == 0 || gid == ANDROID_SYSTEM_GID;
	}
	true
}

/// Whether the base lets this uid create the per-user entry.
///
/// An app-owned base needs owner write and search. A root/system-owned base is
/// usable only when it is sticky and world-writable (`/tmp`); root-owned `0755`
/// cannot hold the entry for an ordinary app uid.
#[cfg(any(target_os = "android", test))]
fn base_accepts_new_entries(uid: u32, mode: u32, euid: u32) -> bool {
	if uid == euid {
		return mode & 0o300 == 0o300;
	}
	(uid == 0 || uid == ANDROID_SYSTEM_UID) && mode & 0o1003 == 0o1003
}

/// Dial `endpoint` and verify the server runs as our effective uid.
pub async fn connect(endpoint: &str, _timeout: Duration) -> Result<Stream, IpcError> {
	let stream = UnixStream::connect(endpoint)
		.await
		.map_err(|error| IpcError::io(&error, &format!("connect {endpoint}")))?;
	let cred = stream
		.peer_cred()
		.map_err(|error| IpcError::io(&error, "read peer credentials"))?;
	let euid = euid();
	if cred.uid() != euid {
		return Err(IpcError::new(
			"EACCES",
			format!("{endpoint} is served by uid {}, expected {euid}", cred.uid()),
		));
	}
	Ok(Stream(stream))
}

/// How [`Stream::finish_write`] left the stream.
pub enum WriteFinish {
	/// Write side shut; reading continues until EOF.
	HalfClosed,
	/// The whole stream must be dropped now.
	#[allow(dead_code, reason = "Windows-only outcome kept in the shared shape")]
	Closed,
}

/// Connected socket.
pub struct Stream(UnixStream);

impl Stream {
	pub async fn readable(&self) -> io::Result<()> {
		self.0.readable().await
	}

	/// Read into `buf`'s spare capacity without initializing it first.
	pub fn try_read_buf(&self, buf: &mut Vec<u8>) -> io::Result<usize> {
		self.0.try_read_buf(buf)
	}

	pub async fn writable(&self) -> io::Result<()> {
		self.0.writable().await
	}

	pub fn try_write_vectored(&self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
		self.0.try_write_vectored(bufs)
	}

	/// Shut the write half; the peer reads EOF after the queued bytes.
	pub fn finish_write(&self) -> io::Result<WriteFinish> {
		// SAFETY: the descriptor is owned by `self.0` and live for this call.
		if unsafe { libc::shutdown(self.0.as_raw_fd(), libc::SHUT_WR) } != 0 {
			return Err(io::Error::last_os_error());
		}
		Ok(WriteFinish::HalfClosed)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn scratch(tag: &str) -> PathBuf {
		let dir = std::env::temp_dir().join(format!("omp-ipc-{}-{tag}", std::process::id()));
		let _ = fs::remove_dir_all(&dir);
		fs::create_dir_all(&dir).unwrap();
		dir
	}

	fn leaf(base: &Path) -> PathBuf {
		base.join(format!("omp-{}", euid()))
	}

	#[test]
	fn ensure_private_dir_creates_leaf_0700() {
		let base = scratch("create");
		let dir = leaf(&base);
		ensure_private_dir(&dir, euid()).unwrap();
		let meta = fs::symlink_metadata(&dir).unwrap();
		assert!(meta.file_type().is_dir());
		assert_eq!(meta.mode() & 0o777, 0o700);
		fs::remove_dir_all(base).unwrap();
	}

	#[test]
	fn ensure_private_dir_repairs_any_non_0700_owner_mode() {
		for mode in [0o500, 0o600, 0o200, 0o705] {
			let base = scratch(&format!("repair-{mode:o}"));
			let dir = leaf(&base);
			fs::create_dir(&dir).unwrap();
			fs::set_permissions(&dir, Permissions::from_mode(mode)).unwrap();
			ensure_private_dir(&dir, euid()).unwrap();
			assert_eq!(fs::symlink_metadata(&dir).unwrap().mode() & 0o777, 0o700);
			fs::remove_dir_all(base).unwrap();
		}
	}

	#[test]
	fn ensure_private_dir_rejects_non_directory_leaf() {
		let base = scratch("file");
		let dir = leaf(&base);
		fs::write(&dir, b"x").unwrap();
		let error = ensure_private_dir(&dir, euid()).unwrap_err();
		assert_eq!(error.code, "EACCES");
		assert!(error.message.contains("not a directory"), "{}", error.message);
		fs::remove_dir_all(base).unwrap();
	}

	#[test]
	fn ensure_private_dir_rejects_symlink_leaf() {
		let base = scratch("symlink");
		let target = base.join("target");
		fs::create_dir(&target).unwrap();
		let dir = leaf(&base);
		std::os::unix::fs::symlink(&target, &dir).unwrap();
		let error = ensure_private_dir(&dir, euid()).unwrap_err();
		assert_eq!(error.code, "EACCES");
		assert!(error.message.contains("not a directory"), "{}", error.message);
		fs::remove_dir_all(base).unwrap();
	}

	#[cfg(any(target_os = "android", target_os = "linux"))]
	#[test]
	fn ensure_trusted_base_accepts_app_private_chain() {
		let base = scratch("trusted");
		assert!(ensure_trusted_base(&base, euid()).is_ok());
		fs::remove_dir_all(base).unwrap();
	}

	#[cfg(any(target_os = "android", target_os = "linux"))]
	#[test]
	fn ensure_trusted_base_rejects_world_writable_base() {
		let base = scratch("open");
		fs::set_permissions(&base, Permissions::from_mode(0o777)).unwrap();
		let error = ensure_trusted_base(&base, euid()).unwrap_err();
		assert_eq!(error.code, "EACCES");
		assert!(error.message.contains("no usable IPC relocation directory"), "{}", error.message);
		fs::set_permissions(&base, Permissions::from_mode(0o700)).unwrap();
		fs::remove_dir_all(base).unwrap();
	}

	#[cfg(any(target_os = "android", target_os = "linux"))]
	#[test]
	fn ensure_trusted_base_rejects_symlink_component() {
		let root = scratch("link");
		let real = root.join("real");
		fs::create_dir(&real).unwrap();
		let link = root.join("link");
		std::os::unix::fs::symlink(&real, &link).unwrap();
		let error = ensure_trusted_base(&link, euid()).unwrap_err();
		assert_eq!(error.code, "EACCES");
		assert!(error.message.contains("not a real directory"), "{}", error.message);
		fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn ensure_trusted_base_requires_normalized_absolute_path() {
		assert!(ensure_trusted_base(Path::new("relative/path"), euid()).is_err());
		assert!(ensure_trusted_base(Path::new("/tmp/../etc"), euid()).is_err());
	}

	#[test]
	fn trusted_component_requires_trusted_owner_and_safe_writes() {
		let euid = 501;
		let egid = 501;
		assert!(trusted_component(euid, egid, 0o700, euid, egid));
		// Own group may write (Android app dirs are 0771); a foreign group may not.
		assert!(trusted_component(euid, egid, 0o770, euid, egid));
		assert!(!trusted_component(euid, 20, 0o770, euid, egid));
		assert!(!trusted_component(euid, egid, 0o702, euid, egid));
		// A foreign owner can chmod/rename its own entries, whatever the mode.
		assert!(!trusted_component(502, 502, 0o755, euid, egid));
		assert!(!trusted_component(502, 502, 0o555, euid, egid));
		// Owner alone is not enough: non-sticky group/other write is replaceable.
		assert!(!trusted_component(0, 0, 0o777, euid, egid));
		assert!(!trusted_component(ANDROID_SYSTEM_UID, ANDROID_SYSTEM_GID, 0o777, euid, egid));
		assert!(!trusted_component(euid, egid, 0o777, euid, egid));
		// Sticky write-open directories only let the entry owner rename (the /tmp rule).
		assert!(trusted_component(0, 0, 0o1777, euid, egid));
		assert!(trusted_component(euid, egid, 0o1770, euid, egid));
		// Android's system tree is 0771 system:system; that group cannot hold apps.
		assert!(trusted_component(ANDROID_SYSTEM_UID, ANDROID_SYSTEM_GID, 0o771, euid, egid));
		assert!(!trusted_component(ANDROID_SYSTEM_UID, 20, 0o770, euid, egid));
		// Read-only foreign-group modes are safe without the sticky bit.
		assert!(trusted_component(euid, 20, 0o750, euid, egid));
		assert!(trusted_component(0, 20, 0o755, euid, egid));
	}

	#[test]
	fn base_accepts_new_entries_requires_owner_write_and_search() {
		let euid = 501;
		assert!(base_accepts_new_entries(euid, 0o700, euid));
		assert!(!base_accepts_new_entries(euid, 0o600, euid));
		assert!(!base_accepts_new_entries(euid, 0o500, euid));
		// Root/system bases qualify only as sticky world-writable (`/tmp`), never
		// as root-owned 0755 for an ordinary app uid.
		assert!(base_accepts_new_entries(0, 0o1777, euid));
		assert!(!base_accepts_new_entries(0, 0o755, euid));
		assert!(!base_accepts_new_entries(0, 0o777, euid));
		// Sticky but not searchable for others cannot accept our entry.
		assert!(!base_accepts_new_entries(0, 0o1002, euid));
		assert!(!base_accepts_new_entries(ANDROID_SYSTEM_UID, 0o777, euid));
		assert!(base_accepts_new_entries(ANDROID_SYSTEM_UID, 0o1777, euid));
	}

	/// Exercise the real install-root ancestry: every lexical component of the
	/// host's relocation base (`/tmp` on Linux CI, `<prefix>/tmp` on Android)
	/// must satisfy the owner/mode policy. macOS is excluded because its `/tmp`
	/// is a symlink the policy intentionally rejects.
	#[cfg(any(target_os = "android", target_os = "linux"))]
	#[test]
	fn android_install_root_ancestry_is_trusted() {
		let euid = euid();
		let dir = PathBuf::from(endpoint::relocation_dir(euid));
		let Some(base) = dir.parent() else { return };
		if !base.is_dir() {
			eprintln!("SKIP: {} is absent on this host", base.display());
			return;
		}
		ensure_trusted_base(base, euid).unwrap();
	}
}
