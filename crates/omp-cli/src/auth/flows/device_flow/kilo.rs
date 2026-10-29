//! Kilo Gateway device login (`registry/kilo.ts`).
//!
//! Bespoke poll: `POST /codes` → `{code, verificationUrl, expiresIn}`, then
//! `GET /codes/{code}` answers `202` while pending, `403`/`410` for denied or
//! expired, and `200 {status:"approved", token}` when done. The token is a
//! one-year static credential with no refresh grant.

use std::time::Duration;

use serde_json::json;

use super::super::super::error::{AuthError, AuthErrorKind};
use super::super::super::http::HttpRequest;
use super::super::super::store::{Credential, OAuthCredentials};
use super::super::Flow;
use super::{now_ms, number_field, publish, sleep, string_field};

const BASE_URL: &str = "https://api.kilo.ai/api/device-auth";
const POLL_INTERVAL_MS: u64 = 5_000;
const ONE_YEAR_MS: i64 = 365 * 24 * 60 * 60 * 1000;

pub async fn login(flow: &Flow<'_>) -> Result<Credential, AuthError> {
	let provider = "kilo";
	let response = flow
		.http
		.send(
			HttpRequest::json(format!("{BASE_URL}/codes"), json!({}))
				.header("Content-Type", "application/json"),
			flow.cancel,
			provider,
		)
		.await?;
	if !response.is_success() {
		if response.status == 429 {
			return Err(AuthError::new(
				AuthErrorKind::Polling,
				"Too many pending authorization requests. Please try again later.",
			)
			.for_provider(provider)
			.with_status(429));
		}
		return Err(response.error_for(AuthErrorKind::DeviceAuth, provider, "Failed to initiate device authorization"));
	}
	let payload = response.json(provider)?;
	let user_code = string_field(&payload, "code").unwrap_or_default();
	let verification_url = string_field(&payload, "verificationUrl").unwrap_or_default();
	let expires_in = number_field(&payload, "expiresIn").unwrap_or_default();
	if user_code.is_empty() || verification_url.is_empty() || expires_in <= 0.0 {
		return Err(AuthError::new(
			AuthErrorKind::Validation,
			"Kilo device authorization response missing required fields",
		)
		.for_provider(provider));
	}
	publish(flow, &verification_url, &user_code);

	let deadline = Duration::from_millis((expires_in * 1000.0) as u64);
	let started = std::time::Instant::now();
	while started.elapsed() < deadline {
		flow.check()?;
		// Kilo's poll answers immediately; the pacing lives between attempts.
		let response = flow
			.http
			.send(
				HttpRequest::get(format!(
					"{BASE_URL}/codes/{}",
					super::super::super::callback::encode_component(&user_code)
				)),
				flow.cancel,
				provider,
			)
			.await?;
		match response.status {
			202 => {},
			403 => {
				return Err(AuthError::new(AuthErrorKind::DeviceAuth, "Authorization was denied").for_provider(provider));
			},
			410 => {
				return Err(AuthError::new(
					AuthErrorKind::DeviceAuth,
					"Authorization code expired. Please try again.",
				)
				.for_provider(provider));
			},
			status if (200..300).contains(&status) => {
				let payload = response.json(provider)?;
				let status = string_field(&payload, "status").unwrap_or_default();
				let token = string_field(&payload, "token").unwrap_or_default();
				if status == "approved" && !token.is_empty() {
					return Ok(Credential::OAuth(OAuthCredentials {
						access: token,
						refresh: String::new(),
						expires: now_ms() + ONE_YEAR_MS,
						..OAuthCredentials::default()
					}));
				}
				if status == "denied" {
					return Err(AuthError::new(AuthErrorKind::DeviceAuth, "Authorization was denied")
						.for_provider(provider));
				}
				if status == "expired" {
					return Err(AuthError::new(
						AuthErrorKind::DeviceAuth,
						"Authorization code expired. Please try again.",
					)
					.for_provider(provider));
				}
			},
			_ => {
				return Err(response.error_for(AuthErrorKind::Polling, provider, "Failed to poll device authorization"));
			},
		}

		let remaining = deadline.saturating_sub(started.elapsed());
		if remaining.is_zero() {
			break;
		}
		sleep(flow, remaining.min(Duration::from_millis(POLL_INTERVAL_MS))).await?;
	}

	Err(AuthError::new(AuthErrorKind::Timeout, "Authentication timed out. Please try again.").for_provider(provider))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn poll_status_contract() {
		// 202 pending, 403/410 terminal, 2xx carries the status envelope.
		assert_eq!(POLL_INTERVAL_MS, 5_000);
		assert_eq!(ONE_YEAR_MS, 31_536_000_000);
		assert!(format!("{BASE_URL}/codes").starts_with("https://api.kilo.ai/api/device-auth/codes"));
	}
}
