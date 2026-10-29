//! Sign in with OpenRouter (`registry/oauth/openrouter.ts`).
//!
//! No client registration: the S256 PKCE challenge is the only proof of
//! identity, and OpenRouter never echoes `state`, so the callback runs with an
//! empty expected state. The manual-input race doubles as the API-key paste
//! path — a pasted `sk-or-…` key skips the exchange and is validated against
//! `/api/v1/auth/key`.

use serde_json::json;

use super::super::super::callback::CallbackConfig;
use super::super::super::error::{AuthError, AuthErrorKind};
use super::super::super::http::HttpRequest;
use super::super::super::pkce::Pkce;
use super::super::super::store::Credential;
use super::{CodeFlowSpec, Flow, run};

const AUTHORIZE_URL: &str = "https://openrouter.ai/auth";
const KEY_EXCHANGE_URL: &str = "https://openrouter.ai/api/v1/auth/keys";
const KEY_INFO_URL: &str = "https://openrouter.ai/api/v1/auth/key";
const API_KEY_PREFIX: &str = "sk-or-";
const CALLBACK_PORT: u16 = 54549;
const CALLBACK_PATH: &str = "/callback";

/// `loginOpenRouterOAuth` returns `credentials.access`, which `AuthStorage`
/// stores as an API key (`type: api_key, source: login`).
pub async fn login(flow: &Flow<'_>) -> Result<Credential, AuthError> {
	let pkce = Pkce::generate()?;
	let spec = CodeFlowSpec::new(CallbackConfig::new(CALLBACK_PORT, CALLBACK_PATH))
		.with_instructions(
			"Authorize OMP in your browser, or paste an existing OpenRouter API key (sk-or-…) when prompted. If the browser cannot reach this machine, paste the final redirect URL or authorization code instead.",
		)
		.with_manual("Paste the authorization code or redirect URL:");

	// OpenRouter never echoes `state`; an empty expected state disables the
	// callback-state check and the PKCE verifier binds the exchange instead.
	let exchange = run(flow, &spec, "", |redirect_uri| {
		let url = crate::auth::callback::with_query(
			AUTHORIZE_URL,
			&[
				("callback_url", redirect_uri),
				("code_challenge", pkce.challenge.as_str()),
				("code_challenge_method", "S256"),
			],
		);
		(url, None)
	})
	.await?;

	let key = if exchange.code.starts_with(API_KEY_PREFIX) {
		validate_api_key(flow, &exchange.code).await?;
		exchange.code
	} else {
		exchange_key(flow, &exchange.code, &pkce.verifier).await?
	};
	Ok(Credential::ApiKey { key, source_login: true })
}

/// `exchangeOpenRouterCode`: code + verifier → durable `sk-or-…` key.
async fn exchange_key(flow: &Flow<'_>, code: &str, verifier: &str) -> Result<String, AuthError> {
	let payload = flow
		.http
		.post_json(
			KEY_EXCHANGE_URL,
			json!({ "code": code, "code_verifier": verifier, "code_challenge_method": "S256" }),
			Vec::new(),
			flow.cancel,
			"openrouter",
			AuthErrorKind::TokenExchange,
		)
		.await?;
	let key = payload.get("key").and_then(serde_json::Value::as_str).unwrap_or_default().to_string();
	if key.is_empty() {
		return Err(AuthError::new(AuthErrorKind::Validation, "OpenRouter key exchange returned an empty key")
			.for_provider("openrouter"));
	}
	Ok(key)
}

/// `/api/v1/auth/key` is the canonical "who am I" endpoint; `/models` accepts
/// any bearer, so it cannot reject a wrong key.
async fn validate_api_key(flow: &Flow<'_>, key: &str) -> Result<(), AuthError> {
	let request = HttpRequest::get(KEY_INFO_URL).bearer(key).header("Accept", "application/json");
	let response = flow.http.send(request, flow.cancel, "openrouter").await?;
	if response.is_success() {
		return Ok(());
	}
	if matches!(response.status, 401 | 403) {
		return Err(AuthError::new(
			AuthErrorKind::Validation,
			"API key rejected by OpenRouter. Check the key and try again.",
		)
		.for_provider("openrouter")
		.with_status(response.status));
	}
	Err(response.error_for(AuthErrorKind::Validation, "openrouter", "OpenRouter key validation"))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn authorize_url_carries_pkce_and_no_client_id() {
		let url = crate::auth::callback::with_query(
			AUTHORIZE_URL,
			&[("callback_url", "http://localhost:54549/callback"), ("code_challenge", "ch"), ("code_challenge_method", "S256")],
		);
		assert_eq!(
			url,
			"https://openrouter.ai/auth?callback_url=http%3A%2F%2Flocalhost%3A54549%2Fcallback&code_challenge=ch&code_challenge_method=S256"
		);
		assert!(!url.contains("state="));
	}

	#[test]
	fn pasted_api_keys_are_recognised_by_prefix() {
		assert!("sk-or-v1-abc".starts_with(API_KEY_PREFIX));
		assert!(!"sk-other".starts_with(API_KEY_PREFIX));
	}
}
