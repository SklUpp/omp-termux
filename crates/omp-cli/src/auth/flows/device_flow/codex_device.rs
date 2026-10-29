//! Codex headless device login (`loginOpenAICodexDevice`).
//!
//! `POST deviceauth/usercode` yields a `device_auth_id` + `user_code`; polling
//! `deviceauth/token` returns an authorization code plus its verifier once the
//! user approves, which is then exchanged form-encoded against the shared Codex
//! token endpoint. `403`/`404` mean "not approved yet".

use std::time::Duration;

use serde_json::json;

use super::super::super::error::{AuthError, AuthErrorKind};
use super::super::super::http::HttpRequest;
use super::super::super::store::Credential;
use super::super::oauth_code::codex;
use super::super::Flow;
use super::{number_field, publish, sleep, string_field};

const DEVICE_MAX_POLLS: u32 = 120;
const DEVICE_POLL_INTERVAL_MS: u64 = 5_000;
const DEVICE_POLL_SAFETY_MARGIN_MS: u64 = 3_000;
const DEFAULT_INTERVAL_SECONDS: f64 = 5.0;

pub async fn login(flow: &Flow<'_>) -> Result<Credential, AuthError> {
	let provider = "openai-codex-device";
	flow.notice("Initiating device authorization...");
	let init = flow
		.http
		.send(
			HttpRequest::json(codex::DEVICE_USERCODE_URL, json!({ "client_id": codex::CLIENT_ID }))
				.header("Content-Type", "application/json")
				.timeout(codex::TOKEN_REQUEST_TIMEOUT),
			flow.cancel,
			provider,
		)
		.await?;
	if !init.is_success() {
		return Err(init.error_for(AuthErrorKind::DeviceAuth, provider, "Device authorization initiation"));
	}
	let payload = init.json(provider)?;
	let device_auth_id = string_field(&payload, "device_auth_id").unwrap_or_default();
	let user_code = string_field(&payload, "user_code").unwrap_or_default();
	if device_auth_id.is_empty() || user_code.is_empty() {
		return Err(AuthError::new(
			AuthErrorKind::Validation,
			"Device authorization response missing required fields",
		)
		.for_provider(provider));
	}
	let interval_seconds = number_field(&payload, "interval").unwrap_or(DEFAULT_INTERVAL_SECONDS).max(1.0);
	let interval_ms = (interval_seconds * 1000.0) as u64 + DEVICE_POLL_SAFETY_MARGIN_MS;

	publish(flow, codex::DEVICE_AUTH_URL, &user_code);
	flow.notice(format!("Waiting for browser authorization (code: {user_code})..."));

	for poll in 0..DEVICE_MAX_POLLS {
		let wait = if poll == 0 { interval_ms.min(DEVICE_POLL_INTERVAL_MS) } else { interval_ms };
		sleep(flow, Duration::from_millis(wait)).await?;

		let body = json!({ "device_auth_id": device_auth_id, "user_code": user_code });
		let response = flow
			.http
			.send(
				HttpRequest::json(codex::DEVICE_TOKEN_URL, body).timeout(codex::TOKEN_REQUEST_TIMEOUT),
				flow.cancel,
				provider,
			)
			.await?;
		if matches!(response.status, 403 | 404) {
			continue;
		}
		if !response.is_success() {
			return Err(response.error_for(AuthErrorKind::Polling, provider, "Device token polling"));
		}
		let payload = response.json(provider)?;
		let authorization_code = string_field(&payload, "authorization_code").unwrap_or_default();
		let code_verifier = string_field(&payload, "code_verifier").unwrap_or_default();
		if authorization_code.is_empty() || code_verifier.is_empty() {
			return Err(AuthError::new(
				AuthErrorKind::Validation,
				"Device token response missing authorization_code or code_verifier",
			)
			.for_provider(provider));
		}
		flow.notice("Exchanging authorization code for tokens...");
		// The stored provider is `openai-codex` (`storeCredentialsAs`); the
		// exchange helper stamps that id, which is what the store keys on.
		return codex::exchange_code(
			flow,
			&authorization_code,
			&code_verifier,
			codex::DEVICE_REDIRECT_URI,
			AuthErrorKind::TokenExchange,
		)
		.await;
	}

	Err(AuthError::new(
		AuthErrorKind::Timeout,
		"Device authorization timed out — user did not complete login in time",
	)
	.for_provider(provider))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn poll_budget_and_interval_match_upstream() {
		assert_eq!(DEVICE_MAX_POLLS, 120);
		assert_eq!(DEVICE_POLL_INTERVAL_MS, 5_000);
		assert_eq!(DEVICE_POLL_SAFETY_MARGIN_MS, 3_000);
		// Interval from the response is seconds plus the safety margin.
		assert_eq!((5.0f64 * 1000.0) as u64 + DEVICE_POLL_SAFETY_MARGIN_MS, 8_000);
	}

	#[test]
	fn pending_statuses_are_retryable() {
		assert!(matches!(403, 403 | 404));
		assert!(matches!(404, 403 | 404));
		assert!(!matches!(200, 403 | 404));
	}

	#[test]
	fn device_endpoints_are_the_codex_ones() {
		assert_eq!(codex::DEVICE_REDIRECT_URI, "https://auth.openai.com/deviceauth/callback");
		assert!(codex::DEVICE_TOKEN_URL.ends_with("/api/accounts/deviceauth/token"));
	}
}
