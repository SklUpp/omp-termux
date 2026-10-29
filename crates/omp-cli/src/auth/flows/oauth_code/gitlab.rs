//! GitLab logins: `gitlab-duo` (loopback `localhost:8080/callback`, bundled OAuth
//! app, `GITLAB_CLIENT_ID`/`GITLAB_REDIRECT_URI` overrides) and
//! `gitlab-duo-agent` (GitLab's official VS Code app, `vscode://` redirect that
//! is always completed by pasting the callback URL back).

use serde_json::json;

use super::super::super::callback::{CallbackConfig, encode_component};
use super::super::super::error::{AuthError, AuthErrorKind};
use super::super::super::pkce::Pkce;
use super::super::super::store::{Credential, OAuthCredentials};
use super::{CodeFlowSpec, Flow, run};

const GITLAB_URL: &str = "https://gitlab.com";
const DUO_CLIENT_ID: &str = "da4edff2e6ebd2bc3208611e2768bc1c1dd7be791dc5ff26ca34ca9ee44f7d4b";
const DUO_CALLBACK_PORT: u16 = 8080;
const DUO_CALLBACK_PATH: &str = "/callback";
const WORKFLOW_CLIENT_ID: &str = "36f2a70cddeb5a0889d4fd8295c241b7e9848e89cf9e599d0eed2d8e5350fbf5";
const WORKFLOW_REDIRECT_URI: &str = "vscode://gitlab.gitlab-workflow/authentication";
const SCOPES: &str = "api";
const EXPIRY_SKEW_MS: i64 = 5 * 60 * 1000;

pub async fn login(flow: &Flow<'_>, workflow: bool) -> Result<Credential, AuthError> {
	if workflow { login_workflow(flow).await } else { login_duo(flow).await }
}

async fn login_duo(flow: &Flow<'_>) -> Result<Credential, AuthError> {
	let pkce = Pkce::generate()?;
	let state = super::super::super::pkce::random_state()?;
	let client_id = duo_client_id();
	let callback = duo_callback_config()?;
	let pinned = callback.redirect_uri.clone();
	let spec = CodeFlowSpec::new(callback)
		.with_instructions(
			"Complete GitLab login in browser. If GitLab responds with \"The redirect URI included is not valid\", register your own GitLab OAuth application and set GITLAB_CLIENT_ID + GITLAB_REDIRECT_URI, or use a Personal Access Token via GITLAB_TOKEN.",
		)
		.with_manual("Paste the full redirect URL or authorization code:");

	let exchange = run(flow, &spec, &state, |redirect_uri| {
		let url = super::super::authorize_url(
			&format!("{GITLAB_URL}/oauth/authorize"),
			&super::super::AuthorizeParams {
				client_id: &client_id,
				redirect_uri: pinned.as_deref().unwrap_or(redirect_uri),
				scopes: Some(SCOPES),
				pkce_challenge: Some(&pkce.challenge),
				state: &state,
				extra: &[],
			},
		);
		(url, None)
	})
	.await?;

	let redirect_uri = pinned.unwrap_or(exchange.redirect_uri);
	let fields = vec![
		("client_id".to_string(), client_id),
		("grant_type".to_string(), "authorization_code".to_string()),
		("code".to_string(), exchange.code),
		("code_verifier".to_string(), pkce.verifier),
		("redirect_uri".to_string(), redirect_uri),
	];
	exchange_form(flow, fields, "GitLab OAuth token exchange failed", AuthErrorKind::TokenExchange).await
}

async fn login_workflow(flow: &Flow<'_>) -> Result<Credential, AuthError> {
	let pkce = Pkce::generate()?;
	let state = super::super::super::pkce::random_state()?;
	// No listener can receive a `vscode://` redirect; a random loopback port is
	// bound only so the shared engine has a uniform shape.
	let callback = CallbackConfig::new(0, DUO_CALLBACK_PATH).with_redirect_uri(WORKFLOW_REDIRECT_URI);
	let spec = CodeFlowSpec::new(callback)
		.with_instructions(
			"Complete GitLab login in your browser. This uses GitLab's official VS Code OAuth application. If the redirect opens VS Code instead of returning to OMP, copy the full vscode://gitlab.gitlab-workflow/authentication?... callback URL from VS Code/browser and paste it back into OMP.",
		)
		.with_manual("Paste the vscode://gitlab.gitlab-workflow/authentication?... callback URL:");

	let exchange = run(flow, &spec, &state, |_redirect_uri| {
		let url = super::super::authorize_url(
			&format!("{GITLAB_URL}/oauth/authorize"),
			&super::super::AuthorizeParams {
				client_id: WORKFLOW_CLIENT_ID,
				redirect_uri: WORKFLOW_REDIRECT_URI,
				scopes: Some(SCOPES),
				pkce_challenge: Some(&pkce.challenge),
				state: &state,
				extra: &[],
			},
		);
		(url, None)
	})
	.await?;

	let fields = vec![
		("client_id".to_string(), WORKFLOW_CLIENT_ID.to_string()),
		("redirect_uri".to_string(), WORKFLOW_REDIRECT_URI.to_string()),
		("grant_type".to_string(), "authorization_code".to_string()),
		("code".to_string(), exchange.code),
		("code_verifier".to_string(), pkce.verifier),
	];
	exchange_form(flow, fields, "GitLab Duo Workflow OAuth token exchange failed", AuthErrorKind::TokenExchange)
		.await
}

pub async fn refresh(flow: &Flow<'_>, credentials: &OAuthCredentials) -> Result<Credential, AuthError> {
	let workflow = flow.provider() == "gitlab-duo-agent";
	if credentials.refresh.is_empty() {
		return Err(AuthError::new(AuthErrorKind::TokenRefresh, "stored credential has no refresh token")
			.for_provider(flow.provider()));
	}
	let mut fields = vec![
		("client_id".to_string(), if workflow { WORKFLOW_CLIENT_ID.to_string() } else { duo_client_id() }),
		(
			"grant_type".to_string(),
			"refresh_token".to_string(),
		),
		("refresh_token".to_string(), credentials.refresh.clone()),
	];
	if workflow {
		fields.push(("redirect_uri".to_string(), WORKFLOW_REDIRECT_URI.to_string()));
	}
	let message = if workflow {
		"GitLab Duo Workflow OAuth refresh failed"
	} else {
		"GitLab OAuth refresh failed"
	};
	exchange_form(flow, fields, message, AuthErrorKind::TokenRefresh).await
}

async fn exchange_form(
	flow: &Flow<'_>,
	fields: Vec<(String, String)>,
	failure: &str,
	kind: AuthErrorKind,
) -> Result<Credential, AuthError> {
	let response = flow
		.http
		.send(
			super::super::super::http::HttpRequest::form(format!("{GITLAB_URL}/oauth/token"), fields),
			flow.cancel,
			flow.provider(),
		)
		.await?;
	if !response.is_success() {
		return Err(AuthError::new(
			kind,
			format!("{failure}: {} {}", response.status, super::super::super::error::summarize_error_body(&response.body)),
		)
		.for_provider(flow.provider())
		.with_status(response.status));
	}
	let payload = response.json(flow.provider())?;
	map_token_response(&payload, flow.provider())
}

fn map_token_response(payload: &serde_json::Value, provider: &str) -> Result<Credential, AuthError> {
	let access = payload.get("access_token").and_then(serde_json::Value::as_str).unwrap_or_default();
	let refresh = payload.get("refresh_token").and_then(serde_json::Value::as_str).unwrap_or_default();
	let expires_in = payload.get("expires_in").and_then(serde_json::Value::as_i64);
	if access.is_empty() || refresh.is_empty() || expires_in.is_none() {
		return Err(AuthError::new(
			AuthErrorKind::Validation,
			"GitLab OAuth token response missing required fields",
		)
		.for_provider(provider));
	}
	let created_at_ms = payload.get("created_at").and_then(serde_json::Value::as_i64).map(|seconds| seconds * 1000);
	Ok(Credential::OAuth(OAuthCredentials {
		access: access.to_string(),
		refresh: refresh.to_string(),
		expires: created_at_ms.unwrap_or_else(now_ms) + expires_in.unwrap_or(0) * 1000 - EXPIRY_SKEW_MS,
		..OAuthCredentials::default()
	}))
}

/// `resolveClientId`: `GITLAB_CLIENT_ID` wins over the bundled app.
fn duo_client_id() -> String {
	std::env::var("GITLAB_CLIENT_ID")
		.ok()
		.map(|value| value.trim().to_string())
		.filter(|value| !value.is_empty())
		.unwrap_or_else(|| DUO_CLIENT_ID.to_string())
}

/// `resolveCallbackOptions`: honour `GITLAB_REDIRECT_URI` exactly, including its
/// loopback host/port, and reject an HTTPS loopback (the listener is plaintext).
fn duo_callback_config() -> Result<CallbackConfig, AuthError> {
	let raw = std::env::var("GITLAB_REDIRECT_URI").ok().map(|value| value.trim().to_string()).filter(|value| !value.is_empty());
	let Some(raw) = raw else {
		return Ok(CallbackConfig::new(DUO_CALLBACK_PORT, DUO_CALLBACK_PATH));
	};
	if !(raw.starts_with("http://") || raw.starts_with("https://")) {
		return Err(AuthError::config("gitlab-duo", format!("GITLAB_REDIRECT_URI must use http:// or https://, got: {raw}")));
	}
	let (scheme, rest) = raw.split_once("://").unwrap_or((&raw, ""));
	let authority = rest.split('/').next().unwrap_or("");
	let host = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
	let host = host.trim_start_matches('[').split(']').next().unwrap_or(host);
	let host = host.split(':').next().unwrap_or(host);
	let path = &rest[authority.len()..];
	let path = if path.is_empty() { DUO_CALLBACK_PATH } else { path };
	let is_loopback = matches!(host, "localhost" | "127.0.0.1" | "::1");
	if is_loopback && scheme != "http" {
		return Err(AuthError::config(
			"gitlab-duo",
			format!("GITLAB_REDIRECT_URI loopback callbacks must use http://, got: {raw}"),
		));
	}
	let port = authority
		.rsplit_once(':')
		.and_then(|(_, port)| port.parse::<u16>().ok())
		.unwrap_or(if scheme == "https" { 443 } else { 80 });
	let config = CallbackConfig::new(if is_loopback { port } else { 0 }, path).with_redirect_uri(&raw);
	Ok(if is_loopback { config.with_hostname(host) } else { config })
}

fn now_ms() -> i64 {
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
		.unwrap_or(0)
}

/// Instruction text embeds the redirect contract; used by the paste hint.
#[must_use]
pub fn workflow_redirect_hint() -> String {
	format!("{WORKFLOW_REDIRECT_URI}?code={}", encode_component("…"))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn maps_created_at_seconds_into_expiry_with_skew() {
		let payload = json!({ "access_token": "a", "refresh_token": "r", "expires_in": 3600, "created_at": 1000 });
		let Credential::OAuth(creds) = map_token_response(&payload, "gitlab-duo").expect("mapped") else {
			panic!("oauth expected");
		};
		assert_eq!(creds.expires, 1000 * 1000 + 3600 * 1000 - EXPIRY_SKEW_MS);
	}

	#[test]
	fn rejects_incomplete_token_response() {
		let payload = json!({ "access_token": "a" });
		assert_eq!(
			map_token_response(&payload, "gitlab-duo").expect_err("missing fields").kind,
			AuthErrorKind::Validation
		);
	}

	#[test]
	fn workflow_redirect_is_vscode_scheme() {
		assert!(WORKFLOW_REDIRECT_URI.starts_with("vscode://gitlab.gitlab-workflow/"));
		assert!(workflow_redirect_hint().contains("vscode://"));
	}

	#[test]
	fn default_callback_matches_registered_app() {
		// Without the env override the bundled app's registered redirect is used.
		if std::env::var("GITLAB_REDIRECT_URI").is_err() {
			let config = duo_callback_config().expect("default config");
			assert_eq!(config.preferred_port, 8080);
			assert_eq!(config.callback_path, "/callback");
			assert!(config.redirect_uri.is_none());
		}
	}
}
