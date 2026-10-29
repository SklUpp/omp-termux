//! Credential selection policy (`auth-storage.ts#selectCredentialByType`,
//! `#getCredentialOrder`, `#isCredentialBlocked`, `#markCredentialBlocked`).
//!
//! One provider can hold several grants (two Claude accounts, a login key plus
//! a broker-migrated copy). Selection is deterministic:
//!
//! 1. filter the provider's active rows to one credential type (+ caller filter);
//! 2. order them — a session id maps to a sticky start index via
//!    `xxHash32(sessionId) % total`, no session means round-robin per
//!    `provider:type`;
//! 3. walk the wrapped order skipping rows blocked by a usage limit (in-memory
//!    window plus the persisted `auth_credential_blocks` row), falling back to
//!    the first ordering position when every row is blocked.
//!
//! Usage-ranked strategies (`rankingStrategyResolver` upstream) are a hook:
//! [`CredentialRanking`] is consulted with the ordered candidates exactly where
//! the TS code calls `#selectApiKeyCredential`'s strategy branch, so the
//! runtime can plug its usage reports in without this module probing providers.

use std::collections::HashMap;
use std::sync::Mutex;

use super::error::AuthError;
use super::store::{Credential, CredentialStore, StoredCredential};

/// `AuthCredential["type"]` values used as ordering keys.
pub const OAUTH: &str = "oauth";
pub const API_KEY: &str = "api_key";

/// `#getProviderTypeKey`: `"anthropic:oauth"`.
#[must_use]
pub fn provider_type_key(provider: &str, credential_type: &str) -> String {
	format!("{provider}:{credential_type}")
}

/// Optional usage-ranked selection (`rankingStrategyResolver`).
///
/// Upstream resolves a `CredentialRankingStrategy` per provider, probes usage
/// limits and orders candidates by remaining capacity. That probing lives in
/// the usage/transport layer, so the native module asks the hook for the winning
/// candidate index and falls back to the block-aware order when it declines.
pub trait CredentialRanking: Send + Sync {
	/// Return the index (into `candidates`) to use, or `None` to keep the
	/// block-aware order.
	fn select(&self, provider: &str, candidates: &[StoredCredential], order: &[usize]) -> Option<usize>;
}

/// In-memory half of the selection state: round-robin counters and unscoped
/// backoff windows (`#providerRoundRobinIndex`, `#credentialBackoff`).
#[derive(Default)]
pub struct SelectionState {
	round_robin: Mutex<HashMap<String, i64>>,
	backoff: Mutex<HashMap<String, HashMap<i64, i64>>>,
}

impl SelectionState {
	/// `#getCredentialOrder`: session hash or round-robin start, wrapped.
	#[must_use]
	pub fn order(&self, provider_key: &str, session_id: Option<&str>, total: usize) -> Vec<usize> {
		if total <= 1 {
			return vec![0];
		}
		let start = match session_id {
			Some(session_id) => usize::try_from(xxhash_rust::xxh32::xxh32(session_id.as_bytes(), 0)).unwrap_or(0) % total,
			None => {
				let mut counters = self.round_robin.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
				let next = (counters.get(provider_key).copied().unwrap_or(-1) + 1) % total as i64;
				counters.insert(provider_key.to_string(), next);
				usize::try_from(next).unwrap_or(0)
			},
		};
		(0..total).map(|step| (start + step) % total).collect()
	}

	/// `#getCredentialBlockedUntil`: in-memory window and persisted row agree on
	/// the longest expiry. Scopes honour both their own backoff key and the
	/// unscoped one (a legacy catch-all block applies to every scope).
	fn blocked_until(
		&self,
		store: &CredentialStore,
		provider_key: &str,
		credential_id: i64,
		scopes: &[&str],
		now_ms: i64,
	) -> Result<Option<i64>, AuthError> {
		let mut blocked: Option<i64> = self.memory_blocked_until(provider_key, credential_id, now_ms);
		for scope in scopes {
			let scoped = self.memory_blocked_until(&scoped_key(provider_key, scope), credential_id, now_ms);
			blocked = max_option(blocked, scoped);
		}
		blocked = max_option(blocked, store.get_block(credential_id, provider_key, "")?);
		for scope in scopes {
			blocked = max_option(blocked, store.get_block(credential_id, provider_key, scope)?);
		}
		Ok(blocked)
	}

	fn memory_blocked_until(&self, backoff_key: &str, credential_id: i64, now_ms: i64) -> Option<i64> {
		let mut windows = self.backoff.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
		let Some(entries) = windows.get_mut(backoff_key) else { return None };
		let Some(until) = entries.get(&credential_id).copied() else { return None };
		if until <= now_ms {
			entries.remove(&credential_id);
			if entries.is_empty() {
				windows.remove(backoff_key);
			}
			return None;
		}
		Some(until)
	}

	/// `#isCredentialBlocked`.
	pub fn is_blocked(
		&self,
		store: &CredentialStore,
		provider_key: &str,
		credential_id: i64,
		scopes: &[&str],
		now_ms: i64,
	) -> Result<bool, AuthError> {
		Ok(self.blocked_until(store, provider_key, credential_id, scopes, now_ms)?.is_some())
	}

	/// `#markCredentialBlocked`: extend (never shorten) the window in memory and
	/// in `auth_credential_blocks`.
	pub fn mark_blocked(
		&self,
		store: &CredentialStore,
		provider_key: &str,
		credential_id: i64,
		blocked_until_ms: i64,
		block_scope: Option<&str>,
	) -> Result<(), AuthError> {
		let backoff_key = scoped_key(provider_key, block_scope.unwrap_or_default());
		{
			let mut windows = self.backoff.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
			let entries = windows.entry(backoff_key).or_default();
			let next = entries.get(&credential_id).copied().unwrap_or(0).max(blocked_until_ms);
			entries.insert(credential_id, next);
		}
		store.upsert_block(credential_id, provider_key, block_scope.unwrap_or_default(), blocked_until_ms)
	}
}

fn scoped_key(provider_key: &str, block_scope: &str) -> String {
	if block_scope.is_empty() {
		provider_key.to_string()
	} else {
		format!("{provider_key}\0{block_scope}")
	}
}

fn max_option(left: Option<i64>, right: Option<i64>) -> Option<i64> {
	match (left, right) {
		(Some(left), Some(right)) => Some(left.max(right)),
		(Some(value), None) | (None, Some(value)) => Some(value),
		(None, None) => None,
	}
}

/// `#selectCredentialByType` + the `#selectApiKeyCredential` strategy branch.
pub fn select(
	store: &CredentialStore,
	state: &SelectionState,
	provider: &str,
	credential_type: &str,
	session_id: Option<&str>,
	block_scopes: &[&str],
	filter: impl Fn(&StoredCredential) -> bool,
	ranking: Option<&dyn CredentialRanking>,
) -> Result<Option<StoredCredential>, AuthError> {
	let candidates: Vec<StoredCredential> = store
		.list_active(provider)?
		.into_iter()
		.filter(|row| row.credential.credential_type() == credential_type && filter(row))
		.collect();
	match candidates.len() {
		0 => return Ok(None),
		1 => return Ok(candidates.into_iter().next()),
		_ => {},
	}

	let provider_key = provider_type_key(provider, credential_type);
	let order = state.order(&provider_key, session_id, candidates.len());
	let fallback = order[0];

	if let Some(ranking) = ranking {
		if let Some(index) = ranking.select(provider, &candidates, &order) {
			if let Some(row) = candidates.get(index) {
				return Ok(Some(row.clone()));
			}
		}
	}

	let now = now_ms();
	for index in &order {
		let Some(candidate) = candidates.get(*index) else { continue };
		if !state.is_blocked(store, &provider_key, candidate.id, block_scopes, now)? {
			return Ok(Some(candidate.clone()));
		}
	}
	Ok(candidates.get(fallback).cloned())
}

/// Credential type of a stored row, for the callers that need the match.
#[must_use]
pub fn is_api_key_login(row: &StoredCredential) -> bool {
	matches!(row.credential, Credential::ApiKey { source_login: true, .. })
}

fn now_ms() -> i64 {
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
		.unwrap_or(0)
}

#[cfg(test)]
mod tests {
	use super::super::store::{Credential, CredentialStore, StoredCredential};
	use super::*;

	fn key(value: &str, source_login: bool) -> Credential {
		Credential::ApiKey { key: value.to_string(), source_login }
	}

	#[test]
	fn session_string_free_round_robin_wraps() {
		let state = SelectionState::default();
		let key = provider_type_key("anthropic", API_KEY);
		assert_eq!(state.order(&key, None, 3), vec![0, 1, 2]);
		assert_eq!(state.order(&key, None, 3), vec![1, 2, 0]);
		assert_eq!(state.order(&key, None, 3), vec![2, 0, 1]);
		assert_eq!(state.order(&key, None, 3), vec![0, 1, 2]);
	}

	#[test]
	fn session_order_is_sticky_and_wraps() {
		let state = SelectionState::default();
		let key = provider_type_key("anthropic", API_KEY);
		let first = state.order(&key, Some("session-a"), 4);
		assert_eq!(first, state.order(&key, Some("session-a"), 4));
		assert_eq!(first.len(), 4);
		let start = xxhash_rust::xxh32::xxh32(b"session-a", 0) as usize % 4;
		assert_eq!(first[0], start);
		assert_eq!(first.last().map(|last| (start + 3) % 4), Some(*first.last().unwrap_or(&0)));
	}

	#[test]
	fn single_and_empty_candidates_short_circuit() {
		let store = CredentialStore::in_memory().expect("store");
		let state = SelectionState::default();
		assert!(select(&store, &state, "anthropic", API_KEY, None, &[], |_| true, None).expect("none").is_none());

		store.upsert("anthropic", &key("k1", true)).expect("upsert");
		let chosen = select(&store, &state, "anthropic", API_KEY, None, &[], |_| true, None)
			.expect("one")
			.expect("row");
		assert_eq!(chosen.credential.api_key_value(), Some("k1"));
	}

	#[test]
	fn blocked_rows_are_skipped_then_fallback_is_used() {
		let store = CredentialStore::in_memory().expect("store");
		let state = SelectionState::default();
		let key_name = provider_type_key("anthropic", API_KEY);
		let first = store.upsert("anthropic", &key("k1", true)).expect("upsert");
		let second = store.upsert("anthropic", &key("k2", true)).expect("upsert");

		// The first selection walks the order `[0, 1]` and takes row 0.
		assert_eq!(state.order(&key_name, None, 2)[0], 0);
		let rows = store.list_active("anthropic").expect("rows");
		assert_eq!(rows[0].id, first);

		// Blocking the head makes the next ordering position win.
		state
			.mark_blocked(&store, &key_name, first, now_ms() + 60_000, None)
			.expect("mark");
		let chosen = select(&store, &state, "anthropic", API_KEY, None, &[], |_| true, None)
			.expect("rows")
			.expect("row");
		assert_eq!(chosen.id, second);

		// Blocking everything still yields the fallback (first ordering position).
		state
			.mark_blocked(&store, &key_name, second, now_ms() + 60_000, None)
			.expect("mark");
		let fallback = select(&store, &state, "anthropic", API_KEY, None, &[], |_| true, None)
			.expect("rows")
			.expect("row");
		assert_eq!(fallback.id, first);
	}

	#[test]
	fn expired_blocks_do_not_block() {
		let store = CredentialStore::in_memory().expect("store");
		let state = SelectionState::default();
		let key_name = provider_type_key("anthropic", API_KEY);
		let id = store.upsert("anthropic", &key("k1", true)).expect("upsert");
		state.mark_blocked(&store, &key_name, id, now_ms() - 1, None).expect("mark");
		assert!(!state.is_blocked(&store, &key_name, id, &[], now_ms()).expect("blocked"));
	}

	#[test]
	fn ranking_hook_wins_when_it_answers() {
		struct PickLast;
		impl CredentialRanking for PickLast {
			fn select(&self, _provider: &str, candidates: &[StoredCredential], _order: &[usize]) -> Option<usize> {
				candidates.len().checked_sub(1)
			}
		}

		let store = CredentialStore::in_memory().expect("store");
		let state = SelectionState::default();
		store.upsert("anthropic", &key("k1", true)).expect("upsert");
		store.upsert("anthropic", &key("k2", true)).expect("upsert");
		let chosen = select(&store, &state, "anthropic", API_KEY, None, &[], |_| true, Some(&PickLast))
			.expect("rows")
			.expect("row");
		assert_eq!(chosen.credential.api_key_value(), Some("k2"));
	}

	#[test]
	fn login_filter_separates_login_keys() {
		let store = CredentialStore::in_memory().expect("store");
		let state = SelectionState::default();
		store.upsert("anthropic", &key("static", false)).expect("upsert");
		store.upsert("anthropic", &key("login", true)).expect("upsert");
		let login = select(&store, &state, "anthropic", API_KEY, None, &[], super::is_api_key_login, None)
			.expect("rows")
			.expect("row");
		assert_eq!(login.credential.api_key_value(), Some("login"));
	}
}
