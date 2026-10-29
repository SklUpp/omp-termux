//! RPC-side auth bridge: one `agent.db`, one login at a time, TS-exact frames.
//!
//! The bridge owns the single [`AuthBackend`] used by the process and hands the
//! runtime a [`CredentialResolver`] for the per-request key lookup. Every
//! blocking call runs on a dedicated OS thread — never on a runtime worker and
//! never with a runtime nested inside the RPC bridge — so `rusqlite`, refresh
//! leases and the interactive login can block without stalling stdin.
//!
//! Invariants (upstream `rpc-mode.ts` behaviour):
//! - `get_login_providers` → `{providers:[{id,name,available,authenticated}]}`;
//! - one login at a time: a concurrent `login` or a providers query during a
//!   login is rejected with the typed `login_in_progress` code instead of
//!   queueing behind the store lock;
//! - the login response is deferred but is the ordinary response type: success
//!   `{providerId}`, failure the [`AuthError`] message — never a token;
//! - UI frames are `extension_ui_request` with `open_url` / `notify` / `input`
//!   (600 s prompt timeout), and a cancelled prompt is [`InputReply::Cancelled`],
//!   never an empty key;
//! - cancellation is explicit: [`AuthBridge::cancel_login`] and
//!   [`AuthBridge::shutdown`] fire the [`Cancel`] token and fail the login's own
//!   pending dialog; dropping a future is not cancellation.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::input::ExtensionUiBridge;
use super::session::FrameSink;
use super::types::RpcResponse;
use crate::auth::error::{AuthError, AuthErrorKind};
use crate::auth::http::Cancel;
use crate::auth::{
	AuthBackend, AuthUrlInfo, InputPrompt, InputReply, LoginCallbacks, LoginOutcome, LoginProvider,
};

/// Typed rejection code shared by `login` and `get_login_providers`.
pub const LOGIN_IN_PROGRESS: &str = "login_in_progress";

/// Upstream prompt budget for a pasted code (`rpc-mode.ts`, 600_000 ms).
const LOGIN_PROMPT_TIMEOUT: Duration = Duration::from_secs(600);

/// Input lookup for a synchronous caller that may also be a runtime worker.
///
/// The work runs on its own thread, so `api_key_for_session` never trips the
/// "called from an async runtime worker" guard, and a queued caller can be
/// cancelled while a login holds the store.
fn off_thread<T, F>(work: F) -> Result<T, AuthError>
where
	F: FnOnce() -> Result<T, AuthError> + Send + 'static,
	T: Send + 'static,
{
	let (tx, rx) = std::sync::mpsc::channel();
	std::thread::Builder::new()
		.name("omp-auth-call".to_string())
		.spawn(move || {
			let _ = tx.send(work());
		})
		.map_err(|error| {
			AuthError::new(AuthErrorKind::Configuration, format!("cannot spawn auth worker: {error}"))
		})?;
	rx.recv().map_err(|_| {
		AuthError::new(AuthErrorKind::Configuration, "auth worker thread dropped")
	})?
}

/// Acquire the store lock, polling so a queued caller honours cancellation.
fn lock_backend<'a>(
	backend: &'a Mutex<AuthBackend>,
	cancel: Option<&Cancel>,
	provider: &str,
) -> Result<MutexGuard<'a, AuthBackend>, AuthError> {
	loop {
		if let Some(cancel) = cancel {
			cancel.check(provider)?;
		}
		match backend.try_lock() {
			Ok(guard) => return Ok(guard),
			Err(std::sync::TryLockError::Poisoned(error)) => return Ok(error.into_inner()),
			Err(std::sync::TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(20)),
		}
	}
}

fn lock_poisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
	mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Per-request credential lookup for the runtime plan builder.
///
/// Contract: call from a blocking thread (the RPC turn thread is one). `Ok(None)`
/// means "no stored credential, no env key" — a genuine absence, never a
/// failure; the caller then falls back to its own config/env resolution.
pub trait CredentialResolver: Send + Sync {
	/// `getApiKey(provider, sessionId)` order: fresh/refreshed OAuth grant, then
	/// the `/login` API key, then the provider's declared env var, then a static
	/// stored key. `session_id` drives sticky selection.
	fn api_key(
		&self,
		provider: &str,
		session_id: Option<&str>,
		cancel: &Cancel,
	) -> Result<Option<String>, AuthError>;

	/// Status/doctor probe: no refresh, no lease, no interactive path.
	fn peek_api_key(&self, provider: &str) -> Result<Option<String>, AuthError>;
}

/// Resolver handle handed to the runtime; shares the bridge's single store.
pub struct BridgeResolver {
	backend: Arc<Mutex<AuthBackend>>,
}

impl CredentialResolver for BridgeResolver {
	fn api_key(
		&self,
		provider: &str,
		session_id: Option<&str>,
		cancel: &Cancel,
	) -> Result<Option<String>, AuthError> {
		let backend = Arc::clone(&self.backend);
		let provider = provider.to_string();
		let session = session_id.map(str::to_string);
		let cancel = cancel.clone();
		off_thread(move || {
			let guard = lock_backend(&backend, Some(&cancel), &provider)?;
			guard.api_key_for_session(&provider, session.as_deref(), &cancel)
		})
	}

	fn peek_api_key(&self, provider: &str) -> Result<Option<String>, AuthError> {
		let backend = Arc::clone(&self.backend);
		let provider = provider.to_string();
		off_thread(move || {
			let guard = lock_backend(&backend, None, &provider)?;
			guard.peek_api_key(&provider)
		})
	}
}

/// One in-flight login. `done` is signalled once the worker has written its
/// response and released the slot, so shutdown can join it bounded.
struct ActiveLogin {
	id:       u64,
	provider: String,
	cancel:   Cancel,
	done:     Arc<(Mutex<bool>, Condvar)>,
	worker:   Option<JoinHandle<()>>,
}

/// One deferred `get_login_providers` scan. Tracked so shutdown can cancel it
/// instead of leaving a worker parked behind a login or a refresh.
struct ProvidersJob {
	id:     u64,
	cancel: Cancel,
	worker: Option<JoinHandle<()>>,
}

/// RPC-owned auth surface: providers, single-flight login, runtime resolver.
pub struct AuthBridge {
	backend:    Arc<Mutex<AuthBackend>>,
	sink:       Arc<dyn FrameSink>,
	ui:         Arc<ExtensionUiBridge>,
	login:          Arc<Mutex<Option<ActiveLogin>>>,
	next_login:     AtomicU64,
	providers:      Arc<Mutex<Option<ProvidersJob>>>,
	next_providers: AtomicU64,
}

impl AuthBridge {
	/// Open the process store at an explicit `agent.db` path (CLI-resolved, never
	/// a hidden default) and bind it to the RPC transport.
	pub fn open(
		agent_db: impl Into<std::path::PathBuf>,
		sink: Arc<dyn FrameSink>,
		ui: Arc<ExtensionUiBridge>,
	) -> Result<Self, AuthError> {
		Ok(Self::with_backend(AuthBackend::open_at(agent_db)?, sink, ui))
	}

	/// Bind an already-open store (tests, broker hand-off).
	pub fn with_backend(
		backend: AuthBackend,
		sink: Arc<dyn FrameSink>,
		ui: Arc<ExtensionUiBridge>,
	) -> Self {
		Self {
			backend: Arc::new(Mutex::new(backend)),
			sink,
			ui,
			login: Arc::new(Mutex::new(None)),
			next_login: AtomicU64::new(1),
			providers: Arc::new(Mutex::new(None)),
			next_providers: AtomicU64::new(1),
		}
	}

	/// Resolver for the runtime plan builder; shares this bridge's store.
	#[must_use]
	pub fn resolver(&self) -> Arc<dyn CredentialResolver> {
		Arc::new(BridgeResolver {
			backend: Arc::clone(&self.backend),
		})
	}

	#[must_use]
	pub fn is_logging_in(&self) -> bool {
		lock_poisoned(&self.login).is_some()
	}

	/// `get_login_providers` response. TS shape
	/// `{providers:[{id,name,available,authenticated}]}`.
	///
	/// Bounded by construction:
	/// - a login owns the store → typed [`LOGIN_IN_PROGRESS`] rejection, never a
	///   queued stdin read;
	/// - a refresh/lookup holds the store → one deferred scan at a time, tracked
	///   in [`AuthBridge::shutdown`]; further commands are rejected busy instead of
	///   spawning more workers;
	/// - free store → answered on this thread (local `agent.db` query, no worker).
	#[must_use]
	pub fn providers(&self, id: Option<&str>) -> Option<RpcResponse> {
		if let Some(active) = lock_poisoned(&self.login).as_ref() {
			return Some(RpcResponse::error_code(
				id,
				"get_login_providers",
				format!("a {} login is in progress", active.provider),
				LOGIN_IN_PROGRESS,
			));
		}
		let mut slot = lock_poisoned(&self.providers);
		if slot.is_some() {
			return Some(RpcResponse::error_code(
				id,
				"get_login_providers",
				"a credential operation is already in progress",
				LOGIN_IN_PROGRESS,
			));
		}
		match self.backend.try_lock() {
			Ok(guard) => return Some(providers_response(id, guard.providers())),
			Err(std::sync::TryLockError::Poisoned(error)) => {
				return Some(providers_response(id, error.into_inner().providers()));
			},
			Err(std::sync::TryLockError::WouldBlock) => {},
		}

		// The store is busy elsewhere: defer through exactly one tracked worker.
		let job_id = self.next_providers.fetch_add(1, Ordering::Relaxed);
		let cancel = Cancel::new();
		let backend = Arc::clone(&self.backend);
		let sink = Arc::clone(&self.sink);
		let slots = Arc::clone(&self.providers);
		let worker_cancel = cancel.clone();
		let response_id = id.map(str::to_string);
		let spawned = std::thread::Builder::new()
			.name("omp-rpc-auth-providers".to_string())
			.spawn(move || {
				let result = lock_backend(&backend, Some(&worker_cancel), "providers")
					.and_then(|guard| guard.providers());
				let result = result.map_err(|error| {
					if error.kind == AuthErrorKind::Cancelled {
						AuthError::new(AuthErrorKind::Cancelled, "providers query cancelled")
					} else {
						error
					}
				});
				sink.send_response(providers_response(response_id.as_deref(), result));
				let mut slot = lock_poisoned(&slots);
				if slot.as_ref().is_some_and(|job| job.id == job_id) {
					*slot = None;
				}
			});
		match spawned {
			Ok(worker) => {
				*slot = Some(ProvidersJob { id: job_id, cancel, worker: Some(worker) });
				None
			},
			Err(error) => Some(RpcResponse::error(
				id,
				"get_login_providers",
				format!("cannot spawn auth worker: {error}"),
			)),
		}
	}

	/// Start a login. `None` means the response arrives later through the sink
	/// (deferred, ordinary response type); `Some` is an immediate rejection.
	#[must_use]
	pub fn login(&self, id: Option<&str>, provider_id: &str) -> Option<RpcResponse> {
		if crate::auth::registry::find(provider_id).is_none() {
			return Some(RpcResponse::error(
				id,
				"login",
				format!("Unknown OAuth provider: {provider_id}"),
			));
		}
		let login_id = self.next_login.fetch_add(1, Ordering::Relaxed);
		let done = Arc::new((Mutex::new(false), Condvar::new()));
		let cancel = Cancel::new();
		{
			let mut slot = lock_poisoned(&self.login);
			if let Some(active) = slot.as_ref() {
				return Some(RpcResponse::error_code(
					id,
					"login",
					format!("a {} login is already in progress", active.provider),
					LOGIN_IN_PROGRESS,
				));
			}
			*slot = Some(ActiveLogin {
				id: login_id,
				provider: provider_id.to_string(),
				cancel: cancel.clone(),
				done: Arc::clone(&done),
				worker: None,
			});
		}

		let callbacks: Arc<dyn LoginCallbacks> = Arc::new(RpcAuthCallbacks::new(
			provider_id,
			Arc::clone(&self.sink),
			Arc::clone(&self.ui),
		));
		let backend = Arc::clone(&self.backend);
		let sink = Arc::clone(&self.sink);
		let login_slot = Arc::clone(&self.login);
		let provider = provider_id.to_string();
		let response_id = id.map(str::to_string);
		let worker_cancel = cancel;
		let worker_done = Arc::clone(&done);
		let spawned = std::thread::Builder::new()
			.name("omp-rpc-auth-login".to_string())
			.spawn(move || {
				let result = catch_unwind(AssertUnwindSafe(|| {
					let guard = lock_backend(&backend, Some(&worker_cancel), &provider)?;
					guard.login(&provider, callbacks, &worker_cancel)
				}));
				let response = match result {
					Ok(Ok(outcome)) => RpcResponse::success_data(
						response_id.as_deref(),
						"login",
						login_success_data(&outcome),
					),
					Ok(Err(error)) => RpcResponse::error(response_id.as_deref(), "login", error.message),
					Err(_) => RpcResponse::error(
						response_id.as_deref(),
						"login",
						"auth worker panicked",
					),
				};
				sink.send_response(response);
				// Release the slot before signalling completion, so a waiter that
				// sees the flag also sees an idle bridge.
				retire_login(&login_slot, login_id);
				let (flag, ready) = &*worker_done;
				*lock_poisoned(flag) = true;
				ready.notify_all();
			});

		match spawned {
			Ok(worker) => {
				let mut slot = lock_poisoned(&self.login);
				if let Some(active) = slot.as_mut().filter(|active| active.id == login_id) {
					active.worker = Some(worker);
				}
				None
			},
			Err(error) => {
				retire_login(&self.login, login_id);
				Some(RpcResponse::error(
					id,
					"login",
					format!("cannot spawn auth worker: {error}"),
				))
			},
		}
	}

	/// Cancel the in-flight login (abort/finish).
	///
	/// Only this login's own dialog closes: the flow hands the token to
	/// [`LoginCallbacks::input`], whose RPC host dismisses exactly the
	/// dialog it registered (upstream `method:"cancel"` frame). Unrelated
	/// extension dialogs are never swept.
	pub fn cancel_login(&self) -> bool {
		let cancel = {
			let active = lock_poisoned(&self.login);
			let Some(active) = active.as_ref() else { return false };
			active.cancel.clone()
		};
		cancel.cancel();
		true
	}

	/// Cancel and join the login bounded by `timeout`. `true` when nothing is
	/// left running. A worker that outlives the budget is detached (the cancel
	/// token is already fired, so it exits at its next check) rather than
	/// blocking shutdown on a network timeout.
	pub fn shutdown(&self, timeout: Duration) -> bool {
		let providers = lock_poisoned(&self.providers).take();
		let active = lock_poisoned(&self.login).take();
		if let Some(job) = providers {
			// Cancelled before the join: the lock loop exits at its next 20 ms poll,
			// so a scan queued behind a login cannot outlive teardown.
			job.cancel.cancel();
			if let Some(worker) = job.worker {
				let _ = worker.join();
			}
		}
		// Nothing to close when no login owns the bridge: dialogs are login-owned
		// and every login joins its prompt before retiring (see `cancel_login`).
		let Some(active) = active else { return true };
		active.cancel.cancel();
		let deadline = Instant::now() + timeout;
		let (flag, ready) = &*active.done;
		let mut done = lock_poisoned(flag);
		while !*done {
			let remaining = deadline.saturating_duration_since(Instant::now());
			if remaining.is_zero() {
				break;
			}
			let (guard, _) = ready
				.wait_timeout(done, remaining)
				.unwrap_or_else(PoisonError::into_inner);
			done = guard;
		}
		let finished = *done;
		drop(done);
		if finished {
			if let Some(worker) = active.worker {
				let _ = worker.join();
			}
		}
		finished
	}
}

fn retire_login(login: &Mutex<Option<ActiveLogin>>, id: u64) {
	let mut slot = lock_poisoned(login);
	if slot.as_ref().is_some_and(|active| active.id == id) {
		*slot = None;
	}
}

fn login_success_data(outcome: &LoginOutcome) -> Value {
	let provider = match outcome {
		LoginOutcome::OAuth { provider, .. } | LoginOutcome::ApiKey { provider } => provider,
	};
	json!({ "providerId": provider })
}

fn providers_response(id: Option<&str>, result: Result<Vec<LoginProvider>, AuthError>) -> RpcResponse {
	match result {
		Ok(providers) => RpcResponse::success_data(
			id,
			"get_login_providers",
			json!({ "providers": providers_json(&providers) }),
		),
		Err(error) => RpcResponse::error(id, "get_login_providers", error.message),
	}
}

/// TS-exact provider list. `available` mirrors `provider.available ?? true`
/// (no provider sets it today); `authenticated` is "a stored, active credential
/// exists" — the `hasAuth(id)` analogue.
fn providers_json(providers: &[LoginProvider]) -> Value {
	Value::Array(
		providers
			.iter()
			.map(|provider| {
				json!({
					"id": provider.id,
					"name": provider.name,
					"available": true,
					"authenticated": provider.configured,
				})
			})
			.collect(),
	)
}

/// `LoginCallbacks` over the RPC transport. Tokens never pass through here: the
/// URL, progress lines and prompts only.
struct RpcAuthCallbacks {
	provider:     String,
	sink:         Arc<dyn FrameSink>,
	ui:           Arc<ExtensionUiBridge>,
	next_frame:   AtomicU64,
	/// `onAuth` fired: after this, `input` is the pasted-code path (upstream
	/// `authEmitted`); before it, an interactive provider cannot work headlessly.
	auth_emitted: AtomicBool,
}

impl RpcAuthCallbacks {
	fn new(provider: &str, sink: Arc<dyn FrameSink>, ui: Arc<ExtensionUiBridge>) -> Self {
		Self {
			provider: provider.to_string(),
			sink,
			ui,
			next_frame: AtomicU64::new(1),
			auth_emitted: AtomicBool::new(false),
		}
	}

	fn frame_id(&self) -> String {
		format!(
			"auth-{}-{}",
			self.provider,
			self.next_frame.fetch_add(1, Ordering::Relaxed)
		)
	}
}

impl LoginCallbacks for RpcAuthCallbacks {
	fn open_url(&self, info: &AuthUrlInfo) -> Result<(), String> {
		self.auth_emitted.store(true, Ordering::SeqCst);
		let mut frame = serde_json::Map::new();
		frame.insert("type".to_string(), json!("extension_ui_request"));
		frame.insert("id".to_string(), json!(self.frame_id()));
		frame.insert("method".to_string(), json!("open_url"));
		frame.insert("url".to_string(), json!(info.url));
		if let Some(launch_url) = &info.launch_url {
			frame.insert("launchUrl".to_string(), json!(launch_url));
		}
		if let Some(instructions) = &info.instructions {
			frame.insert("instructions".to_string(), json!(instructions));
		}
		self.sink.send(Value::Object(frame));
		Ok(())
	}

	fn notice(&self, message: &str) {
		self.sink.send(json!({
			"type": "extension_ui_request",
			"id": self.frame_id(),
			"method": "notify",
			"message": message,
			"notifyType": "info",
		}));
	}

	fn input(&self, prompt: &InputPrompt, cancel: &Cancel) -> InputReply {
		if !self.auth_emitted.load(Ordering::SeqCst) {
			// Upstream rejects the prompt before `onAuth`; [`InputReply::Rejected`]
			// carries that reason into a `PromptRequired` login error instead of a
			// misleading "cancelled". The condition is exactly "no credential was
			// stored or URL shown yet", which is where an interactive provider
			// cannot proceed headlessly.
			// Message parity with `rpc-mode.ts` (the rejected `onPrompt`).
			return InputReply::Rejected(format!(
				"Provider '{}' requires interactive prompts which are not supported in RPC mode. Use the terminal UI to log in.",
				self.provider
			));
		}
		let mut frame = serde_json::Map::new();
		frame.insert("method".to_string(), json!("input"));
		frame.insert("title".to_string(), json!(prompt.message));
		if let Some(placeholder) = &prompt.placeholder {
			frame.insert("placeholder".to_string(), json!(placeholder));
		}
		// One dialog, owned by this login: the flow's token closes it on cancel or
		// when a browser callback wins, without touching any other dialog.
		match self.ui.request_cancellable(
			Value::Object(frame),
			Some(LOGIN_PROMPT_TIMEOUT),
			&|| cancel.is_cancelled(),
		) {
			Some(response) => match response.value {
				Some(value) => InputReply::Value(value),
				// A cancelled or timed-out dialog carries no value; never turn that
				// into a blank credential/pasted code.
				None => InputReply::Cancelled,
			},
			None => InputReply::Cancelled,
		}
	}
}

#[cfg(test)]
mod tests;
