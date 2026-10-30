//! ACP credential reuse.
//!
//! The ACP agent does not own a login UI: `authenticate` is the TS-exact
//! no-op for `agent` (credentials resolve per request at turn time) and
//! terminal setup points at `omp login`. The runner still needs the same
//! per-request resolution order as every other transport, so this module
//! wraps the existing [`AuthBackend`] store (`<agent dir>/agent.db`) and hands
//! `RunRequest` a [`CredentialResolver`].
//!
//! `ponytail:` this repeats the small off-thread/lock-poll wrapper from
//! `rpc::auth::BridgeResolver` only because that constructor is bound to the
//! RPC `ExtensionUiBridge`. Reuse `BridgeResolver` if it ever exposes a plain
//! backend constructor.

use std::{
	sync::{Arc, Mutex, MutexGuard, PoisonError},
	time::Duration,
};

use crate::{
	auth::http::Cancel,
	auth::{AuthBackend, error::AuthError},
	rpc::auth::CredentialResolver,
};

/// Owns one credential store shared by every ACP session in this process.
pub struct AcpAuth {
	backend: Arc<Mutex<AuthBackend>>,
}

impl AcpAuth {
	/// Open the CLI-resolved `agent.db`. Never falls back to a hidden path.
	pub fn open(agent_db: impl Into<std::path::PathBuf>) -> Result<Self, AuthError> {
		Ok(Self { backend: Arc::new(Mutex::new(AuthBackend::open_at(agent_db)?)) })
	}

	/// Resolver installed on every `RunRequest` from this ACP connection.
	#[must_use]
	pub fn resolver(&self) -> Arc<dyn CredentialResolver> {
		Arc::new(AcpResolver { backend: Arc::clone(&self.backend) })
	}
}

struct AcpResolver {
	backend: Arc<Mutex<AuthBackend>>,
}

impl CredentialResolver for AcpResolver {
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

/// Run a blocking store call off the runtime worker. `rusqlite` and refresh
/// leases must never block the turn thread that called us.
fn off_thread<T, F>(work: F) -> Result<T, AuthError>
where
	F: FnOnce() -> Result<T, AuthError> + Send + 'static,
	T: Send + 'static,
{
	let (tx, rx) = std::sync::mpsc::channel();
	std::thread::Builder::new()
		.name("omp-acp-auth".to_string())
		.spawn(move || {
			let _ = tx.send(work());
		})
		.map_err(|error| {
			AuthError::new(
				crate::auth::error::AuthErrorKind::Configuration,
				format!("cannot spawn ACP auth worker: {error}"),
			)
		})?;
	rx.recv().map_err(|_| {
		AuthError::new(
			crate::auth::error::AuthErrorKind::Configuration,
			"ACP auth worker thread dropped",
		)
	})?
}

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
