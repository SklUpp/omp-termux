//! OpenAI Codex OAuth (`registry/oauth/openai-codex.ts`).
//!
//! Browser flow on `localhost:1455/auth/callback`, form-encoded token exchange,
//! identity from the access-token JWT (`chatgpt_account_id`, profile email,
//! plan type). The device flow reuses [`exchange_code`] with the device redirect
//! URI, so both variants store the same credential shape.

use base64::Engine as _;

use super::super::super::callback::CallbackConfig;
use super::super::super::error::{AuthError, AuthErrorKind};
use super::super::super::pkce::Pkce;
use super::super::super::store::{Credential, OAuthCredentials};
use super::{CodeFlowSpec, Flow, run};

pub(crate) const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub(crate) const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
pub(crate) const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
pub(crate) const DEVICE_USERCODE_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
pub(crate) const DEVICE_TOKEN_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
pub(crate) const DEVICE_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
pub(crate) const DEVICE_AUTH_URL: &str = "https://auth.openai.com/codex/device";
const CALLBACK_PORT: u16 = 1455;
const CALLBACK_PATH: &str = "/auth/callback";
const SCOPE: &str = "openid profile email offline_access api.connectors.read api.connectors.invoke";
const JWT_CLAIM_PATH: &str = "https://api.openai.com/auth";
const JWT_PROFILE_CLAIM: &str = "https://api.openai.com/profile";
/// `OPENAI_HEADER_VALUES.ORIGINATOR_CODEX`.
pub(crate) const ORIGINATOR: &str = "pi";
pub(crate) const TOKEN_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

pub async fn login(flow: &Flow<'_>) -> Result<Credential, AuthError> {
	let pkce = Pkce::generate()?;
	let state = super::super::super::pkce::random_state()?;
	let callback = CallbackConfig::new(CALLBACK_PORT, CALLBACK_PATH)
		.with_redirect_uri(&format!("http://localhost:{CALLBACK_PORT}{CALLBACK_PATH}"))
		.without_port_fallback();
	let spec = CodeFlowSpec::new(callback)
		.with_instructions("A browser window should open. Complete login to finish.")
		.with_manual("Paste the authorization code or redirect URL:");

	let exchange = run(flow, &spec, &state, |redirect_uri| {
		(
			authorization_url(&state, redirect_uri, &pkce.challenge),
			Some("A browser window should open. Complete login to finish.".to_string()),
		)
	})
	.await?;

	exchange_code(flow, &exchange.code, &pkce.verifier, &exchange.redirect_uri, AuthErrorKind::TokenExchange).await
}

/// `createOpenAICodexAuthorizationUrl`.
#[must_use]
pub fn authorization_url(state: &str, redirect_uri: &str, challenge: &str) -> String {
	let params: Vec<(&str, &str)> = vec![
		("response_type", "code"),
		("client_id", CLIENT_ID),
		("redirect_uri", redirect_uri),
		("scope", SCOPE),
		("code_challenge", challenge),
		("code_challenge_method", "S256"),
		("state", state),
		("id_token_add_organizations", "true"),
		("codex_cli_simplified_flow", "true"),
		("originator", ORIGINATOR),
	];
	super::super::super::callback::with_query(AUTHORIZE_URL, &params)
}

/// Form-encoded exchange shared by browser and device flows.
pub(crate) async fn exchange_code(
	flow: &Flow<'_>,
	code: &str,
	verifier: &str,
	redirect_uri: &str,
	kind: AuthErrorKind,
) -> Result<Credential, AuthError> {
	let fields = vec![
		("grant_type".to_string(), "authorization_code".to_string()),
		("client_id".to_string(), CLIENT_ID.to_string()),
		("code".to_string(), code.to_string()),
		("code_verifier".to_string(), verifier.to_string()),
		("redirect_uri".to_string(), redirect_uri.to_string()),
	];
	let response = flow
		.http
		.send(
			super::super::super::http::HttpRequest::form(TOKEN_URL, fields).timeout(TOKEN_REQUEST_TIMEOUT),
			flow.cancel,
			"openai-codex",
		)
		.await?;
	if !response.is_success() {
		return Err(AuthError::new(
			kind,
			format!("Token exchange failed: {}", format_token_error(response.status, &response.body)),
		)
		.for_provider("openai-codex")
		.with_status(response.status));
	}
	let payload = response.json("openai-codex")?;
	let access = payload.get("access_token").and_then(serde_json::Value::as_str).unwrap_or_default().to_string();
	let refresh = payload.get("refresh_token").and_then(serde_json::Value::as_str).unwrap_or_default().to_string();
	let expires_in = payload.get("expires_in").and_then(serde_json::Value::as_i64);
	let id_token = payload.get("id_token").and_then(serde_json::Value::as_str).map(ToString::to_string);
	if access.is_empty() || refresh.is_empty() || expires_in.is_none() {
		return Err(AuthError::new(AuthErrorKind::Validation, "Token response missing required fields")
			.for_provider("openai-codex"));
	}
	let profile = TokenProfile::from_tokens(&access, id_token.as_deref());
	let account_id = profile
		.account_id
		.ok_or_else(|| AuthError::new(AuthErrorKind::Validation, "Failed to extract accountId from token").for_provider("openai-codex"))?;

	Ok(Credential::OAuth(OAuthCredentials {
		access,
		refresh,
		expires: now_ms() + expires_in.unwrap_or(0) * 1000,
		account_id: Some(account_id.clone()),
		email: profile.email,
		org_id: Some(account_id),
		org_name: profile.plan_type,
		..OAuthCredentials::default()
	}))
}

pub async fn refresh(flow: &Flow<'_>, credentials: &OAuthCredentials) -> Result<Credential, AuthError> {
	if credentials.refresh.is_empty() {
		return Err(AuthError::new(AuthErrorKind::TokenRefresh, "stored credential has no refresh token")
			.for_provider("openai-codex"));
	}
	let fields = vec![
		("grant_type".to_string(), "refresh_token".to_string()),
		("refresh_token".to_string(), credentials.refresh.clone()),
		("client_id".to_string(), CLIENT_ID.to_string()),
	];
	let response = flow
		.http
		.send(
			super::super::super::http::HttpRequest::form(TOKEN_URL, fields).timeout(TOKEN_REQUEST_TIMEOUT),
			flow.cancel,
			"openai-codex",
		)
		.await?;
	if !response.is_success() {
		return Err(AuthError::new(
			AuthErrorKind::TokenRefresh,
			format!("OpenAI Codex token refresh failed: {}", format_token_error(response.status, &response.body)),
		)
		.for_provider("openai-codex")
		.with_status(response.status));
	}
	let payload = response.json("openai-codex")?;
	let access = payload.get("access_token").and_then(serde_json::Value::as_str).unwrap_or_default().to_string();
	let refresh_token = payload.get("refresh_token").and_then(serde_json::Value::as_str).unwrap_or_default().to_string();
	let expires_in = payload.get("expires_in").and_then(serde_json::Value::as_i64);
	if access.is_empty() || refresh_token.is_empty() || expires_in.is_none() {
		return Err(AuthError::new(AuthErrorKind::Validation, "Token response missing required fields")
			.for_provider("openai-codex"));
	}
	let profile = TokenProfile::from_tokens(&access, None);
	Ok(Credential::OAuth(OAuthCredentials {
		access,
		refresh: if refresh_token.is_empty() { credentials.refresh.clone() } else { refresh_token },
		expires: now_ms() + expires_in.unwrap_or(0) * 1000,
		account_id: profile.account_id.or_else(|| credentials.account_id.clone()),
		email: profile.email,
		org_id: credentials.org_id.clone(),
		org_name: credentials.org_name.clone(),
		authorized_at: credentials.authorized_at,
		..OAuthCredentials::default()
	}))
}

/// `formatOpenAICodexTokenEndpointError`: `status code: description`.
#[must_use]
pub fn format_token_error(status: u16, body: &str) -> String {
	// Only allowlisted OAuth identifiers reach the message; `error_description`
	// is provider prose and is dropped, and a free-form body is never echoed.
	let summary = crate::auth::error::summarize_error_body(body);
	if summary.is_empty() {
		status.to_string()
	} else {
		format!("{status} {summary}")
	}
}

/// JWT-derived identity (`getTokenProfile`).
#[derive(Debug, Default)]
pub(crate) struct TokenProfile {
	pub account_id: Option<String>,
	pub email: Option<String>,
	pub plan_type: Option<String>,
}

impl TokenProfile {
	pub fn from_tokens(access_token: &str, id_token: Option<&str>) -> Self {
		let payload = decode_jwt(access_token);
		let id_payload = id_token.and_then(decode_jwt);
		let auth = payload.as_ref().and_then(|payload| payload.get(JWT_CLAIM_PATH));
		let id_auth = id_payload.as_ref().and_then(|payload| payload.get(JWT_CLAIM_PATH));
		let account_id = auth
			.and_then(|auth| auth.get("chatgpt_account_id"))
			.and_then(serde_json::Value::as_str)
			.filter(|value| !value.is_empty())
			.map(ToString::to_string);
		let email = payload
			.as_ref()
			.and_then(|payload| payload.get(JWT_PROFILE_CLAIM))
			.and_then(|profile| profile.get("email"))
			.and_then(serde_json::Value::as_str)
			.map(|email| email.trim().to_lowercase())
			.filter(|email| !email.is_empty());
		let plan_type = auth
			.and_then(|auth| auth.get("chatgpt_plan_type"))
			.or_else(|| id_auth.and_then(|auth| auth.get("chatgpt_plan_type")))
			.and_then(serde_json::Value::as_str)
			.map(|plan| plan.trim().to_lowercase())
			.filter(|plan| !plan.is_empty());
		Self { account_id, email, plan_type }
	}
}

/// `decodeJwt`: three dot-separated parts, padded or unpadded base64 payload.
#[must_use]
pub fn decode_jwt(token: &str) -> Option<serde_json::Value> {
	let mut parts = token.split('.');
	let (_header, payload) = (parts.next()?, parts.next()?);
	if parts.next().is_none() {
		return None;
	}
	let engine = base64::engine::GeneralPurpose::new(
		&base64::alphabet::STANDARD,
		base64::engine::GeneralPurposeConfig::new()
			.with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent),
	);
	let bytes = engine.decode(payload).ok()?;
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
	use serde_json::json;

	fn jwt(claims: serde_json::Value) -> String {
		let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
		format!("header.{}.signature", engine.encode(claims.to_string()))
	}

	#[test]
	fn authorization_url_contains_codex_flags() {
		let url = authorization_url("st", "http://localhost:1455/auth/callback", "ch");
		assert!(url.starts_with("https://auth.openai.com/oauth/authorize?response_type=code&client_id=app_EMoamEEZ73f0CkXaXp7hrann"));
		assert!(url.contains("codex_cli_simplified_flow=true"));
		assert!(url.contains("id_token_add_organizations=true"));
		assert!(url.contains("originator=pi"));
		assert!(url.contains("state=st"));
	}

	#[test]
	fn profile_reads_account_email_and_plan() {
		let token = jwt(json!({
			"https://api.openai.com/auth": { "chatgpt_account_id": "acct-1", "chatgpt_plan_type": "Plus" },
			"https://api.openai.com/profile": { "email": " User@Example.COM " }
		}));
		let profile = TokenProfile::from_tokens(&token, None);
		assert_eq!(profile.account_id.as_deref(), Some("acct-1"));
		assert_eq!(profile.email.as_deref(), Some("user@example.com"));
		assert_eq!(profile.plan_type.as_deref(), Some("plus"));
	}

	#[test]
	fn malformed_jwt_has_no_profile() {
		assert!(decode_jwt("not-a-jwt").is_none());
		assert!(decode_jwt("a.b").is_none());
		let profile = TokenProfile::from_tokens("a.b.c", None);
		assert!(profile.account_id.is_none() && profile.email.is_none());
	}

	#[test]
	fn token_error_formatting_keeps_identifiers_only() {
		assert_eq!(format_token_error(400, ""), "400");
		assert_eq!(
			format_token_error(400, r#"{"error":"invalid_grant","error_description":"expired"}"#),
			"400 invalid_grant"
		);
		assert_eq!(format_token_error(500, "boom"), "500");
	}
}
