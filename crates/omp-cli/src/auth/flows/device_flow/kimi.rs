//! Kimi Code device login (`registry/oauth/kimi.ts`).
//!
//! RFC 8628 against `auth.kimi.com` (override: `KIMI_CODE_OAUTH_HOST` /
//! `KIMI_OAUTH_HOST`) with the Kimi CLI identity headers, a persisted device id
//! (`<agent dir>/kimi-device-id`, 0600) and a JWT-derived account id. Refresh
//! reuses the same form-encoded token endpoint.

use std::time::Duration;

use serde_json::Value;

use super::super::super::error::{AuthError, AuthErrorKind};
use super::super::super::http::HttpRequest;
use super::super::super::store::{Credential, OAuthCredentials};
use super::super::Flow;
use super::{jwt_claims, non_empty, number_field, publish, sleep, string_field};

const CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";
const DEFAULT_OAUTH_HOST: &str = "https://auth.kimi.com";
const DEVICE_ID_FILENAME: &str = "kimi-device-id";
const DEFAULT_POLL_INTERVAL_MS: u64 = 5_000;
const DEFAULT_DEVICE_FLOW_TTL_MS: u64 = 15 * 60 * 1000;
const EXPIRY_SKEW_MS: i64 = 5 * 60 * 1000;
const PROVIDER: &str = "kimi-code";

pub async fn login(flow: &Flow<'_>) -> Result<Credential, AuthError> {
	let host = oauth_host();
	let headers = common_headers();
	let request = form_request(
		format!("{host}/api/oauth/device_authorization"),
		vec![("client_id".to_string(), CLIENT_ID.to_string())],
		&headers,
	);
	let response = flow.http.send(request, flow.cancel, PROVIDER).await?;
	if !response.is_success() {
		return Err(response.error_for(AuthErrorKind::DeviceAuth, PROVIDER, "Kimi device authorization"));
	}
	let payload = response.json(PROVIDER)?;
	let user_code = string_field(&payload, "user_code").unwrap_or_default();
	let device_code = string_field(&payload, "device_code").unwrap_or_default();
	let verification_uri = string_field(&payload, "verification_uri").unwrap_or_default();
	if user_code.is_empty() || device_code.is_empty() || verification_uri.is_empty() {
		return Err(AuthError::new(
			AuthErrorKind::Validation,
			"Kimi device authorization response missing required fields",
		)
		.for_provider(PROVIDER));
	}
	let verification_complete = non_empty(string_field(&payload, "verification_uri_complete"))
		.unwrap_or_else(|| verification_uri.clone());
	let expires_ms = number_field(&payload, "expires_in")
		.map(|seconds| (seconds * 1000.0) as u64)
		.filter(|millis| *millis > 0)
		.unwrap_or(DEFAULT_DEVICE_FLOW_TTL_MS);
	let interval_ms = number_field(&payload, "interval")
		.map(|seconds| (seconds * 1000.0) as u64)
		.filter(|millis| *millis > 0)
		.unwrap_or(DEFAULT_POLL_INTERVAL_MS);

	publish(flow, &verification_complete, &user_code);
	flow.notice(format!("Waiting for browser authorization (code: {user_code})..."));

	let deadline = Duration::from_millis(expires_ms);
	let started = std::time::Instant::now();
	let mut wait_ms = interval_ms.max(1000);
	while started.elapsed() < deadline {
		flow.check()?;
		let request = form_request(
			format!("{host}/api/oauth/token"),
			vec![
				("client_id".to_string(), CLIENT_ID.to_string()),
				("device_code".to_string(), device_code.clone()),
				("grant_type".to_string(), "urn:ietf:params:oauth:grant-type:device_code".to_string()),
			],
			&headers,
		);
		let response = flow.http.send(request, flow.cancel, PROVIDER).await?;
		let payload = response.json(PROVIDER).unwrap_or(Value::Null);
		if response.is_success() && string_field(&payload, "access_token").is_some() {
			return Ok(Credential::OAuth(parse_tokens(&payload, None)?));
		}
		match string_field(&payload, "error").as_deref().unwrap_or_default() {
			"authorization_pending" => {},
			"slow_down" => {
				wait_ms += 5_000;
				if let Some(retry_ms) = number_field(&payload, "interval").map(|seconds| (seconds * 1000.0) as u64) {
					wait_ms = wait_ms.max(retry_ms);
				}
			},
			"expired_token" => {
				return Err(AuthError::new(AuthErrorKind::Validation, "Kimi device authorization expired")
					.for_provider(PROVIDER));
			},
			"access_denied" => {
				return Err(AuthError::new(AuthErrorKind::Validation, "Kimi device authorization denied")
					.for_provider(PROVIDER));
			},
			error => {
				return Err(super::classify_poll_error(PROVIDER, error, response.status));
			},
		}
		let remaining = deadline.saturating_sub(started.elapsed());
		if remaining.is_zero() {
			break;
		}
		sleep(flow, remaining.min(Duration::from_millis(wait_ms))).await?;
	}

	Err(AuthError::new(AuthErrorKind::Timeout, "Kimi device flow timed out").for_provider(PROVIDER))
}

pub async fn refresh(flow: &Flow<'_>, credentials: &OAuthCredentials) -> Result<Credential, AuthError> {
	if credentials.refresh.is_empty() {
		return Err(AuthError::new(AuthErrorKind::TokenRefresh, "stored credential has no refresh token")
			.for_provider(PROVIDER));
	}
	let host = oauth_host();
	let request = form_request(
		format!("{host}/api/oauth/token"),
		vec![
			("grant_type".to_string(), "refresh_token".to_string()),
			("refresh_token".to_string(), credentials.refresh.clone()),
			("client_id".to_string(), CLIENT_ID.to_string()),
		],
		&common_headers(),
	);
	let response = flow.http.send(request, flow.cancel, PROVIDER).await?;
	if !response.is_success() {
		return Err(response.error_for(AuthErrorKind::TokenRefresh, PROVIDER, "Kimi token refresh"));
	}
	let payload = response.json(PROVIDER)?;
	Ok(Credential::OAuth(parse_tokens(&payload, Some(&credentials.refresh))?))
}

fn parse_tokens(payload: &Value, refresh_fallback: Option<&str>) -> Result<OAuthCredentials, AuthError> {
	let access = string_field(payload, "access_token").unwrap_or_default();
	let expires_in = payload.get("expires_in").and_then(Value::as_i64);
	let (Some(expires_in), false) = (expires_in, access.is_empty()) else {
		return Err(AuthError::new(AuthErrorKind::Validation, "Kimi token response missing required fields")
			.for_provider(PROVIDER));
	};
	let refresh = non_empty(string_field(payload, "refresh_token"))
		.or_else(|| refresh_fallback.map(ToString::to_string))
		.ok_or_else(|| {
			AuthError::new(AuthErrorKind::Validation, "Kimi token response missing refresh token").for_provider(PROVIDER)
		})?;
	let claims = jwt_claims(&access);
	let account_id = claims
		.as_ref()
		.and_then(|claims| non_empty(string_field(claims, "user_id")).or_else(|| non_empty(string_field(claims, "sub"))));
	Ok(OAuthCredentials {
		access,
		refresh,
		expires: super::now_ms() + expires_in * 1000 - EXPIRY_SKEW_MS,
		account_id,
		..OAuthCredentials::default()
	})
}

fn oauth_host() -> String {
	for key in ["KIMI_CODE_OAUTH_HOST", "KIMI_OAUTH_HOST"] {
		if let Ok(value) = std::env::var(key) {
			let value = value.trim().trim_end_matches('/').to_string();
			if !value.is_empty() {
				return value;
			}
		}
	}
	DEFAULT_OAUTH_HOST.to_string()
}

fn form_request(url: String, fields: Vec<(String, String)>, headers: &[(String, String)]) -> HttpRequest {
	let mut request = HttpRequest::form(url, fields).header("Content-Type", "application/x-www-form-urlencoded");
	request.headers.extend(headers.iter().cloned());
	request
}

/// `getKimiCommonHeaders`: the CLI identity the backend keys rate limits on.
fn common_headers() -> Vec<(String, String)> {
	headers_with(&device_id())
}

/// Pure header builder (the device id comes from the caller so tests never
/// touch the agent directory).
fn headers_with(device_id: &str) -> Vec<(String, String)> {
	let version = env!("CARGO_PKG_VERSION");
	let release = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
	let os_version = sanitize(&release, "unknown");
	let model = sanitize(
		&format!("{} {} {}", os_label(), release.trim(), std::env::consts::ARCH),
		"unknown",
	);
	vec![
		("User-Agent".to_string(), format!("KimiCLI/{version}")),
		("X-Msh-Platform".to_string(), "kimi_cli".to_string()),
		("X-Msh-Version".to_string(), version.to_string()),
		("X-Msh-Device-Name".to_string(), sanitize(&hostname(), "unknown")),
		("X-Msh-Device-Model".to_string(), model),
		("X-Msh-Os-Version".to_string(), os_version),
		("X-Msh-Device-Id".to_string(), device_id.to_string()),
	]
}

fn os_label() -> &'static str {
	match std::env::consts::OS {
		"macos" => "macOS",
		"windows" => "Windows",
		other => other,
	}
}

fn hostname() -> String {
	std::env::var("HOSTNAME")
		.ok()
		.or_else(|| std::fs::read_to_string("/proc/sys/kernel/hostname").ok())
		.unwrap_or_default()
		.trim()
		.to_string()
}

/// Strip non-printable/non-ASCII bytes; header values must stay ASCII.
fn sanitize(value: &str, fallback: &str) -> String {
	let cleaned: String = value.chars().filter(|ch| ch.is_ascii() && !ch.is_ascii_control()).collect();
	let cleaned = cleaned.trim();
	if cleaned.is_empty() { fallback.to_string() } else { cleaned.to_string() }
}

/// Persisted device id (`crypto.randomUUID().replace(/-/g,"")`), 0600. A write
/// failure still returns an id for this login: the header is telemetry only.
fn device_id() -> String {
	let path = crate::dirs::agent_dir().join(DEVICE_ID_FILENAME);
	if let Ok(existing) = std::fs::read_to_string(&path) {
		let existing = existing.trim().to_string();
		if !existing.is_empty() {
			return existing;
		}
	}
	let generated = super::super::super::pkce::random_uuid_v4()
		.map(|uuid| uuid.replace('-', ""))
		.unwrap_or_default();
	if generated.is_empty() {
		return "unknown".to_string();
	}
	if let Some(parent) = path.parent() {
		let _ = std::fs::create_dir_all(parent);
	}
	let _ = write_private(&path, &format!("{generated}\n"));
	generated
}

#[cfg(unix)]
fn write_private(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
	use std::io::Write as _;
	use std::os::unix::fs::OpenOptionsExt as _;
	let mut file = std::fs::OpenOptions::new()
		.write(true)
		.create(true)
		.truncate(true)
		.mode(0o600)
		.open(path)?;
	file.write_all(contents.as_bytes())
}

#[cfg(not(unix))]
fn write_private(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
	std::fs::write(path, contents)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn tokens_require_refresh_and_expiry() {
		let missing_expiry = serde_json::json!({ "access_token": "a", "refresh_token": "r" });
		assert!(parse_tokens(&missing_expiry, None).is_err());

		let fallback = serde_json::json!({ "access_token": "a", "expires_in": 3600 });
		let parsed = parse_tokens(&fallback, Some("old-refresh")).expect("refresh fallback");
		assert_eq!(parsed.refresh, "old-refresh");
	}

	#[test]
	fn account_id_comes_from_jwt() {
		use base64::Engine as _;
		let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
		let access = format!(
			"h.{}.s",
			engine.encode(br#"{"user_id":"user-1","sub":"sub-1"}"#)
		);
		let payload = serde_json::json!({ "access_token": access, "refresh_token": "r", "expires_in": 60 });
		let parsed = parse_tokens(&payload, None).expect("parsed");
		assert_eq!(parsed.account_id.as_deref(), Some("user-1"));
	}

	#[test]
	fn headers_are_ascii_and_identify_the_cli() {
		let headers = headers_with("device-1");
		assert!(headers.iter().any(|(name, value)| name == "X-Msh-Platform" && value == "kimi_cli"));
		assert!(headers.iter().all(|(_, value)| value.is_ascii()));
		assert!(headers.iter().any(|(name, value)| name == "X-Msh-Device-Id" && value == "device-1"));
	}

	#[test]
	fn sanitize_strips_control_bytes() {
		assert_eq!(sanitize("ok\nname", "fallback"), "okname");
		assert_eq!(sanitize("\u{4}", "fallback"), "fallback");
	}
}
