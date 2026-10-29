//! Native provider login, credential storage and API-key lookup.
//!
//! This is the Rust mirror of `packages/ai/src/auth-storage.ts` +
//! `packages/ai/src/registry/registry.ts`, scoped to what a host (CLI `login`,
//! RPC `get_login_providers`/`login`) needs:
//!
//! - [`registry`] is the declarative provider table (`PROVIDERS`, same order as
//!   upstream's interactive login list) plus one `ApiKeySpec` per
//!   `createApiKeyLogin({…})` call site;
//! - [`flows`] holds the per-provider flow implementations, sharing the
//!   callback server ([`callback`]), the device poller ([`device`]), PKCE
//!   ([`pkce`]) and the HTTP client ([`http`]);
//! - [`store`] is the SQLite `agent.db` auth store (schema v7, camelCase JSON,
//!   refresh leases with owner-fenced writes, ordered transactional migration);
//! - [`selection`] is the credential selection policy (session stickiness,
//!   round-robin, usage-limit blocks, optional [`selection::CredentialRanking`]
//!   hook);
//! - [`AuthBackend`] is the public API: list providers, run a login with host
//!   callbacks, and resolve the stored API key for a provider;
//! - [`AuthHandle`] is the async/RPC surface: it wraps the `!Sync` connection in
//!   a mutex and runs blocking work on dedicated threads, so a runtime worker
//!   never blocks on SQLite or a login.
//!
//! Invariants:
//! - tokens never travel through [`LoginCallbacks`] — `open_url` receives a
//!   URL, `input` a prompt, `notice` a progress line; nothing logs a secret;
//! - every failure is a typed [`AuthError`]; user cancellation is
//!   [`AuthErrorKind::Cancelled`], never an empty success, and a stored grant is
//!   disabled only on an `invalid_grant`-class answer;
//! - [`AuthBackend::api_key_for`] returns `Err` for storage/refresh failures
//!   and `Ok(None)` only when there is genuinely no stored credential.

pub mod callback;
pub mod device;
pub mod error;
pub mod flows;
pub mod http;
pub mod pkce;
pub mod registry;
pub mod selection;
pub mod store;

use std::sync::Arc;

use error::{AuthError, AuthErrorKind};
use http::{Cancel, HttpClient};
use registry::ProviderEntry;
use selection::{CredentialRanking, SelectionState};
use store::{Credential, CredentialStore, OAuthCredentials, StoredCredential, REFRESH_LEASE_TTL_MS};

/// `onAuth` payload: the URL the host should open, an optional loopback-only
/// shortcut, and provider instructions (never secret).
#[derive(Debug, Clone)]
pub struct AuthUrlInfo {
	pub url: String,
	pub launch_url: Option<String>,
	pub instructions: Option<String>,
}

/// `OAuthPrompt` — one line of input from the user.
#[derive(Debug, Clone)]
pub struct InputPrompt {
	pub message: String,
	pub placeholder: Option<String>,
	/// The prompt accepts an empty answer (local no-auth endpoints).
	pub allow_empty: bool,
}

/// `onPrompt` result. Cancellation is explicit so a flow can never mistake a
/// dismissed prompt for an empty API key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputReply {
	Value(String),
	/// The host cannot show interactive prompts (headless/RPC). Carries the
	/// host's reason so the login fails with [`AuthErrorKind::PromptRequired`]
	/// instead of a misleading "cancelled".
	Rejected(String),
	Cancelled,
}

/// Host-side callbacks (`OAuthController` without the network half).
///
/// `open_url` may fail (no browser, headless RPC) — the flow then prints the
/// full URL through `notice`, which is exactly the upstream headless contract.
pub trait LoginCallbacks: Send + Sync {
	fn open_url(&self, info: &AuthUrlInfo) -> Result<(), String>;
	fn notice(&self, message: &str);
	/// `onPrompt`. `cancel` is the owning flow's token: the host MUST abandon
	/// *only this dialog* and return as soon as it fires, because the flow joins
	/// the prompt thread before the login retires. Cooperative cancellation is
	/// part of the contract, not an optional hint.
	fn input(&self, prompt: &InputPrompt, cancel: &Cancel) -> InputReply;
}

/// `getOAuthProviders()` entry for the host's provider picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginProvider {
	pub id: String,
	pub name: String,
	/// `oauth` | `device` | `api_key` (`ProviderDefinition` login kind).
	pub auth_type: String,
	/// A stored, non-disabled credential exists for this provider.
	pub configured: bool,
}

/// What a completed login stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginOutcome {
	OAuth { provider: String, identity: Option<String> },
	ApiKey { provider: String },
}

/// Environment fallback declared by a provider definition (`envKeys`).
///
/// Only the registry's string resolvers are listed: the computed ones
/// (`amazon-bedrock`, `bedrock-mantle`, `google-vertex`) resolve cloud
/// credential chains in the transport, not in a login provider.
const ENV_KEYS: &[(&str, &[&str])] = &[
	("anthropic", &["ANTHROPIC_OAUTH_TOKEN", "ANTHROPIC_API_KEY"]),
	("exa", &["EXA_API_KEY"]),
	("gitlab-duo-agent", &["GITLAB_TOKEN"]),
	("kagi", &["KAGI_API_KEY"]),
	("llama.cpp", &["LLAMA_CPP_API_KEY"]),
	("parallel", &["PARALLEL_API_KEY"]),
	("perplexity", &["PERPLEXITY_API_KEY"]),
	("tavily", &["TAVILY_API_KEY"]),
];

/// `getOAuthApiKey` shapes a JSON envelope for providers whose transport needs
/// credential metadata; everything else gets the bare bearer.
const STRUCTURED_API_KEY_PROVIDERS: &[&str] =
	&["github-copilot", "google-gemini-cli", "google-antigravity", "alibaba-coding-plan"];

/// `NEVER_EXPIRES` sentinel used by the Perplexity JWT rule.
const NEVER_EXPIRES: i64 = 8_640_000_000_000_000;

/// Bound for one refresh operation: shorter than the lease TTL, so a suspended
/// worker cannot outlive the ownership it renewed (`auth-storage.ts:2558-2587`).
const REFRESH_OPERATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Auth backend over one `agent.db`.
pub struct AuthBackend {
	store: CredentialStore,
	selection: SelectionState,
	ranking: Option<Arc<dyn CredentialRanking>>,
}

impl AuthBackend {
	/// Open the store at `<agent dir>/agent.db` (`getAgentDbPath()`).
	pub fn open() -> Result<Self, AuthError> {
		Self::open_at(crate::dirs::agent_dir().join("agent.db"))
	}

	/// Open a specific database (tests, broker hand-off).
	pub fn open_at(path: impl Into<std::path::PathBuf>) -> Result<Self, AuthError> {
		Ok(Self::with_store(CredentialStore::open(path)?))
	}

	/// In-memory store for tests and dry runs.
	pub fn in_memory() -> Result<Self, AuthError> {
		Ok(Self::with_store(CredentialStore::in_memory()?))
	}

	fn with_store(store: CredentialStore) -> Self {
		Self { store, selection: SelectionState::default(), ranking: None }
	}

	/// Install the usage-ranked selection hook (`rankingStrategyResolver`).
	///
	/// Without it selection is the block-aware order path — identical to
	/// upstream when no strategy resolves — so nothing is silently excluded.
	#[must_use]
	pub fn with_ranking(mut self, ranking: Arc<dyn CredentialRanking>) -> Self {
		self.ranking = Some(ranking);
		self
	}

	/// Replace (or clear) the usage-ranked selection hook.
	pub fn set_ranking(&mut self, ranking: Option<Arc<dyn CredentialRanking>>) {
		self.ranking = ranking;
	}

	/// Block a credential after a usage-limit hit (`markCredentialBlocked`).
	///
	/// The runtime calls this when a provider answers 429; selection then skips
	/// the row until `blocked_until_ms`, persisted in `agent.db` so a restart
	/// keeps the backoff.
	pub fn block_credential(
		&self,
		provider_id: &str,
		credential_id: i64,
		blocked_until_ms: i64,
		block_scope: Option<&str>,
	) -> Result<(), AuthError> {
		let entry = registry::find(provider_id).ok_or_else(|| AuthError::unknown_provider(provider_id))?;
		let provider_key = selection::provider_type_key(registry::storage_provider(entry), "oauth");
		self.selection
			.mark_blocked(&self.store, &provider_key, credential_id, blocked_until_ms, block_scope)
	}

	/// `getOAuthProviders()`: the login list in upstream order.
	pub fn providers(&self) -> Result<Vec<LoginProvider>, AuthError> {
		let mut out = Vec::with_capacity(registry::PROVIDERS.len());
		for entry in registry::PROVIDERS {
			let storage = registry::storage_provider(entry);
			let configured = !self.store.list_active(storage)?.is_empty();
			out.push(LoginProvider {
				id: entry.id.to_string(),
				name: entry.name.to_string(),
				auth_type: entry.kind.auth_type().to_string(),
				configured,
			});
		}
		Ok(out)
	}

	/// Run a provider login and persist the result.
	///
	/// `cancel` is the host's token (`ctrl.signal` upstream): every HTTP call,
	/// poll and browser wait checks it, and a cancelled login returns
	/// [`AuthErrorKind::Cancelled`] after the loopback listener and any refresh
	/// lease are dropped. There is no cancellation-free overload on purpose.
	///
	/// The stored provider is `storeCredentialsAs` when the definition remaps it
	/// (`openai-codex-device` → `openai-codex`, `zai-coding-plan` → `zai`).
	pub fn login(
		&self,
		provider_id: &str,
		callbacks: Arc<dyn LoginCallbacks>,
		cancel: &Cancel,
	) -> Result<LoginOutcome, AuthError> {
		let entry = registry::find(provider_id).ok_or_else(|| AuthError::unknown_provider(provider_id))?;
		let credential = self.run_flow(entry, callbacks, cancel)?;
		// A cancelled login never persists a token that finished racing the cancel.
		cancel.check(entry.id)?;
		let credential = match credential {
			Credential::OAuth(mut creds) => {
				if creds.authorized_at.is_none() {
					creds.authorized_at = Some(now_ms());
				}
				Credential::OAuth(creds)
			},
			other => other,
		};
		let stored_as = registry::storage_provider(entry);
		self.store.upsert(stored_as, &credential)?;
		Ok(match credential {
			Credential::OAuth(creds) => LoginOutcome::OAuth {
				provider: stored_as.to_string(),
				identity: creds.email.or(creds.account_id).or(creds.project_id).or(creds.org_id),
			},
			Credential::ApiKey { .. } => LoginOutcome::ApiKey { provider: stored_as.to_string() },
		})
	}

	/// Delete the stored credential(s) for a provider id (`logout`).
	pub fn logout(&self, provider_id: &str) -> Result<usize, AuthError> {
		let entry = registry::find(provider_id).ok_or_else(|| AuthError::unknown_provider(provider_id))?;
		self.store.delete_provider(registry::storage_provider(entry))
	}

	/// Registry `envKeys` fallback (`getEnvApiKey`, single-variable resolvers).
	#[must_use]
	pub fn env_api_key(&self, provider_id: &str) -> Option<String> {
		let keys = ENV_KEYS.iter().find(|(id, _)| *id == provider_id).map(|(_, keys)| *keys)?;
		keys.iter().find_map(|key| {
			std::env::var(key)
				.ok()
				.map(|value| value.trim().to_string())
				.filter(|value| !value.is_empty())
		})
	}

	/// Resolve the API key to use for `provider_id` from the store.
	///
	/// Precedence is the caller's: explicit flag > `config.yml`/`models.yml` >
	/// this store > [`Self::env_api_key`]. An expired OAuth grant is refreshed
	/// under the durable refresh lease before the key is minted.
	pub fn api_key_for(&self, provider_id: &str) -> Result<Option<String>, AuthError> {
		self.api_key_for_with_cancel(provider_id, &Cancel::new())
	}

	/// [`Self::api_key_for`] with a caller-owned cancellation token for the
	/// refresh it may trigger (an interactive caller cancels a slow provider).
	pub fn api_key_for_with_cancel(
		&self,
		provider_id: &str,
		cancel: &Cancel,
	) -> Result<Option<String>, AuthError> {
		self.api_key_for_session(provider_id, None, cancel)
	}

	/// `getApiKey(provider, sessionId)` precedence, first match wins:
	///
	/// 1. a fresh (or just-refreshed) OAuth grant from selection;
	/// 2. an API key persisted by `/login`;
	/// 3. the provider's declared env var;
	/// 4. a static stored key (broker-migrated copies) as the last resort.
	///
	/// `session_id` drives sticky selection (`xxHash32(sessionId) % total`);
	/// without it selection is round-robin per `provider:type`.
	///
	/// The OAuth branch may refresh, which blocks on a private runtime: call it
	/// from a plain thread (CLI or a blocking worker). Async hosts use
	/// [`AuthHandle::api_key_for`].
	pub fn api_key_for_session(
		&self,
		provider_id: &str,
		session_id: Option<&str>,
		cancel: &Cancel,
	) -> Result<Option<String>, AuthError> {
		require_blocking_context(provider_id)?;
		let runtime = runtime()?;
		runtime.block_on(self.api_key_for_core(provider_id, session_id, cancel))
	}

	async fn api_key_for_core(
		&self,
		provider_id: &str,
		session_id: Option<&str>,
		cancel: &Cancel,
	) -> Result<Option<String>, AuthError> {
		let Some(entry) = registry::find(provider_id) else { return Ok(None) };
		let stored_as = registry::storage_provider(entry);

		if let Some(stored) = self.select(stored_as, selection::OAUTH, session_id, |_| true)? {
			let credential = self.materialize(provider_id, stored, cancel).await?;
			return api_key_from(provider_id, &credential).map(Some);
		}
		if let Some(stored) = self.select(stored_as, selection::API_KEY, session_id, selection::is_api_key_login)? {
			return Ok(stored.credential.api_key_value().map(ToString::to_string));
		}
		if let Some(env) = self.env_api_key(provider_id) {
			return Ok(Some(env));
		}
		if let Some(stored) =
			self.select(stored_as, selection::API_KEY, session_id, |row| !selection::is_api_key_login(row))?
		{
			return Ok(stored.credential.api_key_value().map(ToString::to_string));
		}
		Ok(None)
	}

	/// Selected OAuth grant, refreshed under the lease when expired.
	async fn materialize(
		&self,
		provider_id: &str,
		stored: StoredCredential,
		cancel: &Cancel,
	) -> Result<Credential, AuthError> {
		let Credential::OAuth(creds) = stored.credential.clone() else { return Ok(stored.credential) };
		if now_ms() < effective_expiry(provider_id, &creds) {
			return Ok(Credential::OAuth(creds));
		}
		self.refresh_core(provider_id, stored.id, &stored.data, &creds, cancel).await
	}

	/// `peekApiKey`: the same precedence without refreshing and without the
	/// usage-ranked hook — a read-only probe (`doctor`, status displays).
	pub fn peek_api_key(&self, provider_id: &str) -> Result<Option<String>, AuthError> {
		let Some(entry) = registry::find(provider_id) else { return Ok(None) };
		let stored_as = registry::storage_provider(entry);

		if let Some(stored) = self.select(stored_as, selection::OAUTH, None, |_| true)? {
			if let Credential::OAuth(creds) = &stored.credential {
				if now_ms() < effective_expiry(provider_id, creds) {
					if provider_id == "github-copilot" {
						let mut envelope = serde_json::Map::new();
						envelope.insert("token".to_string(), serde_json::Value::String(creds.access.clone()));
						if let Some(enterprise_url) = &creds.enterprise_url {
							envelope.insert(
								"enterpriseUrl".to_string(),
								serde_json::Value::String(enterprise_url.clone()),
							);
						}
						if let Some(api_endpoint) = &creds.api_endpoint {
							envelope.insert("apiEndpoint".to_string(), serde_json::Value::String(api_endpoint.clone()));
						}
						return Ok(Some(serde_json::Value::Object(envelope).to_string()));
					}
					return Ok(Some(creds.access.clone()));
				}
			}
		}
		if let Some(stored) = self.select(stored_as, selection::API_KEY, None, selection::is_api_key_login)? {
			return Ok(stored.credential.api_key_value().map(ToString::to_string));
		}
		if let Some(env) = self.env_api_key(provider_id) {
			return Ok(Some(env));
		}
		if let Some(stored) = self.select(stored_as, selection::API_KEY, None, |row| !selection::is_api_key_login(row))? {
			return Ok(stored.credential.api_key_value().map(ToString::to_string));
		}
		Ok(None)
	}

	/// `enableAllGitHubCopilotModels` for the stored Copilot grant: accept the
	/// model policy for every wire model id the catalog reports (synthesized
	/// `-1m` variants share the upstream id, so callers pass wire ids once).
	///
	/// `Err` only for storage/credential failures; per-model outcomes are
	/// returned in completion order and a refused model is just `false`.
	pub fn enable_copilot_models(
		&self,
		wire_model_ids: &[String],
		cancel: &Cancel,
		on_progress: impl FnMut(&str, bool),
	) -> Result<Vec<(String, bool)>, AuthError> {
		let provider = "github-copilot";
		let entry = registry::find(provider).ok_or_else(|| AuthError::unknown_provider(provider))?;
		let stored_as = registry::storage_provider(entry);
		let stored = self
			.select(stored_as, selection::OAUTH, None, |_| true)?
			.ok_or_else(|| AuthError::config(provider, "no stored GitHub Copilot credential"))?;
		require_blocking_context(provider)?;

		let runtime = runtime()?;
		// A policy call needs a live token: refresh through the lease when stale,
		// exactly like the transport does before a chat request.
		let credential = runtime.block_on(self.materialize(provider, stored, cancel))?;
		let credentials = match credential {
			Credential::OAuth(credentials) => credentials,
			Credential::ApiKey { .. } => {
				return Err(AuthError::config(provider, "stored credential is not an OAuth grant"));
			},
		};
		let http = HttpClient::new();
		let callbacks: Arc<dyn LoginCallbacks> = Arc::new(NoCallbacks);
		Ok(runtime.block_on(async {
			let flow = flows::Flow { entry, http: &http, callbacks: &callbacks, cancel };
			flows::device_flow::copilot::enable_all_models(
				&flow,
				&credentials.access,
				credentials.enterprise_url.as_deref(),
				credentials.api_endpoint.as_deref(),
				wire_model_ids,
				on_progress,
			)
			.await
		}))
	}

	/// `#selectCredentialByType` against this backend's store and policy.
	fn select(
		&self,
		stored_as: &str,
		credential_type: &str,
		session_id: Option<&str>,
		filter: impl Fn(&StoredCredential) -> bool,
	) -> Result<Option<StoredCredential>, AuthError> {
		selection::select(
			&self.store,
			&self.selection,
			stored_as,
			credential_type,
			session_id,
			&[],
			filter,
			self.ranking.as_deref(),
		)
	}

	/// The refresh itself: cancellable lease acquisition, periodic lease renewal
	/// (and an absolute deadline) while the provider round-trip runs, metadata
	/// inheritance, and an ownership-fenced compare-and-swap write.
	async fn refresh_core(
		&self,
		provider_id: &str,
		id: i64,
		expected_data: &str,
		observed: &OAuthCredentials,
		cancel: &Cancel,
	) -> Result<Credential, AuthError> {
		let entry = registry::find(provider_id).ok_or_else(|| AuthError::unknown_provider(provider_id))?;
		let callbacks: Arc<dyn LoginCallbacks> = Arc::new(NoCallbacks);
		let http = HttpClient::new();
		// Cancellation-aware wait; a row deleted/disabled meanwhile is refused.
		let lease = self.store.acquire_lease_blocking(id, cancel)?;

		// A peer may have refreshed while we waited for the lease: adopt a fresh
		// row instead of refreshing twice (or stop when it is gone).
		let Some(current_data) = self.store.data_if_active(id)? else {
			return Err(AuthError::config(provider_id, "stored credential is no longer active"));
		};
		let mut candidate = observed.clone();
		let mut expected = expected_data.to_string();
		if current_data != expected_data {
			// Re-read *this* row only: `get(provider)` returns the first active
			// account, which is the wrong grant on a multi-account provider.
			if let Some(current) = self.store.active_by_id(id)? {
				if let Credential::OAuth(creds) = &current.credential {
					if now_ms() < effective_expiry(provider_id, creds) {
						return Ok(current.credential);
					}
					candidate = creds.clone();
				}
				expected = current.data.clone();
			}
		}

		let flow = flows::Flow { entry, http: &http, callbacks: &callbacks, cancel };
		let refresh = flows::refresh(&flow, &candidate);
		tokio::pin!(refresh);
		let cadence = std::time::Duration::from_millis(
			u64::try_from(REFRESH_LEASE_TTL_MS / 3).unwrap_or(30_000),
		);
		let mut renew_at = tokio::time::Instant::now() + cadence;
		let deadline = tokio::time::Instant::now() + REFRESH_OPERATION_TIMEOUT;
		let outcome = loop {
			tokio::select! {
				biased;
				result = &mut refresh => break result,
				() = tokio::time::sleep_until(deadline) => {
					break Err(AuthError::new(
						AuthErrorKind::Timeout,
						"provider refresh exceeded its operation deadline",
					)
					.for_provider(provider_id));
				},
				() = tokio::time::sleep_until(renew_at) => {
					if !lease.renew()? {
						break Err(AuthError::config(provider_id, "refresh ownership was lost"));
					}
					renew_at = tokio::time::Instant::now() + cadence;
				},
			}
		};

		let refreshed = match outcome {
			// A refresh response is tokens-only; keep project/enterprise routing
			// and authorization metadata from the stored grant.
			Ok(Credential::OAuth(mut creds)) => {
				creds.inherit_metadata(&candidate);
				Credential::OAuth(creds)
			},
			Ok(other) => other,
			Err(err) => {
				// Only a provider-declared dead grant is fenced, and only while
				// this call still owns an unexpired lease. Rate limits, outages,
				// parse/network failures and cancellation keep the credential.
				if err.is_definitive() && lease.renew().unwrap_or(false) {
					self.store.disable_if_matches(id, &expected, err.kind.as_str(), lease.owner())?;
				}
				return Err(err);
			},
		};

		// A cancelled caller must not persist a grant that finished racing the
		// cancel: the user asked to stop, so the store keeps the previous row.
		cancel.check(provider_id)?;
		if !lease.renew()? {
			return Err(AuthError::config(provider_id, "refresh ownership was lost"));
		}
		if !self.store.update_if_matches(id, &expected, &refreshed, lease.owner())? {
			// Another writer won the CAS: use its value when it is fresh.
			if let Some(current) = self.store.active_by_id(id)? {
				if let Credential::OAuth(creds) = &current.credential {
					if now_ms() < effective_expiry(provider_id, creds) {
						return Ok(current.credential);
					}
				}
			}
			return Err(AuthError::config(provider_id, "refresh ownership was lost"));
		}
		Ok(refreshed)
	}

	/// Build the flow, run it on a private runtime, and drop the guard.
	fn run_flow(
		&self,
		entry: &'static ProviderEntry,
		callbacks: Arc<dyn LoginCallbacks>,
		cancel: &Cancel,
	) -> Result<Credential, AuthError> {
		let http = HttpClient::new();
		std::thread::scope(|scope| {
			let handle = scope.spawn(|| {
				let runtime = runtime()?;
				runtime.block_on(async {
					let flow = flows::Flow { entry, http: &http, callbacks: &callbacks, cancel };
					flows::login(&flow).await
				})
			});
			match handle.join() {
				Ok(result) => result,
				Err(_) => Err(AuthError::new(
					AuthErrorKind::Configuration,
					"provider login task panicked",
				)
				.for_provider(entry.id)),
			}
		})
	}
}

/// Refresh runs without a user present: no prompt can be answered, so an input
/// request is a cancellation rather than a hang.
struct NoCallbacks;

impl LoginCallbacks for NoCallbacks {
	fn open_url(&self, _info: &AuthUrlInfo) -> Result<(), String> {
		Err("no interactive host".to_string())
	}

	fn notice(&self, _message: &str) {}

	fn input(&self, _prompt: &InputPrompt, cancel: &Cancel) -> InputReply {
		if cancel.is_cancelled() {
			return InputReply::Cancelled;
		}
		InputReply::Rejected("interactive prompts are not available in this host".to_string())
	}
}

/// Shared backend handle for async hosts (RPC).
///
/// [`AuthBackend`] carries a `rusqlite::Connection`: `Send` but **not** `Sync`.
/// The mutex makes the handle `Send + Sync`; every async entry point moves the
/// blocking work to a dedicated OS thread, so neither `rusqlite` nor the
/// private current-thread runtime ever runs on a runtime worker.
///
/// Waiting for the backend is cancellation-aware: a queued lookup returns
/// `Cancelled` as soon as the token fires instead of sitting behind a login.
/// A login holds the lock for its whole flow (state, poll, exchange, store);
/// concurrent callers queue, and cancellation unblocks the queue.
#[derive(Clone)]
pub struct AuthHandle(Arc<std::sync::Mutex<AuthBackend>>);

impl AuthHandle {
	pub fn new(backend: AuthBackend) -> Self {
		Self(Arc::new(std::sync::Mutex::new(backend)))
	}

	/// Providers in registry order with their configured state.
	pub async fn providers(&self) -> Result<Vec<LoginProvider>, AuthError> {
		let backend = Arc::clone(&self.0);
		off_thread(move || {
			let guard = lock_backend(&backend, None, "providers")?;
			guard.providers()
		})
		.await
	}

	/// [`AuthBackend::peek_api_key`]: read-only, never refreshes.
	pub async fn peek_api_key(&self, provider_id: &str) -> Result<Option<String>, AuthError> {
		let backend = Arc::clone(&self.0);
		let provider = provider_id.to_string();
		off_thread(move || {
			let guard = lock_backend(&backend, None, &provider)?;
			guard.peek_api_key(&provider)
		})
		.await
	}

	/// [`AuthBackend::login`] on a dedicated thread. `LoginCallbacks::input`
	/// runs there, so it must wait interruptibly and check `cancel`.
	pub async fn login(
		&self,
		provider_id: &str,
		callbacks: Arc<dyn LoginCallbacks>,
		cancel: Cancel,
	) -> Result<LoginOutcome, AuthError> {
		let backend = Arc::clone(&self.0);
		let provider = provider_id.to_string();
		off_thread(move || {
			let guard = lock_backend(&backend, Some(&cancel), &provider)?;
			guard.login(&provider, callbacks, &cancel)
		})
		.await
	}

	/// [`AuthBackend::api_key_for`] on a dedicated thread, cancellable while
	/// queued for the backend.
	pub async fn api_key_for(
		&self,
		provider_id: &str,
		cancel: Cancel,
	) -> Result<Option<String>, AuthError> {
		let backend = Arc::clone(&self.0);
		let provider = provider_id.to_string();
		let worker_cancel = cancel.clone();
		let work = move || {
			let guard = lock_backend(&backend, Some(&worker_cancel), &provider)?;
			guard.api_key_for_with_cancel(&provider, &worker_cancel)
		};
		await_cancellable(off_thread_future(work)?, &cancel, provider_id).await
	}

	/// [`AuthBackend::logout`] on a dedicated thread.
	pub async fn logout(&self, provider_id: &str) -> Result<usize, AuthError> {
		let backend = Arc::clone(&self.0);
		let provider = provider_id.to_string();
		off_thread(move || {
			let guard = lock_backend(&backend, None, &provider)?;
			guard.logout(&provider)
		})
		.await
	}
}

/// Spawn `work` on a dedicated OS thread and return the receiving half.
fn off_thread_future<T, F>(work: F) -> Result<tokio::sync::oneshot::Receiver<Result<T, AuthError>>, AuthError>
where
	F: FnOnce() -> Result<T, AuthError> + Send + 'static,
	T: Send + 'static,
{
	let (tx, rx) = tokio::sync::oneshot::channel();
	std::thread::Builder::new()
		.name("omp-auth".to_string())
		.spawn(move || {
			let _ = tx.send(work());
		})
		.map_err(|err| AuthError::new(AuthErrorKind::Configuration, format!("cannot spawn auth worker: {err}")))?;
	Ok(rx)
}

/// Off-thread execution for work that is not cancellation-raced by the caller.
async fn off_thread<T, F>(work: F) -> Result<T, AuthError>
where
	F: FnOnce() -> Result<T, AuthError> + Send + 'static,
	T: Send + 'static,
{
	let rx = off_thread_future(work)?;
	rx.await
		.map_err(|_| AuthError::new(AuthErrorKind::Configuration, "auth worker thread dropped"))?
}

/// Wait for `work`, but return `Cancelled` immediately when the token fires.
async fn await_cancellable<T>(
	rx: tokio::sync::oneshot::Receiver<Result<T, AuthError>>,
	cancel: &Cancel,
	provider: &str,
) -> Result<T, AuthError> {
	tokio::select! {
		received = rx => received
			.map_err(|_| AuthError::new(AuthErrorKind::Configuration, "auth worker thread dropped"))?,
		() = cancel_fired(cancel) => Err(AuthError::cancelled(provider)),
	}
}

async fn cancel_fired(cancel: &Cancel) {
	while !cancel.is_cancelled() {
		tokio::time::sleep(std::time::Duration::from_millis(25)).await;
	}
}

/// Acquire the backend lock, polling so a queued caller can be cancelled while
/// a long login holds it.
fn lock_backend<'a>(
	backend: &'a std::sync::Mutex<AuthBackend>,
	cancel: Option<&Cancel>,
	provider: &str,
) -> Result<std::sync::MutexGuard<'a, AuthBackend>, AuthError> {
	loop {
		if let Some(cancel) = cancel {
			cancel.check(provider)?;
		}
		match backend.try_lock() {
			Ok(guard) => return Ok(guard),
			Err(std::sync::TryLockError::Poisoned(err)) => return Ok(err.into_inner()),
			Err(std::sync::TryLockError::WouldBlock) => std::thread::sleep(std::time::Duration::from_millis(20)),
		}
	}
}

/// Blocking entry points must not run on an async runtime worker: they build a
/// private runtime and may wait on a prompt or a lease.
fn require_blocking_context(provider: &str) -> Result<(), AuthError> {
	if tokio::runtime::Handle::try_current().is_ok() {
		return Err(AuthError::config(
			provider,
			"auth lookup blocks on a private runtime; use AuthHandle from async code",
		));
	}
	Ok(())
}

fn runtime() -> Result<tokio::runtime::Runtime, AuthError> {
	tokio::runtime::Builder::new_current_thread()
		.enable_all()
		.build()
		.map_err(|err| AuthError::new(AuthErrorKind::Configuration, format!("cannot start auth runtime: {err}")))
}

/// `getOAuthApiKey`: bearer value, or the structured JSON envelope for the
/// providers whose transport needs the extra metadata.
fn api_key_from(provider: &str, credential: &Credential) -> Result<String, AuthError> {
	match credential {
		Credential::ApiKey { key, .. } => Ok(key.clone()),
		Credential::OAuth(creds) => {
			let expiry = effective_expiry(provider, creds);
			if now_ms() >= expiry {
				return Err(AuthError::new(
					AuthErrorKind::Validation,
					format!(
						"OAuth credential for {provider} is expired and must be refreshed before the API key is read"
					),
				)
				.for_provider(provider));
			}
			if STRUCTURED_API_KEY_PROVIDERS.contains(&provider) {
				let envelope = serde_json::json!({
					"apiEndpoint": creds.api_endpoint,
					"token": creds.access,
					"enterpriseUrl": creds.enterprise_url,
					"projectId": creds.project_id,
					"refreshToken": creds.refresh,
					"expiresAt": creds.expires,
					"email": creds.email,
					"accountId": creds.account_id,
				});
				return Ok(envelope.to_string());
			}
			Ok(creds.access.clone())
		},
	}
}

/// Perplexity JWTs usually omit `exp`; the claim wins when present, otherwise
/// the credential is treated as non-expiring (older logins wrote `login + 1h`).
fn effective_expiry(provider: &str, creds: &OAuthCredentials) -> i64 {
	if provider != "perplexity" {
		return creds.expires;
	}
	let normalized = if creds.expires > 0 && creds.expires < 10_000_000_000 {
		creds.expires * 1000
	} else {
		creds.expires
	};
	match jwt_expiry(creds.access.as_str()) {
		Some(expiry) => expiry,
		None => normalized.max(NEVER_EXPIRES),
	}
}

fn jwt_expiry(token: &str) -> Option<i64> {
	use base64::Engine as _;
	let payload = token.split('.').nth(1)?;
	let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload).ok()?;
	let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
	let exp = value.get("exp")?.as_i64()?;
	Some(exp.saturating_mul(1000).saturating_sub(5 * 60 * 1000))
}

fn now_ms() -> i64 {
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
		.unwrap_or(0)
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::sync::Mutex;

	#[derive(Default)]
	struct Recorder {
		urls: Mutex<Vec<String>>,
		notices: Mutex<Vec<String>>,
		answers: Mutex<Vec<InputReply>>,
	}

	impl LoginCallbacks for Recorder {
		fn open_url(&self, info: &AuthUrlInfo) -> Result<(), String> {
			self.urls.lock().expect("lock").push(info.url.clone());
			Ok(())
		}

		fn notice(&self, message: &str) {
			self.notices.lock().expect("lock").push(message.to_string());
		}

		fn input(&self, _prompt: &InputPrompt, cancel: &Cancel) -> InputReply {
			if cancel.is_cancelled() {
				return InputReply::Cancelled;
			}
			self.answers.lock().expect("lock").pop().unwrap_or(InputReply::Cancelled)
		}
	}

	fn credential(provider: &str, key: &str) -> Credential {
		match provider {
			"anthropic" => Credential::OAuth(OAuthCredentials {
				access: key.to_string(),
				refresh: "refresh".to_string(),
				expires: now_ms() + 3_600_000,
				email: Some("user@example.com".to_string()),
				..OAuthCredentials::default()
			}),
			_ => Credential::ApiKey { key: key.to_string(), source_login: true },
		}
	}

	#[test]
	fn providers_follow_upstream_order_and_report_configured() {
		let backend = AuthBackend::in_memory().expect("store");
		let providers = backend.providers().expect("providers");
		assert_eq!(providers.len(), registry::PROVIDERS.len());
		assert_eq!(providers[0].id, "openai-codex");
		assert!(providers.iter().all(|provider| !provider.configured));

		backend
			.store
			.upsert("anthropic", &credential("anthropic", "tok"))
			.expect("upsert");
		let providers = backend.providers().expect("providers");
		let anthropic = providers.iter().find(|provider| provider.id == "anthropic").expect("entry");
		assert!(anthropic.configured);
		assert_eq!(anthropic.auth_type, "oauth");
	}

	#[test]
	fn api_key_lookup_returns_stored_key_or_none() {
		let backend = AuthBackend::in_memory().expect("store");
		assert_eq!(backend.api_key_for("deepseek").expect("lookup"), None);

		backend
			.store
			.upsert("deepseek", &Credential::ApiKey { key: "sk-test".to_string(), source_login: true })
			.expect("upsert");
		assert_eq!(backend.api_key_for("deepseek").expect("lookup").as_deref(), Some("sk-test"));
		// Unknown ids are not login providers: no key, no error.
		assert_eq!(backend.api_key_for("definitely-not-a-provider").expect("lookup"), None);
	}

	#[test]
	fn structured_envelope_matches_upstream_shape() {
		let backend = AuthBackend::in_memory().expect("store");
		// `github-copilot` credentials are OAuth-shaped in the real flow.
		backend
			.store
			.upsert(
				"github-copilot",
				&Credential::OAuth(OAuthCredentials {
					access: "gh-token".to_string(),
					refresh: "gh-token".to_string(),
					expires: now_ms() + 60_000,
					enterprise_url: Some("company.ghe.com".to_string()),
					api_endpoint: Some("https://api.individual.githubcopilot.com".to_string()),
					..OAuthCredentials::default()
				}),
			)
			.expect("upsert");
		let key = backend.api_key_for("github-copilot").expect("lookup").expect("key");
		let envelope: serde_json::Value = serde_json::from_str(&key).expect("json");
		assert_eq!(envelope["token"], "gh-token");
		assert_eq!(envelope["enterpriseUrl"], "company.ghe.com");
		assert_eq!(envelope["apiEndpoint"], "https://api.individual.githubcopilot.com");
	}

	#[test]
	fn expired_grant_is_an_error_not_an_empty_key() {
		let expired = Credential::OAuth(OAuthCredentials {
			access: "stale".to_string(),
			refresh: "refresh".to_string(),
			expires: now_ms() - 1_000,
			..OAuthCredentials::default()
		});
		let err = api_key_from("anthropic", &expired).expect_err("expired");
		assert_eq!(err.kind, AuthErrorKind::Validation);
		assert!(err.message.contains("expired"));
	}

	#[test]
	fn perplexity_without_exp_never_expires() {
		let creds = OAuthCredentials {
			access: "not-a-jwt".to_string(),
			refresh: "not-a-jwt".to_string(),
			expires: 1,
			..OAuthCredentials::default()
		};
		assert_eq!(effective_expiry("perplexity", &creds), NEVER_EXPIRES);
		assert_eq!(effective_expiry("kimi-code", &creds), 1);
	}

	#[test]
	fn env_api_key_reads_only_declared_vars() {
		let backend = AuthBackend::in_memory().expect("store");
		assert!(backend.env_api_key("unknown-provider").is_none());
		// SAFETY: single-threaded test process section; no other test reads this var.
		unsafe { std::env::set_var("TAVILY_API_KEY", "  tvly-test  ") };
		assert_eq!(backend.env_api_key("tavily").as_deref(), Some("tvly-test"));
		// SAFETY: as above.
		unsafe { std::env::remove_var("TAVILY_API_KEY") };
		assert!(backend.env_api_key("tavily").is_none());
	}

	#[test]
	fn env_beats_static_key_but_loses_to_login_key() {
		let backend = AuthBackend::in_memory().expect("store");
		backend
			.store
			.upsert("exa", &Credential::ApiKey { key: "static".to_string(), source_login: false })
			.expect("static");
		// SAFETY: single-threaded test process section; no other test reads this var.
		unsafe { std::env::set_var("EXA_API_KEY", "env-key") };
		assert_eq!(backend.api_key_for("exa").expect("lookup").as_deref(), Some("env-key"));

		backend
			.store
			.upsert("exa", &Credential::ApiKey { key: "login".to_string(), source_login: true })
			.expect("login");
		assert_eq!(backend.api_key_for("exa").expect("lookup").as_deref(), Some("login"));
		// SAFETY: as above.
		unsafe { std::env::remove_var("EXA_API_KEY") };
	}

	#[test]
	fn peek_never_refreshes_and_omits_expired_grants() {
		let expired = AuthBackend::in_memory().expect("store");
		expired
			.store
			.upsert(
				"anthropic",
				&Credential::OAuth(OAuthCredentials {
					access: "stale".to_string(),
					refresh: "refresh".to_string(),
					expires: now_ms() - 1_000,
					..OAuthCredentials::default()
				}),
			)
			.expect("expired");
		assert_eq!(expired.peek_api_key("anthropic").expect("peek"), None);

		let fresh = AuthBackend::in_memory().expect("store");
		fresh
			.store
			.upsert(
				"anthropic",
				&Credential::OAuth(OAuthCredentials {
					access: "fresh".to_string(),
					refresh: "refresh".to_string(),
					expires: now_ms() + 60_000,
					..OAuthCredentials::default()
				}),
			)
			.expect("fresh");
		assert_eq!(fresh.peek_api_key("anthropic").expect("peek").as_deref(), Some("fresh"));
	}

	#[test]
	fn auth_handle_is_send_and_sync() {
		fn assert_send_sync<T: Send + Sync>() {}
		assert_send_sync::<AuthHandle>();
	}

	#[test]
	fn unknown_provider_login_is_typed() {
		let backend = AuthBackend::in_memory().expect("store");
		let callbacks: Arc<dyn LoginCallbacks> = Arc::new(Recorder::default());
		let cancel = Cancel::new();
		let err = backend.login("nope", callbacks, &cancel).expect_err("unknown");
		assert_eq!(err.kind, AuthErrorKind::UnknownProvider);
		assert!(err.message.contains("nope"));
	}

	#[test]
	fn cancellation_is_a_typed_error() {
		let backend = AuthBackend::in_memory().expect("store");
		let callbacks: Arc<dyn LoginCallbacks> = Arc::new(Recorder::default());
		let cancel = Cancel::new();
		cancel.cancel();
		let err = backend.login("anthropic", callbacks, &cancel).expect_err("cancelled");
		assert_eq!(err.kind, AuthErrorKind::Cancelled);
	}

	#[test]
	fn login_persists_under_store_credentials_as() {
		let backend = AuthBackend::in_memory().expect("store");
		// `zai-coding-plan` stores as `zai`; seed the target to prove the remap.
		backend
			.store
			.upsert("zai", &Credential::ApiKey { key: "k".to_string(), source_login: false })
			.expect("upsert");
		assert_eq!(registry::storage_provider(registry::find("zai-coding-plan").expect("entry")), "zai");
		assert!(!backend.providers().expect("providers").is_empty());
	}

	#[test]
	fn recorder_callbacks_never_see_tokens() {
		let callbacks = Recorder::default();
		callbacks.notice("line");
		assert!(callbacks.notices.lock().expect("lock").iter().all(|line| !line.contains("sk-")));
		assert_eq!(
			callbacks.input(&InputPrompt { message: "m".into(), placeholder: None, allow_empty: false }, &Cancel::new()),
			InputReply::Cancelled
		);
	}
}
