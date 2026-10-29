//! Anthropic Claude Pro/Max OAuth (`registry/oauth/anthropic.ts`).
//!
//! Token exchange is JSON (not form-encoded), the grant is short-lived
//! (`expires_in - 5 min` skew), and identity comes from the token response or
//! the Code Assist bootstrap endpoint, whose failure is tolerated.

use serde_json::json;

use super::super::super::callback::CallbackConfig;
use super::super::super::error::{AuthError, AuthErrorKind};
use super::super::super::http::HttpRequest;
use super::super::super::pkce::Pkce;
use super::super::super::store::OAuthCredentials;
use super::{CodeFlowSpec, Flow, run, split_code_fragment};

/// `decode("OWQx...")` in `registry/oauth/anthropic.ts`; kept encoded here.
const CLIENT_ID_B64: &str = "OWQxYzI1MGEtZTYxYi00NGQ5LTg4ZWQtNTk0NGQxOTYyZjVl";
const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
const TOKEN_URL: &str = "https://api.anthropic.com/v1/oauth/token";
const BOOTSTRAP_URL: &str = "https://api.anthropic.com/api/claude_cli/bootstrap";
const BOOTSTRAP_MODEL: &str = "claude-opus-4-8";
const CALLBACK_PORT: u16 = 54545;
const CALLBACK_PATH: &str = "/callback";
const SCOPES: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";
const GRANT_TTL_SKEW_MS: i64 = 5 * 60 * 1000;

pub async fn login(flow: &Flow<'_>) -> Result<super::super::super::store::Credential, AuthError> {
	let client_id = super::decode_constant("anthropic client id", CLIENT_ID_B64)?;
	let pkce = Pkce::generate()?;
	let state = super::super::super::pkce::random_state()?;
	// A pinned loopback redirect URI is registered with the provider, so the port
	// must be free rather than silently swapped.
	let callback = CallbackConfig::new(CALLBACK_PORT, CALLBACK_PATH)
		.with_redirect_uri(&format!("http://localhost:{CALLBACK_PORT}{CALLBACK_PATH}"))
		.without_port_fallback();
	let spec = CodeFlowSpec::new(callback)
		.with_instructions(
			"Complete login in your browser. If the browser cannot reach this machine, paste the final redirect URL or authorization code when prompted.",
		)
		.with_manual("Paste the authorization code or redirect URL:");

	let exchange = run(flow, &spec, &state, |redirect_uri| {
		let url = super::super::authorize_url(
			AUTHORIZE_URL,
			&super::super::AuthorizeParams {
				client_id: &client_id,
				redirect_uri,
				scopes: Some(SCOPES),
				pkce_challenge: Some(&pkce.challenge),
				state: &state,
				extra: &[("code", "true")],
			},
		);
		(url, None)
	})
	.await?;

	let (code, exchange_state) = split_code_fragment(&exchange.code, &exchange.state);
	let body = json!({
		"grant_type": "authorization_code",
		"client_id": client_id,
		"code": code,
		"state": exchange_state,
		"redirect_uri": exchange.redirect_uri,
		"code_verifier": pkce.verifier,
	});
	let mut headers = vec![("Accept".to_string(), "application/json".to_string())];
	headers.push(("Content-Type".to_string(), "application/json".to_string()));
	let payload = flow
		.http
		.post_json(
			TOKEN_URL,
			body,
			headers,
			flow.cancel,
			"anthropic",
			AuthErrorKind::TokenExchange,
		)
		.await?;

	let tokens = TokenResponse::parse(&payload)?;
	let mut credentials = OAuthCredentials {
		access: tokens.access_token.clone(),
		refresh: tokens.refresh_token.clone(),
		expires: now_ms() + tokens.expires_in * 1000 - GRANT_TTL_SKEW_MS,
		account_id: non_empty(tokens.account_uuid),
		email: non_empty(tokens.account_email),
		org_id: non_empty(tokens.organization_uuid),
		org_name: non_empty(tokens.organization_name),
		..OAuthCredentials::default()
	};
	fill_identity_from_bootstrap(flow, &mut credentials).await;
	Ok(super::super::super::store::Credential::OAuth(credentials))
}

pub async fn refresh(
	flow: &Flow<'_>,
	credentials: &OAuthCredentials,
) -> Result<super::super::super::store::Credential, AuthError> {
	if credentials.refresh.is_empty() {
		return Err(AuthError::new(AuthErrorKind::TokenRefresh, "stored credential has no refresh token")
			.for_provider("anthropic"));
	}
	let client_id = super::decode_constant("anthropic client id", CLIENT_ID_B64)?;
	let body = json!({
		"grant_type": "refresh_token",
		"client_id": client_id,
		"refresh_token": credentials.refresh,
	});
	let mut headers = vec![("anthropic-beta".to_string(), "oauth-2025-04-20".to_string())];
	headers.push(("Content-Type".to_string(), "application/json".to_string()));
	let payload = flow
		.http
		.post_json(TOKEN_URL, body, headers, flow.cancel, "anthropic", AuthErrorKind::TokenRefresh)
		.await?;

	let tokens = TokenResponse::parse(&payload)?;
	let mut refreshed = OAuthCredentials {
		access: tokens.access_token.clone(),
		refresh: if tokens.refresh_token.is_empty() {
			credentials.refresh.clone()
		} else {
			tokens.refresh_token.clone()
		},
		expires: now_ms() + tokens.expires_in * 1000 - GRANT_TTL_SKEW_MS,
		account_id: non_empty(tokens.account_uuid).or_else(|| credentials.account_id.clone()),
		email: non_empty(tokens.account_email).or_else(|| credentials.email.clone()),
		org_id: non_empty(tokens.organization_uuid),
		org_name: non_empty(tokens.organization_name),
		authorized_at: credentials.authorized_at,
		..OAuthCredentials::default()
	};
	fill_identity_from_bootstrap(flow, &mut refreshed).await;
	Ok(super::super::super::store::Credential::OAuth(refreshed))
}

#[derive(Debug)]
struct TokenResponse {
	access_token: String,
	refresh_token: String,
	expires_in: i64,
	account_uuid: Option<String>,
	account_email: Option<String>,
	organization_uuid: Option<String>,
	organization_name: Option<String>,
}

impl TokenResponse {
	fn parse(payload: &serde_json::Value) -> Result<Self, AuthError> {
		let access_token = payload.get("access_token").and_then(serde_json::Value::as_str).unwrap_or_default().to_string();
		let refresh_token =
			payload.get("refresh_token").and_then(serde_json::Value::as_str).unwrap_or_default().to_string();
		let expires_in = payload.get("expires_in").and_then(serde_json::Value::as_i64);
		if access_token.is_empty() || refresh_token.is_empty() || expires_in.is_none() {
			return Err(AuthError::new(
				AuthErrorKind::Validation,
				"Anthropic token response missing required fields",
			)
			.for_provider("anthropic"));
		}
		let account = payload.get("account");
		let organization = payload.get("organization");
		Ok(Self {
			access_token,
			refresh_token,
			expires_in: expires_in.unwrap_or(0),
			account_uuid: string_at(account, "uuid"),
			account_email: string_at(account, "email_address"),
			organization_uuid: string_at(organization, "uuid"),
			organization_name: string_at(organization, "name"),
		})
	}
}

fn string_at(value: Option<&serde_json::Value>, key: &str) -> Option<String> {
	value.and_then(|value| value.get(key)).and_then(serde_json::Value::as_str).map(ToString::to_string)
}

fn non_empty(value: Option<String>) -> Option<String> {
	value.filter(|value| !value.is_empty())
}

/// The bootstrap call only fills gaps; any failure keeps the token identity so a
/// flaky identity endpoint never fails a completed login.
async fn fill_identity_from_bootstrap(flow: &Flow<'_>, credentials: &mut OAuthCredentials) {
	let org_satisfied = credentials.org_id.is_some();
	if credentials.account_id.is_some() && credentials.email.is_some() && org_satisfied {
		return;
	}
	let url = format!(
		"{BOOTSTRAP_URL}?entrypoint=cli&model={}",
		super::super::super::callback::encode_component(BOOTSTRAP_MODEL)
	);
	let request = HttpRequest::get(url)
		.bearer(&credentials.access)
		.header("Accept", "application/json, text/plain, */*")
		.header("Content-Type", "application/json")
		.header("anthropic-beta", "oauth-2025-04-20");
	match flow.http.send(request, flow.cancel, "anthropic").await {
		Ok(response) if response.is_success() => {
			if let Ok(payload) = serde_json::from_str::<serde_json::Value>(&response.body) {
				let account = payload.get("oauth_account");
				credentials.account_id = credentials.account_id.clone().or_else(|| string_at(account, "account_uuid"));
				credentials.email = credentials.email.clone().or_else(|| string_at(account, "account_email"));
				credentials.org_id = credentials.org_id.clone().or_else(|| string_at(account, "organization_uuid"));
				credentials.org_name = credentials.org_name.clone().or_else(|| string_at(account, "organization_name"));
				credentials.authorized_at = Some(now_ms());
			}
		},
		Ok(_) => {},
		Err(err) if err.kind == AuthErrorKind::Cancelled => {},
		Err(_) => {},
	}
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

	#[test]
	fn token_response_requires_full_grant() {
		let short = json!({ "access_token": "a", "refresh_token": "r" });
		assert_eq!(TokenResponse::parse(&short).expect_err("missing expires").kind, AuthErrorKind::Validation);

		let full = json!({
			"access_token": "a",
			"refresh_token": "r",
			"expires_in": 3600,
			"account": { "uuid": "acct", "email_address": "user@example.com" },
			"organization": { "uuid": "org", "name": "Team" }
		});
		let tokens = TokenResponse::parse(&full).expect("parsed");
		assert_eq!(tokens.account_uuid.as_deref(), Some("acct"));
		assert_eq!(tokens.organization_name.as_deref(), Some("Team"));
	}

	#[test]
	fn grant_expiry_applies_five_minute_skew() {
		let expires = now_ms() + 3600 * 1000 - GRANT_TTL_SKEW_MS;
		assert!(expires < now_ms() + 3600 * 1000);
		assert!(expires > now_ms());
	}
}
