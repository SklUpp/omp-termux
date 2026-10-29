//! Credential storage in the shared `agent.db`.
//!
//! Anchors: `packages/ai/src/auth/sqlite-credential-store.ts`
//! (`AUTH_SCHEMA_VERSION = 7`, `auth_credentials`, `auth_credential_refresh_leases`,
//! WAL + `synchronous=NORMAL` + `busy_timeout`, `0600`), `serializeCredential`,
//! `deserializeCredential`, `resolveCredentialIdentityKey`,
//! `tryUpdateAuthCredentialIfMatches`, and `auth-storage.ts:2399` refresh leases.
//!
//! Compatibility rules the native side must honour because the TS side keeps
//! writing the same file: identical table and column names, `data` holding the
//! JSON record with `type` stripped, `credential_type` ∈ {`oauth`, `api_key`},
//! `identity_key` derived the same way, and no writes outside the auth tables
//! (session/usage rows belong to other owners).

use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension as _, params};
use serde::Serialize;

use super::error::{AuthError, AuthErrorKind};
use super::http::Cancel;
use super::pkce::random_owner_id;

/// `AUTH_SCHEMA_VERSION` — the version TS records in `auth_schema_version`.
pub const AUTH_SCHEMA_VERSION: i64 = 7;

/// Refresh lease TTL (`OAUTH_REFRESH_LEASE_TTL_MS`).
pub const REFRESH_LEASE_TTL_MS: i64 = 120_000;
const LEASE_POLL_MIN_MS: u64 = 25;
const LEASE_POLL_MAX_MS: u64 = 250;

const SQLITE_NOW_EPOCH: &str = "CAST(strftime('%s','now') AS INTEGER)";

/// OAuth grant as stored by every provider
/// (`packages/ai/src/registry/oauth/types.ts:OAuthCredentials`).
///
/// The JSON contract is camelCase because TS reads the same `agent.db` rows
/// (`sqlite-credential-store.ts:serializeCredential`); the snake_case aliases
/// keep rows written by earlier native builds readable.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthCredentials {
	#[serde(default)]
	pub access: String,
	#[serde(default)]
	pub refresh: String,
	#[serde(default)]
	pub expires: i64,
	#[serde(default, skip_serializing_if = "Option::is_none", alias = "enterprise_url")]
	pub enterprise_url: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none", alias = "project_id")]
	pub project_id: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub email: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none", alias = "account_id")]
	pub account_id: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none", alias = "api_endpoint")]
	pub api_endpoint: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none", alias = "org_id")]
	pub org_id: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none", alias = "org_name")]
	pub org_name: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none", alias = "authorized_at")]
	pub authorized_at: Option<i64>,
}

impl OAuthCredentials {
	/// The stored JSON object, without the `type` discriminant.
	fn data_json(&self) -> Result<String, AuthError> {
		serde_json::to_string(self)
			.map_err(|err| AuthError::new(AuthErrorKind::Storage, format!("cannot encode credential: {err}")))
	}

	/// Everything except the token material — safe for `LoginProvider` display.
	#[must_use]
	pub fn is_expired(&self, now_ms: i64) -> bool {
		now_ms >= self.expires
	}

	/// Keep routing/identity metadata a token refresh does not echo.
	///
	/// A refresh response carries tokens and `expires` only; `projectId`,
	/// `enterpriseUrl`, `apiEndpoint`, account/org identity and `authorizedAt`
	/// belong to the login and must survive the write, or the next Google
	/// discovery loses its project and Copilot loses its enterprise host.
	pub fn inherit_metadata(&mut self, previous: &Self) {
		fn fill(target: &mut Option<String>, previous: &Option<String>) {
			if target.is_none() {
				target.clone_from(previous);
			}
		}
		fill(&mut self.enterprise_url, &previous.enterprise_url);
		fill(&mut self.project_id, &previous.project_id);
		fill(&mut self.email, &previous.email);
		fill(&mut self.account_id, &previous.account_id);
		fill(&mut self.api_endpoint, &previous.api_endpoint);
		fill(&mut self.org_id, &previous.org_id);
		fill(&mut self.org_name, &previous.org_name);
		if self.authorized_at.is_none() {
			self.authorized_at = previous.authorized_at;
		}
		if self.refresh.is_empty() {
			self.refresh.clone_from(&previous.refresh);
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Credential {
	OAuth(OAuthCredentials),
	ApiKey { key: String, source_login: bool },
}

impl Credential {
	#[must_use]
	pub fn credential_type(&self) -> &'static str {
		match self {
			Self::OAuth(_) => "oauth",
			Self::ApiKey { .. } => "api_key",
		}
	}

	fn data_json(&self) -> Result<String, AuthError> {
		match self {
			Self::OAuth(creds) => creds.data_json(),
			Self::ApiKey { key, source_login } => {
				let value = if *source_login {
					serde_json::json!({ "key": key, "source": "login" })
				} else {
					serde_json::json!({ "key": key })
				};
				serde_json::to_string(&value).map_err(|err| {
					AuthError::new(AuthErrorKind::Storage, format!("cannot encode api key: {err}"))
				})
			},
		}
	}

	#[must_use]
	pub fn api_key_value(&self) -> Option<&str> {
		match self {
			Self::ApiKey { key, .. } => Some(key),
			Self::OAuth(creds) => Some(creds.access.as_str()),
		}
	}
}

/// One active row.
#[derive(Debug, Clone)]
pub struct StoredCredential {
	pub id: i64,
	pub provider: String,
	pub credential: Credential,
	pub identity_key: Option<String>,
	pub data: String,
}

#[derive(Debug, Clone)]
struct Row {
	id: i64,
	provider: String,
	credential_type: String,
	data: String,
	identity_key: Option<String>,
}

/// SQLite-backed auth store with refresh leases and compare-and-swap writes.
pub struct CredentialStore {
	path: PathBuf,
	connection: Connection,
}

impl CredentialStore {
	pub fn open(path: impl Into<PathBuf>) -> Result<Self, AuthError> {
		let path = path.into();
		if let Some(parent) = path.parent() {
			if !parent.as_os_str().is_empty() && !parent.exists() {
				std::fs::create_dir_all(parent)?;
				set_mode(parent, 0o700);
			}
		}
		let connection = open_with_retry(&path)?;
		// Best effort, matching TS: an FS that refuses chmod is not fatal.
		set_mode(&path, 0o600);
		let store = Self { path, connection };
		store.initialize()?;
		Ok(store)
	}

	/// In-memory store for tests and for hosts that only need the registry.
	pub fn in_memory() -> Result<Self, AuthError> {
		let connection = Connection::open_in_memory()?;
		let store = Self { path: PathBuf::from(":memory:"), connection };
		store.initialize()?;
		Ok(store)
	}

	#[must_use]
	pub fn path(&self) -> &Path {
		&self.path
	}

	fn initialize(&self) -> Result<(), AuthError> {
		self.connection.busy_timeout(Duration::from_millis(5_000))?;
		if self.path.as_os_str() != ":memory:" {
			self.connection.pragma_update(None, "journal_mode", "WAL")?;
			self.connection.pragma_update(None, "synchronous", "NORMAL")?;
		}
		// One transaction: a failure leaves the file untouched instead of half
		// migrated tables with a version row that claims otherwise.
		let transaction = self.connection.unchecked_transaction()?;
		transaction.execute_batch(
			"CREATE TABLE IF NOT EXISTS auth_schema_version (
				id INTEGER PRIMARY KEY CHECK (id = 1),
				version INTEGER NOT NULL
			);",
		)?;

		if !table_exists(&transaction, "auth_credentials")? {
			transaction.execute_batch(AUTH_CREDENTIALS_DDL)?;
			transaction.execute_batch(AUTH_CREDENTIAL_INDEXES_DDL)?;
			transaction.execute_batch(AUTH_BLOCKS_DDL)?;
			transaction.execute_batch(AUTH_LEASES_DDL)?;
			transaction.execute_batch(AUTH_BLOCK_GUARD_DDL)?;
			transaction.execute_batch(AUTH_BLOCK_COMPAT_DDL)?;
			write_schema_version(&transaction, AUTH_SCHEMA_VERSION)?;
			transaction.commit()?;
			return Ok(());
		}

		let recorded = read_schema_version(&transaction)?;
		let columns = table_columns(&transaction, "auth_credentials")?;
		let version = recorded.unwrap_or_else(|| infer_schema_version(&columns));
		if version > AUTH_SCHEMA_VERSION {
			// A newer TS peer owns this file: never migrate it downwards and
			// never rewrite its version row.
			transaction.execute_batch(AUTH_CREDENTIAL_INDEXES_DDL)?;
			transaction.execute_batch(AUTH_BLOCKS_DDL)?;
			transaction.execute_batch(AUTH_LEASES_DDL)?;
			transaction.commit()?;
			return Ok(());
		}
		if version < AUTH_SCHEMA_VERSION {
			migrate_auth_schema(&transaction, version, &columns)?;
		}
		transaction.execute_batch(AUTH_CREDENTIAL_INDEXES_DDL)?;
		transaction.execute_batch(AUTH_BLOCKS_DDL)?;
		transaction.execute_batch(AUTH_LEASES_DDL)?;
		transaction.execute_batch(AUTH_BLOCK_GUARD_DDL)?;
		transaction.execute_batch(AUTH_BLOCK_COMPAT_DDL)?;
		backfill_identity_keys(&transaction)?;
		if recorded != Some(AUTH_SCHEMA_VERSION) {
			write_schema_version(&transaction, AUTH_SCHEMA_VERSION)?;
		}
		transaction.commit()?;
		Ok(())
	}

	fn rows_for(&self, provider: &str) -> Result<Vec<Row>, AuthError> {
		let mut statement = self.connection.prepare(
			"SELECT id, provider, credential_type, data, identity_key FROM auth_credentials
			 WHERE provider = ? AND disabled_cause IS NULL ORDER BY id ASC",
		)?;
		let rows = statement.query_map(params![provider], |row| {
			Ok(Row {
				id: row.get(0)?,
				provider: row.get(1)?,
				credential_type: row.get(2)?,
				data: row.get(3)?,
				identity_key: row.get(4)?,
			})
		})?;
		let mut out = Vec::new();
		for row in rows {
			out.push(row?);
		}
		Ok(out)
	}

	/// Active credential row by exact id.
	///
	/// Refresh must re-read *the selected row* after acquiring the lease; a
	/// provider-wide lookup returns the first active row and would rotate the
	/// wrong account's token on multi-account providers.
	pub fn active_by_id(&self, id: i64) -> Result<Option<StoredCredential>, AuthError> {
		let row = self
			.connection
			.query_row(
				"SELECT id, provider, credential_type, data, identity_key FROM auth_credentials
				 WHERE id = ? AND disabled_cause IS NULL",
				params![id],
				|row| {
					Ok(Row {
						id: row.get(0)?,
						provider: row.get(1)?,
						credential_type: row.get(2)?,
						data: row.get(3)?,
						identity_key: row.get(4)?,
					})
				},
			)
			.optional()?;
		Ok(row.and_then(stored_from_row))
	}

	/// First active credential for a provider (insertion order, like `listForProvider`).
	pub fn get(&self, provider: &str) -> Result<Option<StoredCredential>, AuthError> {
		for row in self.rows_for(provider)? {
			if let Some(stored) = stored_from_row(row) {
				return Ok(Some(stored));
			}
		}
		Ok(None)
	}

	pub fn list_active(&self, provider: &str) -> Result<Vec<StoredCredential>, AuthError> {
		let mut out = Vec::new();
		for row in self.rows_for(provider)? {
			if let Some(credential) = deserialize(&row) {
				out.push(StoredCredential {
					id: row.id,
					provider: row.provider,
					identity_key: row.identity_key,
					data: row.data,
					credential,
				});
			}
		}
		Ok(out)
	}

	/// Providers with at least one active credential (`configured` in the login list).
	pub fn list_providers(&self) -> Result<Vec<String>, AuthError> {
		let mut statement = self.connection.prepare(
			"SELECT DISTINCT provider FROM auth_credentials WHERE disabled_cause IS NULL ORDER BY provider ASC",
		)?;
		let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
		let mut out = Vec::new();
		for row in rows {
			out.push(row?);
		}
		Ok(out)
	}

	/// `upsert`: replace the row matching the same identity, else insert. API-key
	/// rows have no identity, so they match on the stored key value only — a new
	/// key is a new row, exactly like `matchesReplacementCredential`.
	pub fn upsert(&self, provider: &str, credential: &Credential) -> Result<i64, AuthError> {
		let data = credential.data_json()?;
		let identity_key = identity_key(provider, credential);
		let existing = {
			let rows = self.rows_for(provider)?;
			match credential {
				Credential::ApiKey { key, .. } => rows.into_iter().find(|row| stored_api_key(row).as_deref() == Some(key.as_str())),
				Credential::OAuth(_) => {
					if identity_key.is_some() {
						rows.into_iter().find(|row| row.identity_key == identity_key)
					} else {
						// Identity-less OAuth grant (e.g. a minted static key): one
						// active row per provider, refreshed in place.
						rows.into_iter().next()
					}
				},
			}
		};

		match existing {
			Some(row) => {
				self.connection.execute(
					&format!(
						"UPDATE auth_credentials SET credential_type = ?, data = ?, identity_key = ?, updated_at = {SQLITE_NOW_EPOCH} WHERE id = ?"
					),
					params![credential.credential_type(), data, identity_key, row.id],
				)?;
				Ok(row.id)
			},
			None => {
				self.connection.execute(
					&format!(
						"INSERT INTO auth_credentials (provider, credential_type, data, identity_key, created_at, updated_at)
						 VALUES (?, ?, ?, ?, {SQLITE_NOW_EPOCH}, {SQLITE_NOW_EPOCH})"
					),
					params![provider, credential.credential_type(), data, identity_key],
				)?;
				Ok(self.connection.last_insert_rowid())
			},
		}
	}

	/// Compare-and-swap refresh write, fenced by refresh-lease ownership: the
	/// row must still hold `expected_data`, stay active, and `owner` must hold an
	/// unexpired lease on it at the moment of the write.
	pub fn update_if_matches(
		&self,
		id: i64,
		expected_data: &str,
		credential: &Credential,
		owner: &str,
	) -> Result<bool, AuthError> {
		let data = credential.data_json()?;
		let provider = self
			.connection
			.query_row("SELECT provider FROM auth_credentials WHERE id = ?", params![id], |row| {
				row.get::<_, String>(0)
			})
			.optional()?
			.ok_or_else(|| AuthError::new(AuthErrorKind::Storage, "credential row disappeared"))?;
		let identity_key = identity_key(&provider, credential);
		let now = now_ms();
		let changed = self.connection.execute(
			&format!(
				"UPDATE auth_credentials SET credential_type = ?, data = ?, identity_key = ?, updated_at = {SQLITE_NOW_EPOCH}
				 WHERE id = ? AND data = ? AND disabled_cause IS NULL
				   AND EXISTS (
				     SELECT 1 FROM auth_credential_refresh_leases
				     WHERE credential_id = ? AND owner = ? AND expires_at_ms > ?
				   )"
			),
			params![credential.credential_type(), data, identity_key, id, expected_data, id, owner, now],
		)?;
		Ok(changed == 1)
	}

	/// Fence a dead grant: disable only if the row still holds `expected_data`
	/// and this caller still owns an unexpired refresh lease on it.
	pub fn disable_if_matches(
		&self,
		id: i64,
		expected_data: &str,
		cause: &str,
		owner: &str,
	) -> Result<bool, AuthError> {
		let now = now_ms();
		let changed = self.connection.execute(
			&format!(
				"UPDATE auth_credentials SET disabled_cause = ?, updated_at = {SQLITE_NOW_EPOCH}
				 WHERE id = ? AND data = ? AND disabled_cause IS NULL
				   AND EXISTS (
				     SELECT 1 FROM auth_credential_refresh_leases
				     WHERE credential_id = ? AND owner = ? AND expires_at_ms > ?
				   )"
			),
			params![cause, id, expected_data, id, owner, now],
		)?;
		Ok(changed == 1)
	}

	/// Stored `data` for an active row; `None` when the row is gone or disabled.
	pub fn data_if_active(&self, id: i64) -> Result<Option<String>, AuthError> {
		self.connection
			.query_row(
				"SELECT data FROM auth_credentials WHERE id = ? AND disabled_cause IS NULL",
				params![id],
				|row| row.get::<_, String>(0),
			)
			.optional()
			.map_err(AuthError::from)
	}

	/// Delete every active credential of a provider (`logout`).
	pub fn delete_provider(&self, provider: &str) -> Result<usize, AuthError> {
		let removed = self
			.connection
			.execute("DELETE FROM auth_credentials WHERE provider = ?", params![provider])?;
		self.connection.execute(
			"DELETE FROM auth_credential_refresh_leases WHERE credential_id NOT IN (SELECT id FROM auth_credentials)",
			[],
		)?;
		Ok(removed)
	}

	/// `upsertCredentialBlock`: extend (never shorten) the usage-limit block
	/// window for `(credential, providerKey, scope)`.
	pub fn upsert_block(
		&self,
		credential_id: i64,
		provider_key: &str,
		block_scope: &str,
		blocked_until_ms: i64,
	) -> Result<(), AuthError> {
		self.connection.execute(
			&format!(
				"INSERT INTO auth_credential_blocks (credential_id, provider_key, block_scope, blocked_until_ms, updated_at)
				 VALUES (?, ?, ?, ?, {SQLITE_NOW_EPOCH})
				 ON CONFLICT(credential_id, provider_key, block_scope) DO UPDATE SET
				   blocked_until_ms = MAX(auth_credential_blocks.blocked_until_ms, excluded.blocked_until_ms),
				   updated_at = excluded.updated_at"
			),
			params![credential_id, provider_key, block_scope, blocked_until_ms],
		)?;
		Ok(())
	}

	/// `getCredentialBlock`: unexpired block window, or `None` after pruning.
	pub fn get_block(&self, credential_id: i64, provider_key: &str, block_scope: &str) -> Result<Option<i64>, AuthError> {
		let blocked_until = self
			.connection
			.query_row(
				"SELECT blocked_until_ms FROM auth_credential_blocks
				 WHERE credential_id = ? AND provider_key = ? AND block_scope = ?",
				params![credential_id, provider_key, block_scope],
				|row| row.get::<_, i64>(0),
			)
			.optional()?;
		match blocked_until {
			Some(until) if until <= now_ms() => {
				self.prune_blocks(now_ms())?;
				Ok(None)
			},
			other => Ok(other),
		}
	}

	/// Delete expired block rows; returns how many were removed.
	pub fn prune_blocks(&self, now: i64) -> Result<usize, AuthError> {
		Ok(self
			.connection
			.execute("DELETE FROM auth_credential_blocks WHERE blocked_until_ms <= ?", params![now])?)
	}

	/// Take the refresh lease for `credential_id`, waiting for a peer's lease to
	/// expire.
	///
	/// `cancel` aborts the wait (`Cancelled`) so a CLI/RPC cancel never hangs
	/// behind a peer that keeps renewing; a row that vanished or was disabled
	/// while waiting is refused instead of refreshed.
	pub fn acquire_lease_blocking(
		&self,
		credential_id: i64,
		cancel: &Cancel,
	) -> Result<RefreshLease<'_>, AuthError> {
		let owner = random_owner_id()?;
		let mut waited = 0u64;
		loop {
			cancel.check("auth")?;
			if self.data_if_active(credential_id)?.is_none() {
				return Err(AuthError::new(AuthErrorKind::Storage, "credential is no longer active")
					.for_provider("auth"));
			}
			let now = now_ms();
			if self.try_acquire_lease(credential_id, &owner, now)? == 1 {
				return Ok(RefreshLease { store: self, credential_id, owner });
			}
			let sleep_ms = waited.clamp(LEASE_POLL_MIN_MS, LEASE_POLL_MAX_MS);
			std::thread::sleep(Duration::from_millis(sleep_ms));
			waited = waited.saturating_add(LEASE_POLL_MIN_MS);
		}
	}

	/// Take the refresh lease for `credential_id`, waiting for a peer's lease to
	/// expire. Ownership ends either by `release` or by lease expiry.
	pub async fn acquire_lease(&self, credential_id: i64) -> Result<RefreshLease<'_>, AuthError> {
		let owner = random_owner_id()?;
		let mut waited = 0u64;
		loop {
			let now = now_ms();
			if self.try_acquire_lease(credential_id, &owner, now)? == 1 {
				return Ok(RefreshLease { store: self, credential_id, owner });
			}
			let sleep_ms = waited.clamp(LEASE_POLL_MIN_MS, LEASE_POLL_MAX_MS);
			tokio::time::sleep(Duration::from_millis(sleep_ms)).await;
			waited = waited.saturating_add(LEASE_POLL_MIN_MS);
		}
	}

	fn try_acquire_lease(&self, credential_id: i64, owner: &str, now: i64) -> Result<usize, AuthError> {
		let expires = now + REFRESH_LEASE_TTL_MS;
		Ok(self.connection.execute(
			&format!(
				"INSERT INTO auth_credential_refresh_leases (credential_id, owner, expires_at_ms, updated_at)
				 VALUES (?, ?, ?, {SQLITE_NOW_EPOCH})
				 ON CONFLICT(credential_id) DO UPDATE SET owner = excluded.owner,
				   expires_at_ms = excluded.expires_at_ms, updated_at = excluded.updated_at
				 WHERE auth_credential_refresh_leases.expires_at_ms <= ?"
			),
			params![credential_id, owner, expires, now],
		)?)
	}

	fn renew_lease(&self, credential_id: i64, owner: &str) -> Result<bool, AuthError> {
		let now = now_ms();
		let changed = self.connection.execute(
			&format!(
				"UPDATE auth_credential_refresh_leases SET expires_at_ms = ?, updated_at = {SQLITE_NOW_EPOCH}
				 WHERE credential_id = ? AND owner = ? AND expires_at_ms > ?"
			),
			params![now + REFRESH_LEASE_TTL_MS, credential_id, owner, now],
		)?;
		Ok(changed == 1)
	}

	fn release_lease(&self, credential_id: i64, owner: &str) -> Result<(), AuthError> {
		self.connection.execute(
			"DELETE FROM auth_credential_refresh_leases WHERE credential_id = ? AND owner = ?",
			params![credential_id, owner],
		)?;
		Ok(())
	}
}

/// Owned refresh lease; dropping it releases the row so a peer can refresh even
/// when the attempt failed.
pub struct RefreshLease<'store> {
	store: &'store CredentialStore,
	credential_id: i64,
	owner: String,
}

impl RefreshLease<'_> {
	#[must_use]
	pub fn credential_id(&self) -> i64 {
		self.credential_id
	}

	/// Extend the lease; `false` means ownership was taken away (or already
	/// expired) and the caller must not persist its result.
	pub fn renew(&self) -> Result<bool, AuthError> {
		self.store.renew_lease(self.credential_id, &self.owner)
	}

	/// Lease owner id, the fence every CAS write must present.
	#[must_use]
	pub fn owner(&self) -> &str {
		&self.owner
	}
}

impl Drop for RefreshLease<'_> {
	fn drop(&mut self) {
		let _ = self.store.release_lease(self.credential_id, &self.owner);
	}
}

/// Full v7 credentials table (`sqlite-credential-store.ts:735-747`).
const AUTH_CREDENTIALS_DDL: &str = concat!(
	"CREATE TABLE IF NOT EXISTS auth_credentials (",
	" id INTEGER PRIMARY KEY AUTOINCREMENT,",
	" provider TEXT NOT NULL,",
	" credential_type TEXT NOT NULL,",
	" data TEXT NOT NULL,",
	" disabled_cause TEXT DEFAULT NULL,",
	" identity_key TEXT DEFAULT NULL,",
	" created_at INTEGER NOT NULL DEFAULT (CAST(strftime('%s','now') AS INTEGER)),",
	" updated_at INTEGER NOT NULL DEFAULT (CAST(strftime('%s','now') AS INTEGER))",
	");"
);

const AUTH_CREDENTIAL_INDEXES_DDL: &str = "CREATE INDEX IF NOT EXISTS idx_auth_provider ON auth_credentials(provider);
	CREATE INDEX IF NOT EXISTS idx_auth_provider_identity ON auth_credentials(provider, identity_key) WHERE identity_key IS NOT NULL;";

/// `#createAuthCredentialBlocksTable`.
const AUTH_BLOCKS_DDL: &str = "CREATE TABLE IF NOT EXISTS auth_credential_blocks (
		credential_id INTEGER NOT NULL,
		provider_key TEXT NOT NULL,
		block_scope TEXT NOT NULL DEFAULT '',
		blocked_until_ms INTEGER NOT NULL,
		updated_at INTEGER NOT NULL,
		PRIMARY KEY (credential_id, provider_key, block_scope)
	);
	CREATE INDEX IF NOT EXISTS idx_auth_credential_blocks_expires ON auth_credential_blocks(blocked_until_ms);";

/// `#createAuthCredentialRefreshLeasesTable`.
const AUTH_LEASES_DDL: &str = "CREATE TABLE IF NOT EXISTS auth_credential_refresh_leases (
		credential_id INTEGER PRIMARY KEY,
		owner TEXT NOT NULL,
		expires_at_ms INTEGER NOT NULL,
		updated_at INTEGER NOT NULL
	);
	CREATE INDEX IF NOT EXISTS idx_auth_credential_refresh_leases_expires ON auth_credential_refresh_leases(expires_at_ms);";

/// `#createAuthCredentialBlockMirrorGuardTable`.
const AUTH_BLOCK_GUARD_DDL: &str = "CREATE TABLE IF NOT EXISTS auth_credential_block_mirror_guard (
		credential_id INTEGER PRIMARY KEY
	) WITHOUT ROWID;";

/// `#createAuthCredentialBlockCompatibilityTriggers`: a physical Codex `shared`
/// block row for pre-meter binaries, with a guard so the projection does not
/// feed back into the meter rows.
const AUTH_BLOCK_COMPAT_DDL: &str = "CREATE TRIGGER IF NOT EXISTS auth_codex_shared_insert_to_meters
	AFTER INSERT ON auth_credential_blocks
	WHEN NEW.provider_key = 'openai-codex:oauth' AND NEW.block_scope = 'shared'
		AND NOT EXISTS (SELECT 1 FROM auth_credential_block_mirror_guard WHERE credential_id = NEW.credential_id)
	BEGIN
		INSERT OR IGNORE INTO auth_credential_block_mirror_guard (credential_id) VALUES (NEW.credential_id);
		INSERT INTO auth_credential_blocks (credential_id, provider_key, block_scope, blocked_until_ms, updated_at)
		VALUES (NEW.credential_id, NEW.provider_key, 'chat', NEW.blocked_until_ms, NEW.updated_at)
		ON CONFLICT(credential_id, provider_key, block_scope) DO UPDATE SET
			blocked_until_ms = MAX(auth_credential_blocks.blocked_until_ms, excluded.blocked_until_ms),
			updated_at = MAX(auth_credential_blocks.updated_at, excluded.updated_at);
		INSERT INTO auth_credential_blocks (credential_id, provider_key, block_scope, blocked_until_ms, updated_at)
		VALUES (NEW.credential_id, NEW.provider_key, 'spark', NEW.blocked_until_ms, NEW.updated_at)
		ON CONFLICT(credential_id, provider_key, block_scope) DO UPDATE SET
			blocked_until_ms = MAX(auth_credential_blocks.blocked_until_ms, excluded.blocked_until_ms),
			updated_at = MAX(auth_credential_blocks.updated_at, excluded.updated_at);
		DELETE FROM auth_credential_block_mirror_guard WHERE credential_id = NEW.credential_id;
	END;
	CREATE TRIGGER IF NOT EXISTS auth_codex_shared_update_to_meters
	AFTER UPDATE ON auth_credential_blocks
	WHEN NEW.provider_key = 'openai-codex:oauth' AND NEW.block_scope = 'shared'
		AND NOT EXISTS (SELECT 1 FROM auth_credential_block_mirror_guard WHERE credential_id = NEW.credential_id)
	BEGIN
		INSERT OR IGNORE INTO auth_credential_block_mirror_guard (credential_id) VALUES (NEW.credential_id);
		INSERT INTO auth_credential_blocks (credential_id, provider_key, block_scope, blocked_until_ms, updated_at)
		VALUES (NEW.credential_id, NEW.provider_key, 'chat', NEW.blocked_until_ms, NEW.updated_at)
		ON CONFLICT(credential_id, provider_key, block_scope) DO UPDATE SET
			blocked_until_ms = MAX(auth_credential_blocks.blocked_until_ms, excluded.blocked_until_ms),
			updated_at = MAX(auth_credential_blocks.updated_at, excluded.updated_at);
		INSERT INTO auth_credential_blocks (credential_id, provider_key, block_scope, blocked_until_ms, updated_at)
		VALUES (NEW.credential_id, NEW.provider_key, 'spark', NEW.blocked_until_ms, NEW.updated_at)
		ON CONFLICT(credential_id, provider_key, block_scope) DO UPDATE SET
			blocked_until_ms = MAX(auth_credential_blocks.blocked_until_ms, excluded.blocked_until_ms),
			updated_at = MAX(auth_credential_blocks.updated_at, excluded.updated_at);
		DELETE FROM auth_credential_block_mirror_guard WHERE credential_id = NEW.credential_id;
	END;
	CREATE TRIGGER IF NOT EXISTS auth_codex_meter_insert_to_shared
	AFTER INSERT ON auth_credential_blocks
	WHEN NEW.provider_key = 'openai-codex:oauth' AND NEW.block_scope IN ('chat', 'spark')
		AND NOT EXISTS (SELECT 1 FROM auth_credential_block_mirror_guard WHERE credential_id = NEW.credential_id)
	BEGIN
		INSERT OR IGNORE INTO auth_credential_block_mirror_guard (credential_id) VALUES (NEW.credential_id);
		DELETE FROM auth_credential_blocks
		WHERE credential_id = NEW.credential_id AND provider_key = NEW.provider_key AND block_scope = 'shared';
		INSERT INTO auth_credential_blocks (credential_id, provider_key, block_scope, blocked_until_ms, updated_at)
		SELECT NEW.credential_id, NEW.provider_key, 'shared', MAX(blocked_until_ms), MAX(updated_at)
		FROM auth_credential_blocks
		WHERE credential_id = NEW.credential_id AND provider_key = NEW.provider_key
			AND block_scope IN ('chat', 'spark')
		GROUP BY credential_id, provider_key;
		DELETE FROM auth_credential_block_mirror_guard WHERE credential_id = NEW.credential_id;
	END;
	CREATE TRIGGER IF NOT EXISTS auth_codex_meter_update_to_shared
	AFTER UPDATE ON auth_credential_blocks
	WHEN NEW.provider_key = 'openai-codex:oauth' AND NEW.block_scope IN ('chat', 'spark')
		AND NOT EXISTS (SELECT 1 FROM auth_credential_block_mirror_guard WHERE credential_id = NEW.credential_id)
	BEGIN
		INSERT OR IGNORE INTO auth_credential_block_mirror_guard (credential_id) VALUES (NEW.credential_id);
		DELETE FROM auth_credential_blocks
		WHERE credential_id = NEW.credential_id AND provider_key = NEW.provider_key AND block_scope = 'shared';
		INSERT INTO auth_credential_blocks (credential_id, provider_key, block_scope, blocked_until_ms, updated_at)
		SELECT NEW.credential_id, NEW.provider_key, 'shared', MAX(blocked_until_ms), MAX(updated_at)
		FROM auth_credential_blocks
		WHERE credential_id = NEW.credential_id AND provider_key = NEW.provider_key
			AND block_scope IN ('chat', 'spark')
		GROUP BY credential_id, provider_key;
		DELETE FROM auth_credential_block_mirror_guard WHERE credential_id = NEW.credential_id;
	END;
	CREATE TRIGGER IF NOT EXISTS auth_codex_shared_delete_to_meters
	AFTER DELETE ON auth_credential_blocks
	WHEN OLD.provider_key = 'openai-codex:oauth' AND OLD.block_scope = 'shared'
		AND NOT EXISTS (SELECT 1 FROM auth_credential_block_mirror_guard WHERE credential_id = OLD.credential_id)
	BEGIN
		INSERT OR IGNORE INTO auth_credential_block_mirror_guard (credential_id) VALUES (OLD.credential_id);
		DELETE FROM auth_credential_blocks
		WHERE credential_id = OLD.credential_id AND provider_key = OLD.provider_key
			AND block_scope IN ('chat', 'spark');
		DELETE FROM auth_credential_block_mirror_guard WHERE credential_id = OLD.credential_id;
	END;
	CREATE TRIGGER IF NOT EXISTS auth_codex_meter_delete_to_shared
	AFTER DELETE ON auth_credential_blocks
	WHEN OLD.provider_key = 'openai-codex:oauth' AND OLD.block_scope IN ('chat', 'spark')
		AND NOT EXISTS (SELECT 1 FROM auth_credential_block_mirror_guard WHERE credential_id = OLD.credential_id)
	BEGIN
		INSERT OR IGNORE INTO auth_credential_block_mirror_guard (credential_id) VALUES (OLD.credential_id);
		DELETE FROM auth_credential_blocks
		WHERE credential_id = OLD.credential_id AND provider_key = OLD.provider_key AND block_scope = 'shared';
		INSERT INTO auth_credential_blocks (credential_id, provider_key, block_scope, blocked_until_ms, updated_at)
		SELECT OLD.credential_id, OLD.provider_key, 'shared', MAX(blocked_until_ms), MAX(updated_at)
		FROM auth_credential_blocks
		WHERE credential_id = OLD.credential_id AND provider_key = OLD.provider_key
			AND block_scope IN ('chat', 'spark')
		GROUP BY credential_id, provider_key;
		DELETE FROM auth_credential_block_mirror_guard WHERE credential_id = OLD.credential_id;
	END;";

fn table_exists(connection: &Connection, table: &str) -> Result<bool, AuthError> {
	Ok(connection
		.query_row(
			"SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?",
			params![table],
			|_| Ok(()),
		)
		.optional()?
		.is_some())
}

fn table_columns(connection: &Connection, table: &str) -> Result<Vec<String>, AuthError> {
	let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
	let columns = statement.query_map([], |row| row.get::<_, String>(1))?;
	let mut out = Vec::new();
	for column in columns {
		out.push(column?);
	}
	Ok(out)
}

fn read_schema_version(connection: &Connection) -> Result<Option<i64>, AuthError> {
	connection
		.query_row("SELECT version FROM auth_schema_version WHERE id = 1", [], |row| row.get(0))
		.optional()
		.map_err(AuthError::from)
}

fn write_schema_version(connection: &Connection, version: i64) -> Result<(), AuthError> {
	connection.execute(
		"INSERT OR REPLACE INTO auth_schema_version (id, version) VALUES (1, ?)",
		params![version],
	)?;
	Ok(())
}

/// `#inferAuthSchemaVersionFromColumns`: no version row → decide from shape.
fn infer_schema_version(columns: &[String]) -> i64 {
	let has = |name: &str| columns.iter().any(|column| column == name);
	if has("identity_key") {
		return 3;
	}
	if has("account_id") || has("email") {
		return 2;
	}
	if has("disabled_cause") {
		return 1;
	}
	0
}

/// `#migrateAuthSchema`: the exact ordered TS migration chain, run inside the
/// caller's transaction. The version row advances only after these succeed.
fn migrate_auth_schema(connection: &Connection, from_version: i64, columns: &[String]) -> Result<(), AuthError> {
	if from_version < 1 {
		migrate_v0_to_v1(connection, columns)?;
	}
	if from_version < 3 {
		rebuild_credentials(connection, "auth_credentials_legacy", false)?;
	}
	if from_version < 4 {
		rebuild_credentials(connection, "auth_credentials_v3", true)?;
	}
	if from_version < 5 {
		connection.execute_batch(AUTH_BLOCKS_DDL)?;
	}
	if from_version < 6 {
		connection.execute_batch(AUTH_LEASES_DDL)?;
	}
	if from_version < 7 {
		connection.execute_batch(AUTH_BLOCKS_DDL)?;
		connection.execute_batch(AUTH_BLOCK_GUARD_DDL)?;
		migrate_v6_to_v7(connection)?;
		connection.execute_batch(AUTH_BLOCK_COMPAT_DDL)?;
	}
	Ok(())
}

/// `#migrateAuthSchemaV0ToV1`: v0 had `disabled INTEGER`; map it to a cause.
fn migrate_v0_to_v1(connection: &Connection, columns: &[String]) -> Result<(), AuthError> {
	let has_disabled = columns.iter().any(|column| column == "disabled");
	connection.execute_batch("ALTER TABLE auth_credentials RENAME TO auth_credentials_v0;")?;
	connection.execute_batch(
		"CREATE TABLE auth_credentials (
			id INTEGER PRIMARY KEY AUTOINCREMENT,
			provider TEXT NOT NULL,
			credential_type TEXT NOT NULL,
			data TEXT NOT NULL,
			disabled_cause TEXT DEFAULT NULL,
			created_at INTEGER NOT NULL DEFAULT (CAST(strftime('%s','now') AS INTEGER)),
			updated_at INTEGER NOT NULL DEFAULT (CAST(strftime('%s','now') AS INTEGER))
		);",
	)?;
	let disabled_projection = if has_disabled {
		"CASE WHEN disabled = 1 THEN 'disabled' ELSE NULL END"
	} else {
		"NULL"
	};
	connection.execute_batch(&format!(
		"INSERT INTO auth_credentials (id, provider, credential_type, data, disabled_cause, created_at, updated_at)
		 SELECT id, provider, credential_type, data, {disabled_projection}, created_at, updated_at
		 FROM auth_credentials_v0;
		 DROP TABLE auth_credentials_v0;"
	))?;
	Ok(())
}

/// `#migrateAuthSchemaV1OrV2ToV3` / `#migrateAuthSchemaV3ToV4`: rebuild the
/// table with the v7 column set, preserving ids, disabled causes and (v3→v4)
/// identity keys.
fn rebuild_credentials(connection: &Connection, temporary: &str, keep_identity: bool) -> Result<(), AuthError> {
	connection.execute_batch(&format!("ALTER TABLE auth_credentials RENAME TO {temporary};"))?;
	connection.execute_batch(AUTH_CREDENTIALS_DDL)?;
	let identity = if keep_identity { "identity_key" } else { "NULL" };
	connection.execute_batch(&format!(
		"INSERT INTO auth_credentials (id, provider, credential_type, data, disabled_cause, identity_key, created_at, updated_at)
		 SELECT id, provider, credential_type, data, disabled_cause, {identity}, created_at, updated_at
		 FROM {temporary};
		 DROP TABLE {temporary};"
	))?;
	Ok(())
}

/// `#migrateAuthSchemaV6ToV7`: split the legacy Codex `shared` block into the
/// per-meter rows current code reads, keeping the maximum window.
fn migrate_v6_to_v7(connection: &Connection) -> Result<(), AuthError> {
	connection.execute_batch(
		"DELETE FROM auth_credential_block_mirror_guard;
		INSERT OR IGNORE INTO auth_credential_block_mirror_guard (credential_id)
		SELECT DISTINCT credential_id FROM auth_credential_blocks
		WHERE provider_key = 'openai-codex:oauth' AND block_scope IN ('chat', 'spark', 'shared');
		INSERT INTO auth_credential_blocks (credential_id, provider_key, block_scope, blocked_until_ms, updated_at)
		SELECT credential_id, provider_key, 'chat', blocked_until_ms, updated_at FROM auth_credential_blocks
		WHERE provider_key = 'openai-codex:oauth' AND block_scope = 'shared'
		ON CONFLICT(credential_id, provider_key, block_scope) DO UPDATE SET
			blocked_until_ms = MAX(auth_credential_blocks.blocked_until_ms, excluded.blocked_until_ms),
			updated_at = MAX(auth_credential_blocks.updated_at, excluded.updated_at);
		INSERT INTO auth_credential_blocks (credential_id, provider_key, block_scope, blocked_until_ms, updated_at)
		SELECT credential_id, provider_key, 'spark', blocked_until_ms, updated_at FROM auth_credential_blocks
		WHERE provider_key = 'openai-codex:oauth' AND block_scope = 'shared'
		ON CONFLICT(credential_id, provider_key, block_scope) DO UPDATE SET
			blocked_until_ms = MAX(auth_credential_blocks.blocked_until_ms, excluded.blocked_until_ms),
			updated_at = MAX(auth_credential_blocks.updated_at, excluded.updated_at);
		INSERT INTO auth_credential_blocks (credential_id, provider_key, block_scope, blocked_until_ms, updated_at)
		SELECT credential_id, provider_key, 'shared', MAX(blocked_until_ms), MAX(updated_at)
		FROM auth_credential_blocks
		WHERE provider_key = 'openai-codex:oauth' AND block_scope IN ('chat', 'spark')
		GROUP BY credential_id, provider_key
		ON CONFLICT(credential_id, provider_key, block_scope) DO UPDATE SET
			blocked_until_ms = excluded.blocked_until_ms,
			updated_at = excluded.updated_at;
		DELETE FROM auth_credential_block_mirror_guard;",
	)?;
	Ok(())
}

/// `#backfillCredentialIdentityKeys`: derive identity keys for rows that lack
/// one (including disabled rows); rows without a derivable identity stay NULL.
fn backfill_identity_keys(connection: &Connection) -> Result<(), AuthError> {
	let mut statement = connection.prepare(
		"SELECT id, provider, credential_type, data, identity_key FROM auth_credentials
		 WHERE identity_key IS NULL ORDER BY id ASC",
	)?;
	let rows = statement.query_map([], |row| {
		Ok(Row {
			id: row.get(0)?,
			provider: row.get(1)?,
			credential_type: row.get(2)?,
			data: row.get(3)?,
			identity_key: row.get(4)?,
		})
	})?;
	for row in rows {
		let row = row?;
		let Some(credential) = deserialize(&row) else { continue };
		let Some(key) = identity_key(&row.provider, &credential) else { continue };
		connection.execute(
			&format!("UPDATE auth_credentials SET identity_key = ?, updated_at = {SQLITE_NOW_EPOCH} WHERE id = ?"),
			params![key, row.id],
		)?;
	}
	Ok(())
}

fn stored_from_row(row: Row) -> Option<StoredCredential> {
	let credential = deserialize(&row)?;
	Some(StoredCredential {
		id: row.id,
		provider: row.provider,
		identity_key: row.identity_key,
		data: row.data,
		credential,
	})
}

fn open_with_retry(path: &Path) -> Result<Connection, AuthError> {
	let mut last_error = None;
	for attempt in 0..4u32 {
		match Connection::open(path) {
			Ok(connection) => {
				connection.busy_timeout(Duration::from_millis(5_000))?;
				return Ok(connection);
			},
			Err(err) => {
				last_error = Some(err);
				std::thread::sleep(Duration::from_millis(100 * 2u64.pow(attempt)));
			},
		}
	}
	Err(AuthError::new(
		AuthErrorKind::Configuration,
		format!(
			"Failed to open auth database at '{}' after 4 attempts: {}",
			path.display(),
			last_error.map(|err| err.to_string()).unwrap_or_default()
		),
	))
}

fn deserialize(row: &Row) -> Option<Credential> {
	let value: serde_json::Value = serde_json::from_str(&row.data).ok()?;
	if !value.is_object() {
		return None;
	}
	match row.credential_type.as_str() {
		"api_key" => {
			let key = value.get("key")?.as_str()?.to_string();
			let source_login = value.get("source").and_then(serde_json::Value::as_str) == Some("login");
			Some(Credential::ApiKey { key, source_login })
		},
		"oauth" => serde_json::from_value::<OAuthCredentials>(value).ok().map(Credential::OAuth),
		_ => None,
	}
}

fn stored_api_key(row: &Row) -> Option<String> {
	match deserialize(row)? {
		Credential::ApiKey { key, .. } => Some(key),
		Credential::OAuth(_) => None,
	}
}

/// `resolveCredentialIdentityKey`: `api_key` rows have no identity; OAuth rows
/// are grouped by account/email/project, org-qualified for the two providers
/// whose one email can hold several subscriptions.
#[must_use]
pub fn identity_key(provider: &str, credential: &Credential) -> Option<String> {
	let Credential::OAuth(creds) = credential else { return None };
	let account = trimmed(&creds.account_id).map(|id| format!("account:{id}"));
	let email = trimmed(&creds.email).map(|email| format!("email:{}", email.to_lowercase()));
	let project = trimmed(&creds.project_id).map(|id| format!("project:{id}"));
	let org = trimmed(&creds.org_id).map(|id| format!("org:{id}"));

	if matches!(provider, "anthropic" | "openai-codex") {
		let base = email.clone().or_else(|| account.clone()).or_else(|| project.clone());
		return match (base, org) {
			(Some(base), Some(org)) => Some(format!("{base}|{org}")),
			(Some(base), None) => Some(base),
			(None, org) => org,
		};
	}
	account.or(email).or(project)
}

fn trimmed(value: &Option<String>) -> Option<&str> {
	value.as_deref().map(str::trim).filter(|value| !value.is_empty())
}

fn now_ms() -> i64 {
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
		.unwrap_or(0)
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
	use std::os::unix::fs::PermissionsExt as _;
	let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) {}

#[cfg(test)]
mod tests {
	use super::*;

	fn oauth(access: &str, refresh: &str, account: &str) -> Credential {
		Credential::OAuth(OAuthCredentials {
			access: access.to_string(),
			refresh: refresh.to_string(),
			expires: now_ms() + 60_000,
			account_id: Some(account.to_string()),
			email: Some(format!("{account}@example.com")),
			..OAuthCredentials::default()
		})
	}

	#[test]
	fn stores_and_reads_back_oauth_row() {
		let store = CredentialStore::in_memory().expect("store");
		let id = store.upsert("openai-codex", &oauth("a1", "r1", "acct")).expect("upsert");
		let stored = store.get("openai-codex").expect("get").expect("row");
		assert_eq!(stored.id, id);
		assert_eq!(stored.identity_key.as_deref(), Some("email:acct@example.com"));
		match stored.credential {
			Credential::OAuth(creds) => {
				assert_eq!(creds.access, "a1");
				assert_eq!(creds.account_id.as_deref(), Some("acct"));
			},
			other => panic!("wrong credential: {other:?}"),
		}
	}

	#[test]
	fn api_key_source_login_is_encoded_like_ts() {
		let store = CredentialStore::in_memory().expect("store");
		store
			.upsert("deepseek", &Credential::ApiKey { key: "sk-1".to_string(), source_login: true })
			.expect("upsert");
		let stored = store.get("deepseek").expect("get").expect("row");
		assert_eq!(stored.data, r#"{"key":"sk-1","source":"login"}"#);
		assert!(stored.identity_key.is_none());
	}

	#[test]
	fn upsert_updates_same_identity_and_keeps_multi_account() {
		let store = CredentialStore::in_memory().expect("store");
		let first = store.upsert("openai-codex", &oauth("a1", "r1", "acct")).expect("insert");
		store.upsert("openai-codex", &oauth("a2", "r2", "acct")).expect("update");
		let rows = store.list_active("openai-codex").expect("list");
		assert_eq!(rows.len(), 1, "same identity is one row");
		assert_eq!(rows[0].id, first);

		store.upsert("openai-codex", &oauth("a3", "r3", "other")).expect("second account");
		assert_eq!(store.list_active("openai-codex").expect("list").len(), 2);
	}

	#[test]
	fn cas_update_rejects_stale_data() {
		let store = CredentialStore::in_memory().expect("store");
		let id = store.upsert("anthropic", &oauth("a1", "r1", "acct")).expect("insert");
		let row = store.get("anthropic").expect("get").expect("row");
		let lease = store.acquire_lease_blocking(id, &Cancel::new()).expect("lease");
		assert!(store
			.update_if_matches(id, &row.data, &oauth("a2", "r2", "acct"), lease.owner())
			.expect("cas"));
		assert!(!store
			.update_if_matches(id, &row.data, &oauth("a3", "r3", "acct"), lease.owner())
			.expect("stale cas"));
	}

	#[test]
	fn disable_requires_matching_data_and_hides_row() {
		let store = CredentialStore::in_memory().expect("store");
		let id = store.upsert("anthropic", &oauth("a1", "r1", "acct")).expect("insert");
		let row = store.get("anthropic").expect("get").expect("row");
		let lease = store.acquire_lease_blocking(id, &Cancel::new()).expect("lease");
		assert!(!store
			.disable_if_matches(id, "{\"other\":true}", "invalid_grant", lease.owner())
			.expect("mismatch"));
		assert!(store
			.disable_if_matches(id, &row.data, "invalid_grant", lease.owner())
			.expect("disable"));
		assert!(store.get("anthropic").expect("get").is_none());
		assert_eq!(store.delete_provider("anthropic").expect("delete"), 1);
	}

	#[test]
	fn identity_keys_match_ts_shapes() {
		let mut creds = OAuthCredentials {
			account_id: Some("acct".into()),
			org_id: Some("org".into()),
			..OAuthCredentials::default()
		};
		assert_eq!(identity_key("openai-codex", &Credential::OAuth(creds.clone())).as_deref(), Some("account:acct|org:org"));
		assert_eq!(identity_key("zai", &Credential::OAuth(creds.clone())).as_deref(), Some("account:acct"));
		creds.account_id = None;
		creds.email = Some(" User@Example.com ".into());
		assert_eq!(identity_key("anthropic", &Credential::OAuth(creds)).as_deref(), Some("email:user@example.com|org:org"));
	}

	#[test]
	fn preserves_unrelated_tables() {
		let store = CredentialStore::in_memory().expect("store");
		store
			.connection
			.execute_batch("CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL); INSERT INTO settings VALUES ('a','b');")
			.expect("seed");
		store.upsert("zai", &oauth("a", "r", "acct")).expect("upsert");
		let value: String = store
			.connection
			.query_row("SELECT value FROM settings WHERE key = 'a'", [], |row| row.get(0))
			.expect("settings row survived");
		assert_eq!(value, "b");
	}

	#[tokio::test]
	async fn lease_is_exclusive_and_expires() {
		let store = CredentialStore::in_memory().expect("store");
		let id = store.upsert("zai", &oauth("a", "r", "acct")).expect("upsert");
		let lease = store.acquire_lease(id).await.expect("lease");
		assert!(lease.renew().expect("renew"));
		drop(lease);
		let second = store.acquire_lease(id).await.expect("second lease");
		assert_eq!(second.credential_id(), id);
	}

	#[test]
	fn oauth_json_matches_ts_camel_case_and_reads_legacy_aliases() {
		let creds = OAuthCredentials {
			access: "a".into(),
			refresh: "r".into(),
			expires: 1,
			enterprise_url: Some("ghe.example.com".into()),
			project_id: Some("proj".into()),
			api_endpoint: Some("https://capi.example".into()),
			org_id: Some("org".into()),
			authorized_at: Some(42),
			..OAuthCredentials::default()
		};
		let json = serde_json::to_value(&creds).expect("json");
		assert_eq!(json["enterpriseUrl"], "ghe.example.com");
		assert_eq!(json["projectId"], "proj");
		assert_eq!(json["apiEndpoint"], "https://capi.example");
		assert_eq!(json["orgId"], "org");
		assert_eq!(json["authorizedAt"], 42);
		assert!(json.get("enterprise_url").is_none());

		let legacy: OAuthCredentials = serde_json::from_str(
			r#"{"access":"a","refresh":"r","expires":1,"enterprise_url":"ghe.example.com","project_id":"proj"}"#,
		)
		.expect("legacy row");
		assert_eq!(legacy.enterprise_url.as_deref(), Some("ghe.example.com"));
		assert_eq!(legacy.project_id.as_deref(), Some("proj"));
	}

	#[test]
	fn refresh_inherits_missing_metadata() {
		let previous = OAuthCredentials {
			project_id: Some("proj".into()),
			authorized_at: Some(7),
			refresh: "r".into(),
			..OAuthCredentials::default()
		};
		let mut refreshed = OAuthCredentials { access: "new".into(), expires: 10, ..OAuthCredentials::default() };
		refreshed.inherit_metadata(&previous);
		assert_eq!(refreshed.project_id.as_deref(), Some("proj"));
		assert_eq!(refreshed.authorized_at, Some(7));
		assert_eq!(refreshed.refresh, "r");
		assert_eq!(refreshed.access, "new");
	}

	#[test]
	fn blocks_persist_extend_and_prune() {
		let store = CredentialStore::in_memory().expect("store");
		let id = store.upsert("zai", &oauth("a", "r", "acct")).expect("upsert");
		store.upsert_block(id, "zai:oauth", "", now_ms() + 60_000).expect("block");
		assert!(store.get_block(id, "zai:oauth", "").expect("read").is_some());
		// A shorter window never shortens the existing block.
		store.upsert_block(id, "zai:oauth", "", now_ms() - 1).expect("shorter");
		assert!(store.get_block(id, "zai:oauth", "").expect("read").is_some());
		assert_eq!(store.prune_blocks(now_ms() + 120_000).expect("prune"), 1);
		assert!(store.get_block(id, "zai:oauth", "").expect("gone").is_none());
	}

	#[test]
	fn lease_wait_honours_cancel_and_refuses_inactive_rows() {
		let store = CredentialStore::in_memory().expect("store");
		let id = store.upsert("zai", &oauth("a", "r", "acct")).expect("upsert");

		let cancelled = Cancel::new();
		cancelled.cancel();
		assert_eq!(
			store.acquire_lease_blocking(id, &cancelled).err().expect("cancelled").kind,
			AuthErrorKind::Cancelled
		);

		assert_eq!(
			store.acquire_lease_blocking(id + 1_000, &Cancel::new()).err().expect("inactive").kind,
			AuthErrorKind::Storage
		);
	}

	#[test]
	fn cas_requires_unexpired_lease_ownership() {
		let store = CredentialStore::in_memory().expect("store");
		let id = store.upsert("zai", &oauth("a", "r", "acct")).expect("upsert");
		let expected = store.data_if_active(id).expect("data").expect("active");
		let refreshed = oauth("b", "r", "acct");

		// No lease: the write is refused even though the row is unchanged.
		assert!(!store.update_if_matches(id, &expected, &refreshed, "not-the-owner").expect("cas"));
		assert!(store.data_if_active(id).expect("data").is_some());

		let lease = store.acquire_lease_blocking(id, &Cancel::new()).expect("lease");
		assert!(store.update_if_matches(id, &expected, &refreshed, lease.owner()).expect("cas"));
		assert!(!store.update_if_matches(id, &expected, &refreshed, lease.owner()).expect("stale expected"));
	}
}
