//! Devin CLI login (`registry/oauth/devin.ts`).
//!
//! UUID CSRF state, PKCE, and a loopback server pinned to `127.0.0.1:59653`.
//! The granted CLI token is a JWT; upstream `devinProvider.login` returns
//! `credentials.access`, so the credential is stored as an API key with
//! `source: login` (the API-key envelope the Devin transport expects).

use serde_json::json;

use super::super::super::callback::CallbackConfig;
use super::super::super::error::{AuthError, AuthErrorKind};
use super::super::super::pkce::{Pkce, random_uuid_v4};
use super::super::super::store::Credential;
use super::{CodeFlowSpec, Flow, run};

const DEVIN_WEBAPP_URL: &str = "https://app.devin.ai";
const DEVIN_API_URL: &str = "https://api.devin.ai";
const TOKEN_PATH: &str = "/auth/cli/token";
const CALLBACK_PORT: u16 = 59653;
const CALLBACK_PATH: &str = "/callback";

pub async fn login(flow: &Flow<'_>) -> Result<Credential, AuthError> {
	let pkce = Pkce::generate()?;
	let state = random_uuid_v4()?;
	let spec = CodeFlowSpec::new(
		CallbackConfig::new(CALLBACK_PORT, CALLBACK_PATH).with_hostname("127.0.0.1"),
	)
	.with_instructions("Sign in to Devin in your browser.");

	let exchange = run(flow, &spec, &state, |redirect_uri| {
		let url = crate::auth::callback::with_query(
			&format!("{DEVIN_WEBAPP_URL}/auth/cli/continue"),
			&[
				("redirect_uri", redirect_uri),
				("state", state.as_str()),
				("prompt", "select_account"),
				("code_challenge", pkce.challenge.as_str()),
				("code_challenge_method", "S256"),
			],
		);
		(url, None)
	})
	.await?;

	exchange_cli_token(flow, &exchange.code, &pkce.verifier).await
}

/// `exchangeDevinCliToken`: JSON `{code, code_verifier}` → `{token}`.
async fn exchange_cli_token(flow: &Flow<'_>, code: &str, verifier: &str) -> Result<Credential, AuthError> {
	let payload = flow
		.http
		.post_json(
			&format!("{DEVIN_API_URL}{TOKEN_PATH}"),
			json!({ "code": code, "code_verifier": verifier }),
			vec![("Accept".to_string(), "application/json".to_string())],
			flow.cancel,
			"devin",
			AuthErrorKind::TokenExchange,
		)
		.await?;
	let token = payload.get("token").and_then(serde_json::Value::as_str).unwrap_or_default().to_string();
	if token.is_empty() {
		return Err(AuthError::new(
			AuthErrorKind::Validation,
			"Devin CLI token exchange returned an empty token",
		)
		.for_provider("devin"));
	}
	Ok(Credential::ApiKey { key: token, source_login: true })
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn authorize_url_pins_redirect_and_prompt() {
		let url = crate::auth::callback::with_query(
			&format!("{DEVIN_WEBAPP_URL}/auth/cli/continue"),
			&[
				("redirect_uri", "http://127.0.0.1:59653/callback"),
				("state", "uuid"),
				("prompt", "select_account"),
				("code_challenge", "ch"),
				("code_challenge_method", "S256"),
			],
		);
		assert!(url.starts_with("https://app.devin.ai/auth/cli/continue?redirect_uri=http%3A%2F%2F127.0.0.1%3A59653%2Fcallback"));
		assert!(url.contains("prompt=select_account"));
		assert!(url.contains("code_challenge_method=S256"));
	}

	#[test]
	fn token_path_is_under_api_host() {
		assert_eq!(format!("{DEVIN_API_URL}{TOKEN_PATH}"), "https://api.devin.ai/auth/cli/token");
	}
}
