//! Cursor login (`registry/oauth/cursor.ts`).
//!
//! No loopback server: the login URL embeds a PKCE challenge plus a UUID, and
//! the CLI polls `api2.cursor.sh/auth/poll` with exponential backoff until the
//! browser flow completes. `404` means "not finished yet"; three consecutive
//! errors abort.

use std::time::Duration;

use base64::Engine as _;
use serde_json::json;

use super::super::super::error::{AuthError, AuthErrorKind};
use super::super::super::http::{HttpRequest, HttpResponse};
use super::super::super::pkce::{Pkce, random_uuid_v4};
use super::super::super::store::{Credential, OAuthCredentials};
use super::Flow;

const CURSOR_LOGIN_URL: &str = "https://cursor.com/loginDeepControl";
const CURSOR_POLL_URL: &str = "https://api2.cursor.sh/auth/poll";
const CURSOR_REFRESH_URL: &str = "https://api2.cursor.sh/auth/exchange_user_api_key";
const POLL_MAX_ATTEMPTS: u32 = 150;
const POLL_BASE_DELAY_MS: u64 = 1000;
const POLL_MAX_DELAY_MS: u64 = 10_000;
/// Poll delay grows by 1.2× per unfinished attempt.
const POLL_GROWTH_NUM: u64 = 6;
const POLL_GROWTH_DEN: u64 = 5;
/// A JWT `exp` is authoritative; this constant bounds the fallback.
const FALLBACK_TTL_MS: i64 = 3600 * 1000;
const EXPIRY_SKEW_MS: i64 = 5 * 60 * 1000;

pub async fn login(flow: &Flow<'_>) -> Result<Credential, AuthError> {
	let pkce = Pkce::generate()?;
	let uuid = random_uuid_v4()?;
	let login_url = crate::auth::callback::with_query(
		CURSOR_LOGIN_URL,
		&[
			("challenge", pkce.challenge.as_str()),
			("uuid", uuid.as_str()),
			("mode", "login"),
			("redirectTarget", "cli"),
		],
	);
	// Cursor has no callback server; the URL is the only thing the host opens.
	flow.open(&login_url, None, None);
	flow.notice("Waiting for browser authentication...");

	let (access, refresh) = poll(flow, &uuid, &pkce.verifier).await?;
	Ok(Credential::OAuth(OAuthCredentials {
		expires: token_expiry(&access),
		access,
		refresh,
		..OAuthCredentials::default()
	}))
}

async fn poll(flow: &Flow<'_>, uuid: &str, verifier: &str) -> Result<(String, String), AuthError> {
	let mut delay_ms = POLL_BASE_DELAY_MS;
	let mut consecutive_errors = 0u32;
	for _attempt in 0..POLL_MAX_ATTEMPTS {
		flow.check()?;
		tokio::time::sleep(Duration::from_millis(delay_ms)).await;
		flow.check()?;

		let url = super::super::super::callback::with_query(
			CURSOR_POLL_URL,
			&[("uuid", uuid), ("verifier", verifier)],
		);
		let response = match flow.http.send(HttpRequest::get(url), flow.cancel, "cursor").await {
			Ok(response) => response,
			Err(err) if err.kind == AuthErrorKind::Cancelled => return Err(err),
			Err(_) => {
				consecutive_errors += 1;
				if consecutive_errors >= 3 {
					return Err(AuthError::new(
						AuthErrorKind::Polling,
						"Too many consecutive errors during Cursor auth polling",
					)
					.for_provider("cursor"));
				}
				continue;
			},
		};

		if response.status == 404 {
			consecutive_errors = 0;
			delay_ms = (delay_ms * POLL_GROWTH_NUM / POLL_GROWTH_DEN).min(POLL_MAX_DELAY_MS);
			continue;
		}
		if response.is_success() {
			return parse_tokens(&response);
		}
		return Err(response.error_for(AuthErrorKind::Polling, "cursor", "Cursor auth poll"));
	}
	Err(AuthError::new(AuthErrorKind::Timeout, "Cursor authentication polling timeout").for_provider("cursor"))
}

fn parse_tokens(response: &HttpResponse) -> Result<(String, String), AuthError> {
	let payload = response.json("cursor")?;
	let access = payload.get("accessToken").and_then(serde_json::Value::as_str).unwrap_or_default().to_string();
	let refresh = payload.get("refreshToken").and_then(serde_json::Value::as_str).unwrap_or_default().to_string();
	if access.is_empty() {
		return Err(AuthError::new(AuthErrorKind::Validation, "Cursor poll response missing accessToken")
			.for_provider("cursor"));
	}
	Ok((access, refresh))
}

pub async fn refresh(
	flow: &Flow<'_>,
	credentials: &OAuthCredentials,
) -> Result<Credential, AuthError> {
	let secret = if credentials.refresh.is_empty() { &credentials.access } else { &credentials.refresh };
	if secret.is_empty() {
		return Err(AuthError::new(AuthErrorKind::TokenRefresh, "stored credential has no refresh token")
			.for_provider("cursor"));
	}
	let payload = flow
		.http
		.post_json(
			CURSOR_REFRESH_URL,
			json!({}),
			vec![("Authorization".to_string(), format!("Bearer {secret}"))],
			flow.cancel,
			"cursor",
			AuthErrorKind::TokenRefresh,
		)
		.await?;
	let access = payload.get("accessToken").and_then(serde_json::Value::as_str).unwrap_or_default().to_string();
	if access.is_empty() {
		return Err(AuthError::new(AuthErrorKind::Validation, "Cursor refresh response missing accessToken")
			.for_provider("cursor"));
	}
	let refresh = payload
		.get("refreshToken")
		.and_then(serde_json::Value::as_str)
		.filter(|value| !value.is_empty())
		.map_or_else(|| secret.clone(), ToString::to_string);
	Ok(Credential::OAuth(OAuthCredentials {
		expires: token_expiry(&access),
		access,
		refresh,
		..credentials.clone()
	}))
}

/// Cursor access tokens are JWTs; a missing or malformed `exp` falls back to
/// one hour so the refresh lease still has a deadline.
fn token_expiry(token: &str) -> i64 {
	let exp = jwt_claims(token).and_then(|payload| payload.get("exp").and_then(serde_json::Value::as_i64));
	match exp {
		Some(exp) => exp.saturating_mul(1000).saturating_sub(EXPIRY_SKEW_MS),
		None => now_ms().saturating_add(FALLBACK_TTL_MS),
	}
}

/// `extractCursorAccessTokenUserId`: `sub` after the last `|`.
#[must_use]
pub fn access_token_user_id(token: &str) -> Option<String> {
	let sub = jwt_claims(token)?.get("sub")?.as_str()?.trim().to_string();
	if sub.is_empty() {
		return None;
	}
	let user = sub.rsplit('|').next().unwrap_or(&sub).trim().to_string();
	(!user.is_empty()).then_some(user)
}

fn jwt_claims(token: &str) -> Option<serde_json::Value> {
	let payload = token.split('.').nth(1)?;
	let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload).ok()?;
	serde_json::from_slice(&bytes).ok()
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

	fn jwt(payload: serde_json::Value) -> String {
		let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
		format!("h.{}.s", engine.encode(serde_json::to_vec(&payload).expect("json")))
	}

	#[test]
	fn expiry_uses_jwt_exp_minus_skew() {
		let token = jwt(json!({ "exp": 1_700_000_000 }));
		assert_eq!(token_expiry(&token), 1_700_000_000_000 - EXPIRY_SKEW_MS);
	}

	#[test]
	fn expiry_falls_back_for_opaque_tokens() {
		let expiry = token_expiry("not-a-jwt");
		assert!(expiry > now_ms());
		assert!(expiry <= now_ms() + FALLBACK_TTL_MS + 1000);
	}

	#[test]
	fn user_id_splits_subject() {
		let token = jwt(json!({ "sub": "auth0|user-42" }));
		assert_eq!(access_token_user_id(&token).as_deref(), Some("user-42"));
		let plain = jwt(json!({ "sub": "user-7" }));
		assert_eq!(access_token_user_id(&plain).as_deref(), Some("user-7"));
		assert_eq!(access_token_user_id("opaque"), None);
	}

	#[test]
	fn login_url_shape() {
		let url = crate::auth::callback::with_query(
			CURSOR_LOGIN_URL,
			&[("challenge", "ch"), ("uuid", "u"), ("mode", "login"), ("redirectTarget", "cli")],
		);
		assert_eq!(url, "https://cursor.com/loginDeepControl?challenge=ch&uuid=u&mode=login&redirectTarget=cli");
	}
}
