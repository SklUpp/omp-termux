//! xAI Grok OAuth (`registry/oauth/xai-oauth.ts`).
//!
//! OIDC discovery → RFC 8628 device authorization → form-encoded token poll,
//! then a best-effort `userinfo` identity. Both the token endpoint (from
//! discovery) and the verification URLs are validated to `x.ai`/`.x.ai` before
//! use, so a poisoned discovery document cannot redirect the credential.

use serde_json::Value;

use super::super::super::error::{AuthError, AuthErrorKind};
use super::super::super::http::{HttpRequest, HttpResponse};
use super::super::super::store::{Credential, OAuthCredentials};
use super::super::super::device::{DevicePoll, poll_device_code};
use super::super::Flow;
use super::{jwt_claims, non_empty, now_ms, number_field, publish, string_field};

const ISSUER: &str = "https://auth.x.ai";
const DISCOVERY_URL: &str = "https://auth.x.ai/.well-known/openid-configuration";
const DEVICE_CODE_URL: &str = "https://auth.x.ai/oauth2/device/code";
const USERINFO_URL: &str = "https://auth.x.ai/oauth2/userinfo";
const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
const GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";
const EXPIRY_SKEW_MS: i64 = 5 * 60 * 1000;
const PROVIDER: &str = "xai";

pub async fn login(flow: &Flow<'_>) -> Result<Credential, AuthError> {
	let token_endpoint = discover_token_endpoint(flow).await?;
	let device = request_device_authorization(flow).await?;
	publish(flow, &device.verification_uri_complete, &device.user_code);
	flow.notice("Waiting for xAI device authorization...");

	let credentials = poll_device_code(
		PROVIDER,
		Some(device.interval_seconds),
		Some(device.expires_in_seconds),
		flow.cancel,
		|| poll_token(flow, &token_endpoint, &device.device_code),
	)
	.await?;
	let credentials = with_identity(flow, credentials).await?;
	Ok(Credential::OAuth(credentials))
}

pub async fn refresh(flow: &Flow<'_>, credentials: &OAuthCredentials) -> Result<Credential, AuthError> {
	if credentials.refresh.trim().is_empty() {
		return Err(AuthError::new(AuthErrorKind::Validation, "missing refresh_token").for_provider(PROVIDER));
	}
	let token_endpoint = discover_token_endpoint(flow).await?;
	let request = form(
		&token_endpoint,
		vec![
			("grant_type".to_string(), "refresh_token".to_string()),
			("client_id".to_string(), CLIENT_ID.to_string()),
			("refresh_token".to_string(), credentials.refresh.clone()),
		],
	);
	let response = flow.http.send(request, flow.cancel, PROVIDER).await?;
	if !response.is_success() {
		return Err(response.error_for(AuthErrorKind::TokenRefresh, PROVIDER, "xAI token refresh"));
	}
	let payload = response.json(PROVIDER)?;
	let refreshed = parse_tokens(&payload, "xAI token refresh response", Some(&credentials.refresh))?;
	Ok(Credential::OAuth(with_identity(flow, refreshed).await?))
}

struct DeviceAuthorization {
	device_code: String,
	user_code: String,
	verification_uri_complete: String,
	expires_in_seconds: u64,
	interval_seconds: f64,
}

async fn discover_token_endpoint(flow: &Flow<'_>) -> Result<String, AuthError> {
	let request = HttpRequest::get(DISCOVERY_URL).header("Accept", "application/json");
	let response = flow.http.send(request, flow.cancel, PROVIDER).await.map_err(|err| {
		AuthError::new(AuthErrorKind::Discovery, format!("xAI OIDC discovery failed: {}", err.message))
			.for_provider(PROVIDER)
	})?;
	if response.status != 200 {
		return Err(AuthError::new(
			AuthErrorKind::Discovery,
			format!("xAI OIDC discovery returned status {}.", response.status),
		)
		.for_provider(PROVIDER)
		.with_status(response.status));
	}
	let payload = response.json(PROVIDER)?;
	let endpoint = string_field(&payload, "token_endpoint").unwrap_or_default();
	if endpoint.trim().is_empty() {
		return Err(AuthError::new(
			AuthErrorKind::Validation,
			"xAI OIDC discovery response was missing token_endpoint.",
		)
		.for_provider(PROVIDER));
	}
	validate_xai_endpoint(endpoint.trim(), "token_endpoint")
}

/// Only `https://x.ai` / `https://*.x.ai` are acceptable for xAI endpoints.
fn validate_xai_endpoint(url: &str, field: &str) -> Result<String, AuthError> {
	let invalid = || {
		AuthError::new(AuthErrorKind::Validation, format!("Invalid xAI {field}: {url}")).for_provider(PROVIDER)
	};
	let rest = url.strip_prefix("https://").ok_or_else(invalid)?;
	let host = rest.split(['/', '?', '#']).next().unwrap_or_default().to_ascii_lowercase();
	let host = host.split(':').next().unwrap_or_default();
	if host.is_empty() || !(host == "x.ai" || host.ends_with(".x.ai")) {
		return Err(invalid());
	}
	Ok(url.to_string())
}

async fn request_device_authorization(flow: &Flow<'_>) -> Result<DeviceAuthorization, AuthError> {
	let request = form(
		DEVICE_CODE_URL,
		vec![("client_id".to_string(), CLIENT_ID.to_string()), ("scope".to_string(), SCOPE.to_string())],
	);
	let response = flow.http.send(request, flow.cancel, PROVIDER).await?;
	if !response.is_success() {
		return Err(response.error_for(AuthErrorKind::DeviceAuth, PROVIDER, "xAI device-code request"));
	}
	let payload = response.json(PROVIDER)?;
	let device_code = string_field(&payload, "device_code").unwrap_or_default().trim().to_string();
	let user_code = string_field(&payload, "user_code").unwrap_or_default().trim().to_string();
	let verification_uri = string_field(&payload, "verification_uri").unwrap_or_default().trim().to_string();
	let complete = string_field(&payload, "verification_uri_complete").unwrap_or_default().trim().to_string();
	let expires_in = number_field(&payload, "expires_in").unwrap_or_default();
	let interval = number_field(&payload, "interval").unwrap_or_default();
	if device_code.is_empty() || user_code.is_empty() || verification_uri.is_empty() || complete.is_empty() || expires_in <= 0.0
		|| interval <= 0.0
	{
		return Err(AuthError::new(
			AuthErrorKind::Validation,
			"xAI device-code response missing or invalid required fields.",
		)
		.for_provider(PROVIDER));
	}
	validate_xai_endpoint(&verification_uri, "verification_uri")?;
	validate_xai_endpoint(&complete, "verification_uri_complete")?;
	Ok(DeviceAuthorization {
		device_code,
		user_code,
		verification_uri_complete: complete,
		expires_in_seconds: (expires_in.max(1.0)) as u64,
		interval_seconds: interval,
	})
}

fn form(url: &str, fields: Vec<(String, String)>) -> HttpRequest {
	HttpRequest::form(url, fields)
		.header("Content-Type", "application/x-www-form-urlencoded")
		.header("Accept", "application/json")
}

async fn poll_token(
	flow: &Flow<'_>,
	token_endpoint: &str,
	device_code: &str,
) -> Result<DevicePoll<OAuthCredentials>, AuthError> {
	let request = form(
		token_endpoint,
		vec![
			("grant_type".to_string(), GRANT_TYPE.to_string()),
			("client_id".to_string(), CLIENT_ID.to_string()),
			("device_code".to_string(), device_code.to_string()),
		],
	);
	let response = flow.http.send(request, flow.cancel, PROVIDER).await?;
	let payload = response.json(PROVIDER).unwrap_or(Value::Null);
	if response.is_success() {
		return parse_tokens(&payload, "xAI device-code token response", None).map(DevicePoll::Complete);
	}
	match string_field(&payload, "error").as_deref().unwrap_or_default() {
		"authorization_pending" => Ok(DevicePoll::Pending),
		"slow_down" => Ok(DevicePoll::SlowDown),
		error => {
			let detail = crate::auth::error::oauth_error_identifier(error)
				.unwrap_or_else(|| "provider rejected the device request".to_string());
			Err(AuthError::new(
				AuthErrorKind::Polling,
				format!("xAI device-code token polling failed: {detail}"),
			)
			.for_provider(PROVIDER)
			.with_status(response.status))
		},
	}
}

fn parse_tokens(payload: &Value, label: &str, refresh_fallback: Option<&str>) -> Result<OAuthCredentials, AuthError> {
	let access = string_field(payload, "access_token").unwrap_or_default();
	let refresh = non_empty(string_field(payload, "refresh_token"))
		.or_else(|| refresh_fallback.map(ToString::to_string))
		.unwrap_or_default();
	let expires_in = payload.get("expires_in").and_then(Value::as_i64);
	if access.is_empty() {
		return Err(AuthError::new(AuthErrorKind::Validation, format!("{label} missing access_token"))
			.for_provider(PROVIDER));
	}
	if refresh.is_empty() {
		return Err(AuthError::new(AuthErrorKind::Validation, format!("{label} missing refresh_token"))
			.for_provider(PROVIDER));
	}
	let Some(expires_in) = expires_in else {
		return Err(AuthError::new(AuthErrorKind::Validation, format!("{label} missing expires_in"))
			.for_provider(PROVIDER));
	};
	Ok(OAuthCredentials {
		access,
		refresh,
		expires: now_ms() + expires_in * 1000 - EXPIRY_SKEW_MS,
		..OAuthCredentials::default()
	})
}

/// `fetchXAIOAuthIdentity`: a failed userinfo call keeps the JWT subject, but a
/// cancelled login propagates instead of completing with partial identity.
async fn with_identity(flow: &Flow<'_>, mut credentials: OAuthCredentials) -> Result<OAuthCredentials, AuthError> {
	if let Some(identity) = fetch_identity(flow, &credentials.access).await? {
		credentials.account_id = non_empty(identity.0).or(credentials.account_id);
		credentials.email = non_empty(identity.1).map(|email| email.to_lowercase()).or(credentials.email);
	}
	if credentials.account_id.is_none() {
		credentials.account_id = jwt_claims(&credentials.access)
			.and_then(|claims| non_empty(string_field(&claims, "sub")));
	}
	Ok(credentials)
}

async fn fetch_identity(
	flow: &Flow<'_>,
	access: &str,
) -> Result<Option<(Option<String>, Option<String>)>, AuthError> {
	let request = HttpRequest::get(USERINFO_URL)
		.bearer(access)
		.header("Accept", "application/json");
	let response: HttpResponse = match flow.http.send(request, flow.cancel, PROVIDER).await {
		Ok(response) => response,
		Err(err) if err.kind == AuthErrorKind::Cancelled => return Err(err),
		Err(_) => return Ok(None),
	};
	if !response.is_success() {
		return Ok(None);
	}
	let Ok(payload) = response.json(PROVIDER) else { return Ok(None) };
	let subject = non_empty(string_field(&payload, "sub"));
	let email = non_empty(string_field(&payload, "email"));
	if subject.is_none() && email.is_none() {
		return Ok(None);
	}
	Ok(Some((subject, email)))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn endpoints_must_stay_on_xai() {
		assert!(validate_xai_endpoint("https://auth.x.ai/oauth2/token", "token_endpoint").is_ok());
		assert!(validate_xai_endpoint("https://x.ai/token", "token_endpoint").is_ok());
		assert!(validate_xai_endpoint("http://auth.x.ai/token", "token_endpoint").is_err());
		assert!(validate_xai_endpoint("https://evil.test/token", "token_endpoint").is_err());
		assert!(validate_xai_endpoint("https://auth.x.ai.evil.test/token", "token_endpoint").is_err());
	}

	#[test]
	fn tokens_require_expiry_and_refresh() {
		let missing_refresh = serde_json::json!({ "access_token": "a", "expires_in": 60 });
		assert!(parse_tokens(&missing_refresh, "label", None).is_err());
		let fallback = serde_json::json!({ "access_token": "a", "expires_in": 60 });
		assert_eq!(parse_tokens(&fallback, "label", Some("r")).expect("refresh").refresh, "r");
		let missing_expiry = serde_json::json!({ "access_token": "a", "refresh_token": "r" });
		assert!(parse_tokens(&missing_expiry, "label", None).is_err());
	}

	#[test]
	fn discovery_and_device_urls_are_pinned() {
		assert!(DISCOVERY_URL.starts_with(ISSUER));
		assert!(DEVICE_CODE_URL.starts_with(ISSUER));
		assert_eq!(SCOPE.split(' ').count(), 6);
	}
}
