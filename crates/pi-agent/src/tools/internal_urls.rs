//! Internal URL router: `scheme://` resources resolved by protocol handlers.
//!
//! Mirrors `packages/coding-agent/src/internal-urls/router.ts`: the `read` and
//! `write` tools consult one router before touching the filesystem, handlers
//! register per scheme, reserved OMP namespaces cannot be replaced, and a
//! scheme with no handler is an explicit protocol error instead of a path.
//!
//! No transport lives here: RPC host schemes register an adapter handler that
//! speaks `host_uri_request`/`host_uri_result` frames, and any other embedder
//! can register its own. That keeps `pi-agent` free of RPC dependencies.

use std::{
	collections::BTreeMap,
	path::{Path, PathBuf},
	sync::{Arc, Mutex, MutexGuard},
};

use async_trait::async_trait;

use super::hashline::snapshot::SNAPSHOT_MAX_BYTES;
use crate::CancellationToken;

/// Built-in scheme for directory roots a session binds in
/// [`UrlScope::local_roots`].
pub const LOCAL_SCHEME: &str = "local";

/// OMP-owned namespaces a host may never replace: `security://` is reserved for
/// the security review surface, `local://` for the built-in root handler.
pub const RESERVED_SCHEMES: [&str; 2] = ["local", "security"];

/// One parsed internal URL (`skill://plugin:name` style colons are preserved in
/// the path because only the first `://` separates the scheme).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InternalUrl {
	pub scheme: String,
	/// Verbatim input, used as the resource identity.
	pub href:   String,
	pub host:   String,
	pub path:   String,
}

impl InternalUrl {
	#[must_use]
	pub fn parse(input: &str) -> Option<Self> {
		let scheme = scheme_of(input)?;
		let rest = &input[scheme.len() + 3..];
		let (host, path) = match rest.find('/') {
			Some(index) => (&rest[..index], &rest[index..]),
			None => (rest, ""),
		};
		Some(Self {
			scheme,
			href: input.to_string(),
			host: host.to_string(),
			path: path.to_string(),
		})
	}
}

/// Scheme of a `scheme://` string, lowercased; `None` for a plain path.
#[must_use]
pub fn scheme_of(raw: &str) -> Option<String> {
	let (scheme, _) = raw.split_once("://")?;
	let mut chars = scheme.chars();
	let first = chars.next()?;
	if !first.is_ascii_alphabetic() {
		return None;
	}
	if !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) {
		return None;
	}
	Some(scheme.to_ascii_lowercase())
}

/// A scheme a host may register: lowercase letter first, then `a-z0-9+-.`.
#[must_use]
pub fn is_valid_scheme(scheme: &str) -> bool {
	let mut chars = scheme.chars();
	let Some(first) = chars.next() else {
		return false;
	};
	if !first.is_ascii_lowercase() {
		return false;
	}
	chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '+' | '.' | '-'))
}

/// Whether `scheme` is an OMP namespace hosts may not replace.
#[must_use]
pub fn is_reserved_scheme(scheme: &str) -> bool {
	RESERVED_SCHEMES.contains(&scheme)
}

/// One resolved internal resource (`InternalResource` upstream).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InternalResource {
	pub url:           String,
	pub content:       String,
	/// `text/markdown`, `application/json` or `text/plain`.
	pub content_type:  String,
	pub size:          usize,
	pub source_path:   Option<PathBuf>,
	pub notes:         Option<Vec<String>>,
	pub immutable:     bool,
	pub is_directory:  bool,
}

impl InternalResource {
	#[must_use]
	pub fn text(url: &str, content: String) -> Self {
		Self {
			url: url.to_string(),
			size: content.len(),
			content,
			content_type: "text/plain".to_string(),
			source_path: None,
			notes: None,
			immutable: false,
			is_directory: false,
		}
	}
}

/// Immutable session scope handed to handlers with every call. Callers build
/// the context themselves, so concurrent turns and children never share or
/// overwrite each other's scope.
#[derive(Debug, Clone, Default)]
pub struct UrlScope {
	pub session_file: Option<PathBuf>,
	/// Scheme to directory bindings (e.g. `local` to the session artifacts dir).
	pub local_roots:  BTreeMap<String, PathBuf>,
}

impl UrlScope {
	#[must_use]
	pub fn resolve_context(&self, cwd: &Path, signal: &CancellationToken) -> ResolveContext {
		ResolveContext {
			cwd:          cwd.to_path_buf(),
			session_file: self.session_file.clone(),
			local_roots:  self.local_roots.clone(),
			signal:       signal.clone(),
		}
	}

	#[must_use]
	pub fn write_context(&self, cwd: &Path, signal: &CancellationToken) -> WriteContext {
		WriteContext {
			cwd:         cwd.to_path_buf(),
			local_roots: self.local_roots.clone(),
			signal:      signal.clone(),
		}
	}
}

/// Read context for one `resolve` call.
#[derive(Debug, Clone, Default)]
pub struct ResolveContext {
	pub cwd:          PathBuf,
	pub session_file: Option<PathBuf>,
	pub local_roots:  BTreeMap<String, PathBuf>,
	pub signal:       CancellationToken,
}

/// Write context for one `write` call.
#[derive(Debug, Clone, Default)]
pub struct WriteContext {
	pub cwd:         PathBuf,
	pub local_roots: BTreeMap<String, PathBuf>,
	pub signal:      CancellationToken,
}

/// One scheme handler. `resolve` is required; `write` is installed only when
/// [`ProtocolHandler::writable`] is true, which is what makes a read-only
/// scheme reject writes before any host round-trip.
#[async_trait]
pub trait ProtocolHandler: Send + Sync {
	fn scheme(&self) -> &str;

	fn immutable(&self) -> bool {
		false
	}

	fn writable(&self) -> bool {
		false
	}

	async fn resolve(
		&self,
		url: &InternalUrl,
		context: &ResolveContext,
	) -> Result<InternalResource, String>;

	async fn write(
		&self,
		url: &InternalUrl,
		_content: &str,
		_context: &WriteContext,
	) -> Result<(), String> {
		Err(read_only_error(&url.scheme))
	}
}

/// Error text for a write to a scheme without a write handler.
#[must_use]
pub fn read_only_error(scheme: &str) -> String {
	format!("{scheme}:// URLs are read-only for write; use the protocol-specific tool for mutations.")
}

/// Error text for a scheme no handler registered.
#[must_use]
pub fn unknown_protocol_error(scheme: &str, supported: &[String]) -> String {
	let list = if supported.is_empty() {
		"none".to_string()
	} else {
		let mut schemes: Vec<String> = supported
			.iter()
			.map(|supported| format!("{supported}://"))
			.collect();
		schemes.sort();
		schemes.join(", ")
	};
	format!("Unknown protocol: {scheme}://\nSupported: {list}")
}

/// Scheme registry consulted by `read`/`write` before the filesystem. Holds
/// handlers only: every call carries its own context.
pub struct InternalUrlRouter {
	handlers: Mutex<BTreeMap<String, Arc<dyn ProtocolHandler>>>,
}

impl Default for InternalUrlRouter {
	/// Every constructor starts with the built-in `local://` handler, so a
	/// session scope root resolves without an extra mount step.
	fn default() -> Self {
		let mut handlers: BTreeMap<String, Arc<dyn ProtocolHandler>> = BTreeMap::new();
		handlers.insert(LOCAL_SCHEME.to_string(), Arc::new(LocalRootHandler));
		Self { handlers: Mutex::new(handlers) }
	}
}

impl std::fmt::Debug for InternalUrlRouter {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("InternalUrlRouter")
			.field("schemes", &self.schemes())
			.finish()
	}
}

impl InternalUrlRouter {
	#[must_use]
	pub fn new() -> Self {
		Self::default()
	}

	/// Register (or replace) one handler; returns the replaced handler.
	///
	/// Reserved namespaces are built in and refuse replacement, so a host can
	/// never displace `local://` or `security://`.
	pub fn register(
		&self,
		handler: Arc<dyn ProtocolHandler>,
	) -> Result<Option<Arc<dyn ProtocolHandler>>, String> {
		let scheme = handler.scheme().to_ascii_lowercase();
		if is_reserved_scheme(&scheme) {
			return Err(format!("{scheme}:// is reserved for built-in handlers"));
		}
		Ok(self.lock_handlers().insert(scheme, handler))
	}

	/// Remove a handler; `false` when the scheme was not registered or is
	/// reserved.
	pub fn unregister(&self, scheme: &str) -> bool {
		let scheme = scheme.to_ascii_lowercase();
		if is_reserved_scheme(&scheme) {
			return false;
		}
		self.lock_handlers().remove(&scheme).is_some()
	}

	#[must_use]
	pub fn get_handler(&self, scheme: &str) -> Option<Arc<dyn ProtocolHandler>> {
		self.lock_handlers().get(&scheme.to_ascii_lowercase()).cloned()
	}

	/// Registered schemes, sorted.
	#[must_use]
	pub fn schemes(&self) -> Vec<String> {
		self.lock_handlers().keys().cloned().collect()
	}

	/// Whether this input is a `scheme://` URL with a registered handler.
	#[must_use]
	pub fn can_handle(&self, input: &str) -> bool {
		scheme_of(input).is_some_and(|scheme| self.get_handler(&scheme).is_some())
	}

	/// Same predicate as [`Self::can_handle`]; kept for parity with upstream
	/// where `canResolve` also accepts MCP resources (none exist natively).
	#[must_use]
	pub fn can_resolve(&self, input: &str) -> bool {
		self.can_handle(input)
	}

	/// Resolve one internal URL through its handler.
	pub async fn resolve(
		&self,
		input: &str,
		context: &ResolveContext,
	) -> Result<InternalResource, String> {
		let url = InternalUrl::parse(input)
			.ok_or_else(|| format!("Invalid internal URL: {input}"))?;
		let handler = self
			.get_handler(&url.scheme)
			.ok_or_else(|| unknown_protocol_error(&url.scheme, &self.schemes()))?;
		let mut resource = handler.resolve(&url, context).await?;
		if resource.url.is_empty() {
			resource.url = url.href;
		}
		resource.immutable = resource.immutable || handler.immutable();
		if resource.size == 0 && !resource.content.is_empty() {
			resource.size = resource.content.len();
		}
		Ok(resource)
	}

	/// Write one internal URL through its handler. A scheme without a write
	/// handler rejects before any transport call.
	pub async fn write(
		&self,
		input: &str,
		content: &str,
		context: &WriteContext,
	) -> Result<(), String> {
		let url = InternalUrl::parse(input)
			.ok_or_else(|| format!("Invalid internal URL: {input}"))?;
		let handler = self
			.get_handler(&url.scheme)
			.ok_or_else(|| unknown_protocol_error(&url.scheme, &self.schemes()))?;
		if !handler.writable() {
			return Err(read_only_error(&url.scheme));
		}
		handler.write(&url, content, context).await
	}

	fn lock_handlers(&self) -> MutexGuard<'_, BTreeMap<String, Arc<dyn ProtocolHandler>>> {
		self.handlers.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
	}
}

/// Read-only handler for the `local://` root a session binds in
/// [`UrlScope::local_roots`].
///
/// Stateless: the root comes from the immutable per-call [`ResolveContext`], so
/// a child's narrowed scope resolves its own tree while every session shares one
/// router. Traversal is descriptor-relative — `O_NOFOLLOW` on the root, on every
/// intermediate directory and on the final file — and the bytes are read from
/// that verified descriptor, never re-opened by path, so a symlink swapped in
/// mid-walk cannot redirect the read outside the root.
pub struct LocalRootHandler;

#[async_trait]
impl ProtocolHandler for LocalRootHandler {
	fn scheme(&self) -> &str {
		LOCAL_SCHEME
	}

	async fn resolve(
		&self,
		url: &InternalUrl,
		context: &ResolveContext,
	) -> Result<InternalResource, String> {
		let Some(root) = context.local_roots.get(LOCAL_SCHEME) else {
			return Err(format!(
				"{}:// has no root in this session scope; bind it in UrlScope::local_roots",
				LOCAL_SCHEME
			));
		};
		let relative = format!("{}{}", url.host, url.path);
		let relative = relative.trim_start_matches('/');
		if relative.is_empty() {
			return Err(format!("{}:// needs a path below the bound root", LOCAL_SCHEME));
		}
		if Path::new(relative)
			.components()
			.any(|component| !matches!(component, std::path::Component::Normal(_)))
		{
			return Err(format!(
				"{} escapes the {}:// root: only relative paths below it resolve",
				url.href, LOCAL_SCHEME
			));
		}
		let root = root.clone();
		let relative = relative.to_string();
		let name = relative.clone();
		let href = url.href.clone();
		let (bytes, size) = tokio::task::spawn_blocking(move || read_contained(&root, &relative))
			.await
			.map_err(|error| format!("{href}: local read task failed: {error}"))??;
		let truncated = u64::try_from(bytes.len()).unwrap_or(u64::MAX) > SNAPSHOT_MAX_BYTES as u64;
		let content = decode_utf8(&bytes, truncated, &url.href)?;
		let mut resource = InternalResource::text(&url.href, content);
		resource.content_type = content_type_for(Path::new(&name)).to_string();
		// The reported size is the file's own length, never the truncated read.
		resource.size = usize::try_from(size).unwrap_or(usize::MAX);
		if truncated {
			resource.notes = Some(vec![format!(
				"read truncated at {} bytes; the file is {} bytes on disk and only the first {} \
				 bytes were returned",
				SNAPSHOT_MAX_BYTES,
				size,
				resource.content.len()
			)]);
		}
		Ok(resource)
	}
}

/// Decode resource bytes as UTF-8; a capped read is cut back to the last valid
/// character boundary instead of failing the whole read.
fn decode_utf8(bytes: &[u8], truncated: bool, href: &str) -> Result<String, String> {
	match std::str::from_utf8(bytes) {
		Ok(text) => Ok(text.to_string()),
		Err(error) if truncated => Ok(std::str::from_utf8(&bytes[..error.valid_up_to()])
			.unwrap_or_default()
			.to_string()),
		Err(_) => Err(format!("{href} is not UTF-8 text; binary resources are not supported")),
	}
}

/// Read `relative` below `root` through descriptors only, capped at
/// [`SNAPSHOT_MAX_BYTES`] plus one byte so the caller can tell it was cut.
#[cfg(unix)]
fn read_contained(root: &Path, relative: &str) -> Result<(Vec<u8>, u64), String> {
	use std::{
		ffi::CString,
		io::Read,
		os::{
			fd::{AsRawFd, FromRawFd, OwnedFd},
			unix::ffi::OsStrExt,
		},
	};

	fn cstring(part: &str) -> Result<CString, String> {
		CString::new(part)
			.map_err(|_| format!("local:// path component contains a NUL: {part:?}"))
	}
	fn last_error() -> std::io::Error {
		std::io::Error::last_os_error()
	}

	let root_c = CString::new(root.as_os_str().as_bytes())
		.map_err(|_| format!("local:// root contains a NUL: {}", root.display()))?;
	// SAFETY: `root_c` is NUL-terminated; the fd returned is owned by this call.
	let root_fd = unsafe {
		libc::open(
			root_c.as_ptr(),
			libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
		)
	};
	if root_fd < 0 {
		return Err(format!("local:// root {}: {}", root.display(), last_error()));
	}
	// SAFETY: `root_fd` is a fresh descriptor this call owns.
	let mut dir = unsafe { OwnedFd::from_raw_fd(root_fd) };

	let mut parts: Vec<&str> = relative.split('/').filter(|part| !part.is_empty()).collect();
	let Some(name) = parts.pop() else {
		return Err(format!("local://{relative} needs a file name"));
	};
	for part in parts {
		let c_part = cstring(part)?;
		// SAFETY: `dir` is open and `c_part` is NUL-terminated.
		let fd = unsafe {
			libc::openat(
				dir.as_raw_fd(),
				c_part.as_ptr(),
				libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
			)
		};
		if fd < 0 {
			return Err(format!("local://{relative}: {}", last_error()));
		}
		// SAFETY: fresh descriptor, ownership moves into the guard.
		dir = unsafe { OwnedFd::from_raw_fd(fd) };
	}

	let c_name = cstring(name)?;
	// SAFETY: `dir` is open and `c_name` is NUL-terminated.
	let fd = unsafe {
		libc::openat(
			dir.as_raw_fd(),
			c_name.as_ptr(),
			// `O_NONBLOCK` keeps a FIFO from parking this blocking worker in
			// `open`; the regular-file check below still rejects it.
			libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
		)
	};
	if fd < 0 {
		return Err(format!("local://{relative}: {}", last_error()));
	}
	// SAFETY: fresh descriptor, ownership moves into the `File`.
	let file = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(fd) });
	let metadata = file.metadata().map_err(|error| format!("local://{relative}: {error}"))?;
	if !metadata.is_file() {
		return Err(format!("local://{relative} is not a regular file"));
	}
	let size = metadata.len();
	let cap = SNAPSHOT_MAX_BYTES as u64 + 1;
	let mut bytes = Vec::with_capacity(usize::try_from(size.min(cap)).unwrap_or(0));
	file.take(cap)
		.read_to_end(&mut bytes)
		.map_err(|error| format!("local://{relative}: {error}"))?;
	Ok((bytes, size))
}

#[cfg(not(unix))]
fn read_contained(_root: &Path, relative: &str) -> Result<(Vec<u8>, u64), String> {
	Err(format!(
		"local://{relative} needs descriptor-relative traversal; unsupported on this platform"
	))
}

/// Content type for an extension the `read` tool can render; everything else is
/// plain text.
fn content_type_for(path: &Path) -> &'static str {
	match path.extension().and_then(std::ffi::OsStr::to_str) {
		Some("md" | "markdown") => "text/markdown",
		Some("json") => "application/json",
		_ => "text/plain",
	}
}

#[cfg(test)]
mod tests {
	use std::sync::atomic::{AtomicUsize, Ordering};

	#[cfg(unix)]
	use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

	use super::*;

	struct Echo {
		scheme:    &'static str,
		immutable: bool,
		writable:  bool,
		writes:    AtomicUsize,
	}

	impl Echo {
		fn new(scheme: &'static str, immutable: bool, writable: bool) -> Arc<Self> {
			Arc::new(Self { scheme, immutable, writable, writes: AtomicUsize::new(0) })
		}
	}

	#[async_trait]
	impl ProtocolHandler for Echo {
		fn scheme(&self) -> &str {
			self.scheme
		}

		fn immutable(&self) -> bool {
			self.immutable
		}

		fn writable(&self) -> bool {
			self.writable
		}

		async fn resolve(
			&self,
			url: &InternalUrl,
			context: &ResolveContext,
		) -> Result<InternalResource, String> {
			let cwd = context.cwd.display();
			Ok(InternalResource::text(&url.href, format!("{cwd}|{}", url.host)))
		}

		async fn write(
			&self,
			_url: &InternalUrl,
			content: &str,
			_context: &WriteContext,
		) -> Result<(), String> {
			self.writes.fetch_add(1, Ordering::SeqCst);
			if content.is_empty() {
				return Err("empty".to_string());
			}
			Ok(())
		}
	}

	fn runtime() -> tokio::runtime::Runtime {
		tokio::runtime::Builder::new_current_thread()
			.enable_all()
			.build()
			.expect("runtime")
	}

	#[test]
	fn scheme_parsing_and_validation() {
		assert_eq!(scheme_of("notes://a/b").as_deref(), Some("notes"));
		assert_eq!(scheme_of("Skill://x").as_deref(), Some("skill"));
		assert_eq!(scheme_of("/tmp/a:b"), None);
		assert_eq!(scheme_of("1bad://x"), None);
		assert!(is_valid_scheme("a1+-."));
		assert!(!is_valid_scheme("A"));
		assert!(is_reserved_scheme("security"));
		assert!(is_reserved_scheme("local"));
		let url = InternalUrl::parse("skill://plugin:name/x").expect("parse");
		assert_eq!(url.scheme, "skill");
		assert_eq!(url.host, "plugin:name");
		assert_eq!(url.path, "/x");
	}

	#[test]
	fn unknown_scheme_is_a_protocol_error_not_a_path() {
		let router = InternalUrlRouter::new();
		let cancel = CancellationToken::new();
		let context = UrlScope::default().resolve_context(Path::new("/w"), &cancel);
		let error = runtime()
			.block_on(router.resolve("nope://x", &context))
			.expect_err("unknown");
		assert_eq!(error, "Unknown protocol: nope://\nSupported: local://");
		router.register(Echo::new("notes", false, false)).expect("register");
		let error = runtime()
			.block_on(router.resolve("nope://x", &context))
			.expect_err("unknown");
		assert_eq!(error, "Unknown protocol: nope://\nSupported: local://, notes://");
		assert!(!router.can_handle("nope://x"));
		assert!(router.can_handle("notes://x"));
	}

	#[test]
	fn read_only_scheme_rejects_write_before_the_handler() {
		let router = InternalUrlRouter::new();
		let read_only = Echo::new("notes", false, false);
		router.register(read_only.clone()).expect("register");
		let writable = Echo::new("docs", true, true);
		router.register(writable.clone()).expect("register");
		let cancel = CancellationToken::new();
		let context = UrlScope::default().write_context(Path::new("/w"), &cancel);
		let runtime = runtime();
		assert_eq!(
			runtime
				.block_on(router.write("notes://a", "x", &context))
				.expect_err("read-only"),
			"notes:// URLs are read-only for write; use the protocol-specific tool for mutations."
		);
		assert_eq!(read_only.writes.load(Ordering::SeqCst), 0);
		runtime
			.block_on(router.write("docs://a", "x", &context))
			.expect("writable");
		assert_eq!(writable.writes.load(Ordering::SeqCst), 1);
		assert!(router.unregister("docs"));
		assert!(!router.unregister("docs"));
		assert_eq!(router.schemes(), vec!["local".to_string(), "notes".to_string()]);
	}

	#[test]
	fn scope_travels_per_call_with_handler_immutability() {
		let router = InternalUrlRouter::new();
		router.register(Echo::new("notes", true, false)).expect("register");
		let scope = UrlScope {
			session_file: Some(PathBuf::from("/s/session.jsonl")),
			local_roots:  BTreeMap::from([("local".to_string(), PathBuf::from("/s/session"))]),
		};
		let context = scope.resolve_context(Path::new("/w"), &CancellationToken::new());
		assert_eq!(context.session_file, Some(PathBuf::from("/s/session.jsonl")));
		assert_eq!(context.local_roots.get("local"), Some(&PathBuf::from("/s/session")));
		let resource = runtime()
			.block_on(router.resolve("notes://a", &context))
			.expect("resolve");
		assert_eq!(resource.url, "notes://a");
		assert_eq!(resource.content, "/w|a");
		assert_eq!(resource.content_type, "text/plain");
		assert_eq!(resource.size, resource.content.len());
		assert!(resource.immutable);
	}

	#[test]
	fn builtin_local_is_reserved_and_cannot_be_replaced() {
		let router = InternalUrlRouter::new();
		assert_eq!(router.schemes(), vec!["local".to_string()]);
		let error = router
			.register(Echo::new("local", false, false))
			.err()
			.expect("reserved");
		assert_eq!(error, "local:// is reserved for built-in handlers");
		assert!(!router.unregister("local"));
		assert!(router.get_handler("local").is_some());
	}

	#[test]
	fn decode_utf8_cuts_a_capped_read_back_to_a_boundary() {
		let bytes = "abc\u{00e9}".as_bytes().to_vec();
		let cut = &bytes[..4];
		assert_eq!(decode_utf8(cut, true, "local://x").expect("cut"), "abc");
		assert!(decode_utf8(cut, false, "local://x").is_err());
	}

	#[test]
	fn local_root_handler_resolves_only_inside_its_root() {
		let root = std::env::temp_dir().join(format!("omp-local-url-{}", std::process::id()));
		let outside = std::env::temp_dir().join(format!("omp-local-out-{}", std::process::id()));
		std::fs::create_dir_all(root.join("nested")).expect("root");
		std::fs::create_dir_all(&outside).expect("outside");
		std::fs::write(root.join("note.md"), "hello").expect("note");
		std::fs::write(root.join("nested").join("deep.txt"), "deep").expect("deep");
		std::fs::write(outside.join("secret.txt"), "secret").expect("secret");
		let scope = UrlScope {
			session_file: None,
			local_roots:  BTreeMap::from([(LOCAL_SCHEME.to_string(), root.clone())]),
		};
		let context = scope.resolve_context(&root, &CancellationToken::new());
		let router = InternalUrlRouter::new();
		let resource = runtime()
			.block_on(router.resolve("local://note.md", &context))
			.expect("resolve inside the root");
		assert_eq!(resource.content, "hello");
		assert_eq!(resource.content_type, "text/markdown");
		assert_eq!(resource.size, 5);
		let deep = runtime()
			.block_on(router.resolve("local://nested/deep.txt", &context))
			.expect("resolve a nested file");
		assert_eq!(deep.content, "deep");
		assert!(runtime()
			.block_on(router.resolve("local://../escape.md", &context))
			.is_err());
		assert!(runtime()
			.block_on(router.resolve("local://missing.md", &context))
			.is_err());
		assert!(runtime()
			.block_on(router.resolve("local://nested", &context))
			.is_err());
		assert!(runtime()
			.block_on(router.write(
				"local://note.md",
				"x",
				&scope.write_context(&root, &CancellationToken::new())
			))
			.is_err());
		// No root bound for this call: the handler stays honest.
		let bare = UrlScope::default().resolve_context(&root, &CancellationToken::new());
		assert!(runtime()
			.block_on(router.resolve("local://note.md", &bare))
			.is_err());
		#[cfg(unix)]
		{
			std::os::unix::fs::symlink(outside.join("secret.txt"), root.join("link.txt"))
				.expect("symlink file");
			std::os::unix::fs::symlink(&outside, root.join("linkdir")).expect("symlink dir");
			assert!(runtime()
				.block_on(router.resolve("local://link.txt", &context))
				.is_err());
			assert!(runtime()
				.block_on(router.resolve("local://linkdir/secret.txt", &context))
				.is_err());
		}
		std::fs::remove_dir_all(&root).ok();
		std::fs::remove_dir_all(&outside).ok();
	}

	#[cfg(unix)]
	fn unique_temp(label: &str) -> PathBuf {
		let nanos = SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.map(|duration| duration.as_nanos())
			.unwrap_or_default();
		let path = std::env::temp_dir().join(format!(
			"omp-local-{label}-{}-{nanos}",
			std::process::id()
		));
		std::fs::create_dir(&path).expect("temp dir");
		path
	}

	/// Nonblocking writers pair with a reader a regression left in `open(2)`;
	/// bounded so a fixed build cannot spin.
	#[cfg(unix)]
	fn release_blocked_reader(fifo: &Path) {
		use std::os::unix::ffi::OsStrExt;
		let Ok(c_fifo) = std::ffi::CString::new(fifo.as_os_str().as_bytes()) else {
			return;
		};
		let deadline = Instant::now() + Duration::from_secs(2);
		while Instant::now() < deadline {
			// SAFETY: NUL-terminated path; a successful fd is closed right here.
			let fd = unsafe { libc::open(c_fifo.as_ptr(), libc::O_WRONLY | libc::O_NONBLOCK) };
			if fd >= 0 {
				// SAFETY: fd was returned by `open` above and is owned here.
				unsafe { libc::close(fd) };
				return;
			}
			std::thread::sleep(Duration::from_millis(25));
		}
	}

	#[cfg(unix)]
	#[test]
	fn local_root_fifo_fails_fast_instead_of_blocking() {
		use std::os::unix::ffi::OsStrExt;

		let root = unique_temp("fifo");
		let fifo = root.join("pipe");
		let c_fifo = std::ffi::CString::new(fifo.as_os_str().as_bytes()).expect("cstr");
		// SAFETY: NUL-terminated path inside a directory this test created.
		assert_eq!(unsafe { libc::mkfifo(c_fifo.as_ptr(), 0o600) }, 0, "mkfifo");

		let scope = UrlScope {
			session_file: None,
			local_roots:  BTreeMap::from([(LOCAL_SCHEME.to_string(), root.clone())]),
		};
		let context = scope.resolve_context(&root, &CancellationToken::new());
		let router = InternalUrlRouter::new();
		let rt = runtime();
		// The outcome is fixed before any writer exists, so the timeout — not a
		// writer pairing — decides a regressed build.
		let outcome = rt.block_on(async {
			tokio::time::timeout(Duration::from_secs(2), router.resolve("local://pipe", &context))
				.await
		});
		if outcome.is_err() {
			release_blocked_reader(&fifo);
		}
		// The runtime must outlive the release, or dropping it waits on the
		// parked blocking worker.
		drop(rt);

		let message = match outcome {
			Ok(Err(message)) => message,
			other => panic!("a FIFO must fail fast: {other:?}"),
		};
		assert!(message.contains("is not a regular file"), "{message}");
		std::fs::remove_file(&fifo).expect("unlink fifo");
		std::fs::remove_dir(&root).expect("rmdir");
	}
}
