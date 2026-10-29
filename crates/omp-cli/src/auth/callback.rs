//! Loopback OAuth callback server (`/callback` + `/launch`).
//!
//! Anchors: `packages/ai/src/registry/oauth/callback-server.ts:124-560`.
//! Behaviour that matters and is preserved:
//!
//! - preferred port first; random port only when the flow allows it, and a
//!   pinned `redirect_uri`/`allowPortFallback=false` turns a busy port into an
//!   actionable configuration error instead of advertising a URI the provider
//!   will reject;
//! - `localhost` binds both `127.0.0.1` and `::1` so a browser that resolves the
//!   IPv6 literal first still reaches us (a wildcard bind by another process
//!   would otherwise swallow the code);
//! - `/launch` 302-redirects to the pending authorization URL and is never
//!   advertised when it would collide with the callback path or when the
//!   redirect does not return to this loopback server;
//! - callback `state` is compared in constant time; a forged error without our
//!   state never terminates the login.

use std::io::{Read as _, Write as _};
use std::net::{Ipv4Addr, Ipv6Addr, TcpListener, TcpStream};
use std::sync::{
	Arc, Mutex,
	atomic::{AtomicBool, Ordering},
	mpsc::{self, Receiver, Sender, TryRecvError},
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::error::{AuthError, AuthErrorKind};
use super::pkce::constant_time_eq;

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);
pub const LAUNCH_PATH: &str = "/launch";

#[derive(Debug, Clone)]
pub struct CallbackConfig {
	pub preferred_port: u16,
	pub callback_path: String,
	pub callback_hostname: String,
	pub redirect_uri: Option<String>,
	pub allow_port_fallback: bool,
	pub manual_input_only: bool,
}

impl CallbackConfig {
	#[must_use]
	pub fn new(preferred_port: u16, callback_path: &str) -> Self {
		Self {
			preferred_port,
			callback_path: callback_path.to_string(),
			callback_hostname: "localhost".to_string(),
			redirect_uri: None,
			allow_port_fallback: true,
			manual_input_only: false,
		}
	}

	#[must_use]
	pub fn with_hostname(mut self, hostname: &str) -> Self {
		self.callback_hostname = hostname.to_string();
		self
	}

	#[must_use]
	pub fn with_redirect_uri(mut self, redirect_uri: &str) -> Self {
		self.redirect_uri = Some(redirect_uri.to_string());
		self
	}

	#[must_use]
	pub fn without_port_fallback(mut self) -> Self {
		self.allow_port_fallback = false;
		self
	}

	#[must_use]
	pub fn manual_input_only(mut self) -> Self {
		self.manual_input_only = true;
		self
	}
}

#[derive(Debug, Clone)]
pub struct CallbackOutcome {
	pub code: String,
	pub state: String,
}

type CallbackSignal = Result<CallbackOutcome, AuthError>;

/// A bound loopback listener plus the result channel it feeds.
pub struct CallbackServer {
	port: u16,
	redirect_uri: String,
	launch_url: Option<String>,
	receiver: Receiver<CallbackSignal>,
	pending_auth_url: Arc<Mutex<Option<String>>>,
	stop: Arc<AtomicBool>,
	threads: Vec<JoinHandle<()>>,
	manual_only: bool,
}

impl CallbackServer {
	/// Bind the loopback listener(s). `expected_state` is compared inside the
	/// request handler, never by the caller after the fact.
	pub fn start(
		config: &CallbackConfig,
		expected_state: &str,
		provider: &str,
	) -> Result<Self, AuthError> {
		if config.manual_input_only {
			return Ok(Self {
				port: 0,
				redirect_uri: build_redirect_uri(config, config.preferred_port),
				launch_url: None,
				receiver: mpsc::channel().1,
				pending_auth_url: Arc::new(Mutex::new(None)),
				stop: Arc::new(AtomicBool::new(true)),
				threads: Vec::new(),
				manual_only: true,
			});
		}

		let (listener, port) = bind_primary(config, provider)?;
		let mut listeners = vec![listener];

		// `localhost` must answer on both loopback literals; a host that cannot
		// bind `::1` (IPv6 disabled) is served by the IPv4 listener alone.
		if config.callback_hostname == "localhost" {
			match TcpListener::bind((Ipv6Addr::LOCALHOST, port)) {
				Ok(listener) => listeners.push(listener),
				Err(err) if err.kind() == std::io::ErrorKind::AddrInUse && config.preferred_port != 0 => {
					// Nothing has been spawned yet, so the IPv4 port is released
					// with `listeners` instead of leaking a listener thread.
					return Err(AuthError::config(
						provider,
						format!(
							"OAuth callback port {port} is already used on ::1. Free the port and retry, or set a different callback port."
						),
					));
				},
				Err(_) => {},
			}
		}

		let stop = Arc::new(AtomicBool::new(false));
		let pending_auth_url = Arc::new(Mutex::new(None));
		let (sender, receiver) = mpsc::channel();
		let threads = listeners
			.into_iter()
			.map(|listener| {
				spawn_listener(
					listener,
					config.callback_path.clone(),
					expected_state.to_string(),
					sender.clone(),
					Arc::clone(&pending_auth_url),
					Arc::clone(&stop),
				)
			})
			.collect();

		let redirect_uri = build_redirect_uri(config, port);
		let launch_url = launch_url_if_safe(config, port);
		Ok(Self { port, redirect_uri, launch_url, receiver, pending_auth_url, stop, threads, manual_only: false })
	}

	#[must_use]
	pub fn port(&self) -> u16 {
		self.port
	}

	#[must_use]
	pub fn redirect_uri(&self) -> &str {
		&self.redirect_uri
	}

	#[must_use]
	pub fn launch_url(&self) -> Option<&str> {
		self.launch_url.as_deref()
	}

	/// Publish the authorization URL so `/launch` can 302 to it.
	pub fn set_pending_auth_url(&self, url: &str) {
		if let Ok(mut guard) = self.pending_auth_url.lock() {
			*guard = Some(url.to_string());
		}
	}

	/// Non-blocking poll used by the async wait loop.
	pub fn poll(&self) -> Option<CallbackSignal> {
		if self.manual_only {
			// Manual-only flows never bind a listener; the paste channel is the
			// only source of a code.
			return None;
		}
		match self.receiver.try_recv() {
			Ok(signal) => Some(signal),
			Err(TryRecvError::Empty) => None,
			Err(TryRecvError::Disconnected) => Some(Err(AuthError::new(
				AuthErrorKind::Configuration,
				"OAuth callback listener stopped unexpectedly",
			))),
		}
	}

	pub fn stop(&self) {
		self.stop.store(true, Ordering::SeqCst);
		if let Ok(mut guard) = self.pending_auth_url.lock() {
			*guard = None;
		}
	}
}

impl Drop for CallbackServer {
	fn drop(&mut self) {
		self.stop();
		for thread in self.threads.drain(..) {
			// Listener threads poll `stop` every 25ms and return on their own.
			let _ = thread.join();
		}
	}
}

fn build_redirect_uri(config: &CallbackConfig, port: u16) -> String {
	config.redirect_uri.clone().unwrap_or_else(|| {
		format!("http://{}:{}{}", config.callback_hostname, port, config.callback_path)
	})
}

/// `#launchUrlIfSafe`: only advertise the short self-redirect when the
/// callback really returns to this loopback listener.
fn launch_url_if_safe(config: &CallbackConfig, port: u16) -> Option<String> {
	if config.callback_path == LAUNCH_PATH {
		return None;
	}
	if let Some(redirect) = &config.redirect_uri {
		let (scheme, rest) = redirect.split_once("://")?;
		if scheme != "http" && scheme != "https" {
			return None;
		}
		let authority_end = rest.find('/').unwrap_or(rest.len());
		let host = &rest[..authority_end];
		let path = &rest[authority_end..];
		let host = host.rsplit_once('@').map_or(host, |(_, host)| host);
		// `authority` still carries an optional port (`localhost:1455`); an
		// IPv6 literal keeps its brackets so the port split stays unambiguous.
		let host = if let Some(rest) = host.strip_prefix('[') {
			rest.split(']').next().unwrap_or(rest)
		} else {
			host.split(':').next().unwrap_or(host)
		};
		let is_loopback = matches!(host, "localhost" | "127.0.0.1" | "::1");
		if !is_loopback || path == LAUNCH_PATH {
			return None;
		}
	}
	Some(format!("http://{}:{}{}", config.callback_hostname, port, LAUNCH_PATH))
}

fn bind_primary(config: &CallbackConfig, provider: &str) -> Result<(TcpListener, u16), AuthError> {
	let hostname = &config.callback_hostname;
	let bind = |port: u16| -> std::io::Result<TcpListener> {
		if hostname == "localhost" || is_ipv4_literal(hostname) {
			TcpListener::bind((Ipv4Addr::LOCALHOST, port))
		} else {
			TcpListener::bind((hostname.as_str(), port))
		}
	};

	match bind(config.preferred_port) {
		Ok(listener) => {
			let port = listener.local_addr().map_err(AuthError::from)?.port();
			Ok((listener, port))
		},
		Err(err) => {
			if config.redirect_uri.is_some() {
				return Err(AuthError::config(
					provider,
					format!(
						"OAuth callback port {} is in use, but the pinned redirect URI ({}) requires this exact port. Free port {} and retry.",
						config.preferred_port,
						config.redirect_uri.as_deref().unwrap_or_default(),
						config.preferred_port
					),
				));
			}
			if !config.allow_port_fallback {
				return Err(AuthError::config(
					provider,
					format!(
						"OAuth callback port {} is in use. The provider validates redirect URIs against its registered callback, so a random port would be rejected. Free port {} and retry.",
						config.preferred_port, config.preferred_port
					),
				));
			}
			let listener = bind(0).map_err(|fallback_err| {
				AuthError::config(
					provider,
					format!(
						"cannot bind OAuth callback port {} ({err}); random port bind also failed ({fallback_err})",
						config.preferred_port
					),
				)
			})?;
			let port = listener.local_addr().map_err(AuthError::from)?.port();
			Ok((listener, port))
		},
	}
}

fn is_ipv4_literal(host: &str) -> bool {
	host.parse::<Ipv4Addr>().is_ok()
}

fn spawn_listener(
	listener: TcpListener,
	callback_path: String,
	expected_state: String,
	sender: Sender<CallbackSignal>,
	pending_auth_url: Arc<Mutex<Option<String>>>,
	stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
	std::thread::spawn(move || {
		if listener.set_nonblocking(true).is_err() {
			return;
		}
		while !stop.load(Ordering::SeqCst) {
			match listener.accept() {
				Ok((stream, _)) => {
					handle_connection(
						stream,
						&callback_path,
						&expected_state,
						&sender,
						&pending_auth_url,
						&stop,
					);
				},
				Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
					std::thread::sleep(Duration::from_millis(25));
				},
				Err(_) => std::thread::sleep(Duration::from_millis(25)),
			}
		}
	})
}

/// Read one HTTP request line with an 8 KiB cap, an absolute deadline and a
/// stop check between reads.
///
/// A local app that streams an unterminated request line must not grow memory
/// or pin the listener thread (the listener would never answer the browser and
/// `Drop` would join a blocked thread).
fn read_request_line(stream: &mut TcpStream, stop: &AtomicBool) -> Option<String> {
	const MAX_REQUEST_LINE_BYTES: usize = 8 * 1024;
	const DEADLINE: Duration = Duration::from_secs(2);
	let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
	let deadline = Instant::now() + DEADLINE;
	let mut buffer: Vec<u8> = Vec::with_capacity(512);
	let mut chunk = [0u8; 512];
	while buffer.len() < MAX_REQUEST_LINE_BYTES && Instant::now() < deadline {
		if stop.load(Ordering::SeqCst) {
			return None;
		}
		match stream.read(&mut chunk) {
			Ok(0) => break,
			Ok(read) => {
				buffer.extend_from_slice(&chunk[..read]);
				if buffer.contains(&b'\n') {
					break;
				}
			},
			Err(err)
				if matches!(
					err.kind(),
					std::io::ErrorKind::WouldBlock
						| std::io::ErrorKind::TimedOut
						| std::io::ErrorKind::Interrupted
				) => {},
			Err(_) => return None,
		}
	}
	String::from_utf8_lossy(&buffer)
		.lines()
		.next()
		.map(|line| line.trim_end_matches('\r').to_string())
}

fn handle_connection(
	mut stream: TcpStream,
	callback_path: &str,
	expected_state: &str,
	sender: &Sender<CallbackSignal>,
	pending_auth_url: &Arc<Mutex<Option<String>>>,
	stop: &AtomicBool,
) {
	let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
	let Some(request_line) = read_request_line(&mut stream, stop) else { return };
	let mut parts = request_line.split_whitespace();
	let _method = parts.next().unwrap_or_default();
	let target = parts.next().unwrap_or_default();
	let (path, query) = target.split_once('?').unwrap_or((target, ""));

	if path != callback_path {
		if path == LAUNCH_PATH {
			let pending = pending_auth_url.lock().ok().and_then(|guard| guard.clone());
			match pending {
				Some(url) => {
					let _ = write!(
						stream,
						"HTTP/1.1 302 Found\r\nLocation: {url}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
					);
				},
				None => respond(&mut stream, 503, "OAuth launch URL is no longer active"),
			}
			return;
		}
		respond(&mut stream, 404, "Not Found");
		return;
	}

	let params = parse_query(query);
	let code = params.iter().find(|(name, _)| name == "code").map(|(_, value)| value.clone());
	let state = params.iter().find(|(name, _)| name == "state").map(|(_, value)| value.clone()).unwrap_or_default();
	let error = params.iter().find(|(name, _)| name == "error").map(|(_, value)| value.clone()).unwrap_or_default();
	let state_matches = expected_state.is_empty() || constant_time_eq(&state, expected_state);

	if !error.is_empty() {
		if state_matches {
			// The redirect carries our nonce: surface the denial now instead of
			// waiting out the full timeout. Forged errors are ignored below.
			// The redirect is untrusted input: only an allowlisted OAuth
			// identifier is quoted back, never provider prose.
			let detail = crate::auth::error::oauth_error_identifier(&error).map_or_else(
				|| "provider denied the request".to_string(),
				|identifier| format!("provider denied the request ({identifier})"),
			);
			let _ = sender.send(Err(AuthError::new(AuthErrorKind::DeviceAuth, format!("Authorization failed: {detail}"))));
		}
		respond(&mut stream, 500, "Authorization failed");
		return;
	}
	if code.is_none() {
		respond(&mut stream, 500, "Missing authorization code");
		return;
	}
	if !state_matches {
		respond(&mut stream, 500, "State mismatch - possible CSRF attack");
		return;
	}

	let code = code.unwrap_or_default();
	let _ = sender.send(Ok(CallbackOutcome { code: code.clone(), state: state.clone() }));
	let page = format!(
		"<!doctype html><meta charset=\"utf-8\"><title>OAuth complete</title><p>Authorization received. You can close this window.</p><script>window.__OAUTH_STATE__={};</script>",
		serde_json::json!({ "ok": true, "code": code, "state": state })
	);
	respond(&mut stream, 200, &page);
}

fn respond(stream: &mut TcpStream, status: u16, body: &str) {
	let reason = match status {
		200 => "OK",
		404 => "Not Found",
		500 => "Internal Server Error",
		503 => "Service Unavailable",
		_ => "OK",
	};
	let _ = write!(
		stream,
		"HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
		body.len()
	);
	let _ = stream.flush();
}

fn percent_decode(value: &str) -> String {
	let bytes = value.as_bytes();
	let mut out = Vec::with_capacity(bytes.len());
	let mut index = 0;
	while index < bytes.len() {
		match bytes[index] {
			b'+' => {
				out.push(b' ');
				index += 1;
			},
			b'%' if index + 2 < bytes.len() => {
				let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok();
				match hex.and_then(|hex| u8::from_str_radix(hex, 16).ok()) {
					Some(byte) => {
						out.push(byte);
						index += 3;
					},
					None => {
						out.push(b'%');
						index += 1;
					},
				}
			},
			byte => {
				out.push(byte);
				index += 1;
			},
		}
	}
	String::from_utf8_lossy(&out).into_owned()
}

#[must_use]
pub fn parse_query(query: &str) -> Vec<(String, String)> {
	query
		.split('&')
		.filter(|pair| !pair.is_empty())
		.map(|pair| {
			let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
			(percent_decode(name), percent_decode(value))
		})
		.collect()
}

/// Parse a pasted redirect URL or bare code (`parseCallbackInput`).
///
/// Any scheme is accepted (`https://`, `vscode://`, `omp://`, custom loopback
/// schemes): the code rides in the `?query` or `#fragment`, and a bare
/// `code=…&state=…` paste is treated the same way. Anything without a
/// recognisable query is a raw code, optionally `code#state`.
#[must_use]
pub fn parse_callback_input(input: &str) -> (Option<String>, Option<String>) {
	let value = input.trim();
	if value.is_empty() {
		return (None, None);
	}

	let find_in_query = |query: &str| {
		let params = parse_query(query);
		let find = |name: &str| params.iter().find(|(key, _)| key == name).map(|(_, value)| value.clone());
		(find("code"), find("state"))
	};

	// `scheme://host/path?code=…&state=…#…` for any scheme.
	if value.contains("://") {
		if let Some((_, after_query)) = value.split_once('?') {
			let query = after_query.split('#').next().unwrap_or("");
			return find_in_query(query);
		}
		if let Some((_, fragment)) = value.rsplit_once('#') {
			if fragment.contains("code=") {
				return find_in_query(fragment);
			}
		}
	}

	if let Some(query) = value.strip_prefix('?').or_else(|| value.strip_prefix('#')) {
		return find_in_query(query);
	}
	// Bare query form (`code=abc&state=xyz`), no URL around it.
	if value.contains("code=") && value.contains('=') {
		return find_in_query(value);
	}
	// Raw code, optionally `code#state`.
	match value.split_once('#') {
		Some((code, state)) => {
			(Some(code.to_string()), (!state.is_empty()).then(|| state.to_string()))
		},
		None => (Some(value.to_string()), None),
	}
}

/// Percent-encode one query-string component (`URLSearchParams` semantics).
#[must_use]
pub fn encode_component(value: &str) -> String {
	let mut out = String::with_capacity(value.len());
	for byte in value.as_bytes() {
		match byte {
			b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'*' => out.push(char::from(*byte)),
			b' ' => out.push('+'),
			_ => out.push_str(&format!("%{byte:02X}")),
		}
	}
	out
}

/// Build `url?k=v&…` exactly as `URLSearchParams.toString()` would.
#[must_use]
pub fn with_query(url: &str, params: &[(&str, &str)]) -> String {
	let query = params
		.iter()
		.map(|(name, value)| format!("{}={}", encode_component(name), encode_component(value)))
		.collect::<Vec<_>>()
		.join("&");
	format!("{url}?{query}")
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parses_full_redirect_and_bare_code() {
		let (code, state) = parse_callback_input("http://localhost:54545/callback?code=abc%2B1&state=deadbeef");
		assert_eq!(code.as_deref(), Some("abc+1"));
		assert_eq!(state.as_deref(), Some("deadbeef"));

		let (code, state) = parse_callback_input("raw-code#csrf");
		assert_eq!(code.as_deref(), Some("raw-code"));
		assert_eq!(state.as_deref(), Some("csrf"));

		assert_eq!(parse_callback_input("   "), (None, None));
	}

	#[test]
	fn encodes_like_url_search_params() {
		assert_eq!(
			with_query("https://x/auth", &[("scope", "a b"), ("redirect_uri", "http://127.0.0.1:1/cb")]),
			"https://x/auth?scope=a+b&redirect_uri=http%3A%2F%2F127.0.0.1%3A1%2Fcb"
		);
	}

	#[test]
	fn launch_url_suppressed_for_non_loopback_redirect() {
		let mut config = CallbackConfig::new(1455, "/auth/callback");
		config.redirect_uri = Some("http://localhost:1455/auth/callback".to_string());
		assert_eq!(launch_url_if_safe(&config, 1455).as_deref(), Some("http://localhost:1455/launch"));

		config.redirect_uri = Some("vscode://oauth/callback".to_string());
		assert_eq!(launch_url_if_safe(&config, 1455), None);

		config.redirect_uri = None;
		config.callback_path = LAUNCH_PATH.to_string();
		assert_eq!(launch_url_if_safe(&config, 1455), None);
	}

	#[test]
	fn server_delivers_code_and_checks_state() {
		let config = CallbackConfig::new(0, "/callback");
		let state = "abc123";
		let server = CallbackServer::start(&config, state, "test").expect("bind loopback");
		let port = server.port();
		assert!(port > 0);

		let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).expect("connect");
		write!(
			client,
			"GET /callback?code=the-code&state=abc123 HTTP/1.1\r\nHost: localhost\r\n\r\n"
		)
		.expect("request");
		let _ = client.flush();

		let mut outcome = None;
		for _ in 0..200 {
			if let Some(signal) = server.poll() {
				outcome = Some(signal);
				break;
			}
			std::thread::sleep(Duration::from_millis(10));
		}
		let outcome = outcome.expect("callback delivered").expect("state matches");
		assert_eq!(outcome.code, "the-code");
		assert_eq!(outcome.state, "abc123");
	}

	#[test]
	fn forged_state_is_ignored_and_reported_as_error_page() {
		let config = CallbackConfig::new(0, "/callback");
		let server = CallbackServer::start(&config, "expected", "test").expect("bind loopback");
		let port = server.port();
		let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).expect("connect");
		write!(client, "GET /callback?code=x&state=forged HTTP/1.1\r\nHost: localhost\r\n\r\n").expect("request");
		let _ = client.flush();
		std::thread::sleep(Duration::from_millis(120));
		assert!(server.poll().is_none(), "state mismatch must not resolve the flow");
	}

	#[test]
	fn parses_custom_scheme_and_bare_query_pastes() {
		let (code, state) = parse_callback_input(
			"vscode://gitlab.gitlab-workflow/authentication?code=the-code&state=nonce#extra",
		);
		assert_eq!(code.as_deref(), Some("the-code"));
		assert_eq!(state.as_deref(), Some("nonce"));

		let (code, state) = parse_callback_input("code=abc&state=xyz");
		assert_eq!(code.as_deref(), Some("abc"));
		assert_eq!(state.as_deref(), Some("xyz"));

		let (code, state) = parse_callback_input("omp://auth#code=frag&state=s2");
		assert_eq!(code.as_deref(), Some("frag"));
		assert_eq!(state.as_deref(), Some("s2"));
	}
}
