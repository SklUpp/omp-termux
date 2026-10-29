//! GitHub Copilot device login (`registry/oauth/github-copilot.ts`).
//!
//! GitHub's device flow with a pasted-optional enterprise domain, JSON bodies
//! (`Accept: application/json`), a 1.2× growth factor per pending poll and a
//! 1.4× factor after `slow_down`. The GitHub token doubles as `access` and
//! `refresh` (there is no refresh grant — `refreshGitHubCopilotToken` just
//! re-stamps the same token with a far-future expiry).

use std::time::Duration;

use serde_json::{Value, json};

use super::super::super::error::{AuthError, AuthErrorKind};
use super::super::super::http::HttpRequest;
use super::super::super::store::{Credential, OAuthCredentials};
use super::super::Flow;
use super::{now_ms, publish, sleep, string_field};

const CLIENT_ID: &str = "Ov23li8tweQw6odWQebz";
const SCOPE: &str = "read:user";
/// Copilot endpoints identify as the opencode client.
const USER_AGENT: &str = "opencode/1.3.15";
/// `COPILOT_API_VERSION` — sent on `api.githubcopilot.com` requests (never on
/// `api.github.com` REST endpoints, which validate a different vocabulary).
const COPILOT_API_VERSION: &str = "2026-06-01";
const INITIAL_POLL_MULTIPLIER: f64 = 1.2;
const SLOW_DOWN_POLL_MULTIPLIER: f64 = 1.4;
const POLL_FLOOR_MS: f64 = 1000.0;
const POLL_SCALE_MS: f64 = 1000.0;
const SLOW_DOWN_INCREMENT_SECONDS: f64 = 5.0;
/// `FAR_FUTURE_MS`: ten years, mirroring `refreshGitHubCopilotToken`.
const FAR_FUTURE_MS: i64 = 10 * 365 * 24 * 60 * 60 * 1000;
const PROVIDER: &str = "github-copilot";

pub async fn login(flow: &Flow<'_>) -> Result<Credential, AuthError> {
	let input = flow.ask("GitHub Enterprise URL/domain (blank for github.com)", "company.ghe.com", true)?;
	let domain = normalize_domain(&input)?;
	let enterprise_domain = normalize_enterprise_domain(&input, domain.as_deref());
	let host = domain.filter(|host| !is_public_github_host(host)).unwrap_or_else(|| "github.com".to_string());

	let device = start_device_flow(flow, &host).await?;
	publish(flow, &device.verification_uri, &device.user_code);
	let github_token = poll_access_token(flow, &host, &device).await?;

	let api_endpoint = discover_api_endpoint(flow, &github_token).await?;
	Ok(Credential::OAuth(OAuthCredentials {
		access: github_token.clone(),
		refresh: github_token,
		expires: now_ms() + FAR_FUTURE_MS,
		enterprise_url: enterprise_domain,
		api_endpoint,
		..OAuthCredentials::default()
	}))
}

/// `refreshGitHubCopilotToken`: there is no refresh grant, so the stored GitHub
/// token is re-stamped with a far-future expiry and the discovered enterprise
/// metadata is preserved.
pub async fn refresh(
	flow: &Flow<'_>,
	credentials: &OAuthCredentials,
) -> Result<Credential, AuthError> {
	let provider = flow.provider();
	if credentials.access.trim().is_empty() {
		return Err(AuthError::new(AuthErrorKind::TokenRefresh, "stored credential has no GitHub token")
			.for_provider(provider));
	}
	Ok(Credential::OAuth(OAuthCredentials {
		expires: now_ms() + FAR_FUTURE_MS,
		access: credentials.access.clone(),
		refresh: credentials.access.clone(),
		..credentials.clone()
	}))
}

struct DeviceCode {
	device_code: String,
	user_code: String,
	verification_uri: String,
	interval_seconds: f64,
	expires_in: f64,
}

async fn start_device_flow(flow: &Flow<'_>, host: &str) -> Result<DeviceCode, AuthError> {
	let request = HttpRequest::json(
		format!("https://{host}/login/device/code"),
		json!({ "client_id": CLIENT_ID, "scope": SCOPE }),
	)
	.header("Accept", "application/json")
	.header("User-Agent", USER_AGENT);
	let response = flow.http.send(request, flow.cancel, PROVIDER).await?;
	if !response.is_success() {
		return Err(response.error_for(AuthErrorKind::DeviceAuth, PROVIDER, "Device code request"));
	}
	let payload = response.json(PROVIDER)?;
	let device_code = string_field(&payload, "device_code").unwrap_or_default();
	let user_code = string_field(&payload, "user_code").unwrap_or_default();
	let verification_uri = string_field(&payload, "verification_uri").unwrap_or_default();
	let interval = payload.get("interval").and_then(Value::as_f64);
	let expires_in = payload.get("expires_in").and_then(Value::as_f64);
	if device_code.is_empty() || user_code.is_empty() || verification_uri.is_empty() || interval.is_none() || expires_in.is_none()
	{
		return Err(AuthError::new(AuthErrorKind::Validation, "Invalid device code response fields")
			.for_provider(PROVIDER));
	}
	Ok(DeviceCode {
		device_code,
		user_code,
		verification_uri,
		interval_seconds: interval.unwrap_or(5.0),
		expires_in: expires_in.unwrap_or(900.0),
	})
}

async fn poll_access_token(flow: &Flow<'_>, host: &str, device: &DeviceCode) -> Result<String, AuthError> {
	let deadline = Duration::from_millis((device.expires_in.max(0.0) * 1000.0) as u64);
	let started = std::time::Instant::now();
	let mut interval_ms = (device.interval_seconds * POLL_SCALE_MS).max(POLL_FLOOR_MS);
	let mut multiplier = INITIAL_POLL_MULTIPLIER;
	let mut slow_downs = 0u32;

	while started.elapsed() < deadline {
		let remaining = deadline.saturating_sub(started.elapsed());
		let wait = Duration::from_millis((interval_ms * multiplier).ceil() as u64).min(remaining);
		sleep(flow, wait).await?;

		let request = HttpRequest::json(
			format!("https://{host}/login/oauth/access_token"),
			json!({
				"client_id": CLIENT_ID,
				"device_code": device.device_code,
				"grant_type": "urn:ietf:params:oauth:grant-type:device_code",
			}),
		)
		.header("Accept", "application/json")
		.header("User-Agent", USER_AGENT);
		let response = flow.http.send(request, flow.cancel, PROVIDER).await?;
		if !response.is_success() {
			return Err(response.error_for(AuthErrorKind::Polling, PROVIDER, "Device token request"));
		}
		let payload = response.json(PROVIDER)?;
		if let Some(token) = string_field(&payload, "access_token") {
			if !token.is_empty() {
				return Ok(token);
			}
		}
		match string_field(&payload, "error").as_deref().unwrap_or_default() {
			"authorization_pending" => {},
			"slow_down" => {
				slow_downs += 1;
				let retry = payload.get("interval").and_then(Value::as_f64).filter(|seconds| *seconds > 0.0);
				interval_ms = match retry {
					Some(seconds) => (seconds * POLL_SCALE_MS).max(POLL_FLOOR_MS),
					None => (interval_ms + SLOW_DOWN_INCREMENT_SECONDS * POLL_SCALE_MS).max(POLL_FLOOR_MS),
				};
				multiplier = SLOW_DOWN_POLL_MULTIPLIER;
			},
			error => {
				return Err(super::classify_poll_error(PROVIDER, error, response.status));
			},
		}
	}

	if slow_downs > 0 {
		return Err(AuthError::new(
			AuthErrorKind::Timeout,
			"Device flow timed out after one or more slow_down responses. This is often caused by clock drift in WSL or VM environments. Please sync or restart the VM clock and try again.",
		)
		.for_provider(PROVIDER));
	}
	Err(AuthError::new(AuthErrorKind::Timeout, "Device flow timed out").for_provider(PROVIDER))
}

/// `discoverGitHubCopilotApiEndpoint`: best effort; a failure keeps `None` so
/// the transport falls back to the canonical host, but a cancelled login stays
/// cancelled instead of persisting success.
async fn discover_api_endpoint(flow: &Flow<'_>, token: &str) -> Result<Option<String>, AuthError> {
	let request = HttpRequest::get("https://api.github.com/copilot_internal/user")
		.header("Accept", "application/json")
		.header("Authorization", format!("token {token}"))
		.header("User-Agent", USER_AGENT);
	let response = match flow.http.send(request, flow.cancel, PROVIDER).await {
		Ok(response) => response,
		Err(err) if err.kind == AuthErrorKind::Cancelled => return Err(err),
		Err(_) => return Ok(None),
	};
	if !response.is_success() {
		return Ok(None);
	}
	let Ok(payload) = response.json(PROVIDER) else { return Ok(None) };
	let Some(endpoint) = payload.get("endpoints").and_then(|endpoints| endpoints.get("api")).and_then(Value::as_str)
	else {
		return Ok(None);
	};
	Ok(normalize_api_endpoint(endpoint))
}

fn normalize_api_endpoint(input: &str) -> Option<String> {
	let trimmed = input.trim();
	if !trimmed.starts_with("https://") {
		return None;
	}
	let host = trimmed.trim_start_matches("https://").split('/').next()?;
	if host.is_empty() {
		return None;
	}
	Some(trimmed.trim_end_matches('/').to_string())
}

/// `getGitHubCopilotBaseUrl`: enterprise-aware capi host (`copilot-api.<domain>`).
#[must_use]
pub fn copilot_base_url(enterprise_domain: Option<&str>) -> String {
	const PUBLIC: &str = "https://api.githubcopilot.com";
	let Some(input) = enterprise_domain.map(str::trim).filter(|value| !value.is_empty()) else {
		return PUBLIC.to_string();
	};
	let normalized = normalize_domain(input).ok().flatten().unwrap_or_else(|| input.to_lowercase());
	if normalized.is_empty() || is_public_github_host(&normalized) {
		return PUBLIC.to_string();
	}
	if normalized.starts_with("copilot-api.") {
		format!("https://{normalized}")
	} else {
		format!("https://copilot-api.{normalized}")
	}
}

/// `enableAllGitHubCopilotModels`: accept the model policy (`POST
/// /models/<id>/policy {"state":"enabled"}`) for every wire model id, five at a
/// time, reporting each result. A failed or refused model is `false`, never an
/// error: policy acceptance is best effort and must not fail a login.
pub async fn enable_all_models(
	flow: &Flow<'_>,
	token: &str,
	enterprise_domain: Option<&str>,
	api_endpoint: Option<&str>,
	wire_model_ids: &[String],
	mut on_progress: impl FnMut(&str, bool),
) -> Vec<(String, bool)> {
	let mut results = Vec::with_capacity(wire_model_ids.len());
	let base_url = api_endpoint
		.map(ToString::to_string)
		.unwrap_or_else(|| copilot_base_url(enterprise_domain));
	for batch in wire_model_ids.chunks(5) {
		if flow.check().is_err() {
			break;
		}
		let mut set = tokio::task::JoinSet::new();
		for model_id in batch {
			let url = format!("{base_url}/models/{model_id}/policy");
			let request = HttpRequest::json(url, json!({ "state": "enabled" }))
				.header("Accept", "application/json")
				.header("Content-Type", "application/json")
				.header("User-Agent", USER_AGENT)
				.header("X-GitHub-Api-Version", COPILOT_API_VERSION)
				.header("openai-intent", "chat-policy")
				.header("x-interaction-type", "chat-policy")
				.bearer(token);
			let http = flow.http.clone();
			let cancel = flow.cancel.clone();
			let model_id = model_id.clone();
			set.spawn(async move {
				let success = http
					.send(request, &cancel, PROVIDER)
					.await
					.map(|response| response.is_success())
					.unwrap_or(false);
				(model_id, success)
			});
		}
		while let Some(joined) = set.join_next().await {
			if let Ok((model_id, success)) = joined {
				on_progress(&model_id, success);
				results.push((model_id, success));
			}
		}
	}
	results
}

fn normalize_domain(input: &str) -> Result<Option<String>, AuthError> {
	let trimmed = input.trim();
	if trimmed.is_empty() {
		return Ok(None);
	}
	let candidate = if trimmed.contains("://") {
		trimmed.to_string()
	} else {
		format!("https://{trimmed}")
	};
	let host = candidate
		.trim_start_matches("https://")
		.trim_start_matches("http://")
		.split('/')
		.next()
		.unwrap_or_default()
		.to_ascii_lowercase();
	if host.is_empty() || !host.contains('.') && host != "localhost" && !host.contains(':') {
		return Err(AuthError::new(AuthErrorKind::Validation, "Invalid GitHub Enterprise URL/domain")
			.for_provider(PROVIDER));
	}
	Ok(Some(host))
}

fn is_public_github_host(host: &str) -> bool {
	matches!(host, "api.github.com" | "github.com" | "www.github.com")
}

fn normalize_enterprise_domain(input: &str, normalized: Option<&str>) -> Option<String> {
	if input.trim().is_empty() {
		return None;
	}
	let domain = normalized.map_or_else(|| input.trim().to_ascii_lowercase(), ToString::to_string);
	if domain.is_empty() || is_public_github_host(&domain) {
		return None;
	}
	Some(domain)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn domain_normalization_matches_upstream() {
		assert_eq!(normalize_domain("").expect("ok"), None);
		assert_eq!(normalize_domain("company.ghe.com").expect("ok").as_deref(), Some("company.ghe.com"));
		assert_eq!(
			normalize_domain("https://company.ghe.com/login").expect("ok").as_deref(),
			Some("company.ghe.com")
		);
		assert!(normalize_domain("not a domain").is_err());
	}

	#[test]
	fn enterprise_domain_ignores_public_hosts() {
		assert_eq!(normalize_enterprise_domain("github.com", Some("github.com")), None);
		assert_eq!(
			normalize_enterprise_domain("company.ghe.com", Some("company.ghe.com")).as_deref(),
			Some("company.ghe.com")
		);
		assert_eq!(normalize_enterprise_domain("", None), None);
	}

	#[test]
	fn api_endpoint_requires_https_and_trims_slash() {
		assert_eq!(
			normalize_api_endpoint("https://api.individual.githubcopilot.com/").as_deref(),
			Some("https://api.individual.githubcopilot.com")
		);
		assert_eq!(normalize_api_endpoint("http://insecure.test"), None);
		assert_eq!(normalize_api_endpoint("https://"), None);
	}

	#[test]
	fn token_is_its_own_refresh_grant() {
		// `refreshGitHubCopilotToken` re-stamps the same value with a far-future expiry.
		assert_eq!(FAR_FUTURE_MS, 315_360_000_000);
	}

	#[test]
	fn base_url_tracks_enterprise_domains() {
		assert_eq!(copilot_base_url(None), "https://api.githubcopilot.com");
		assert_eq!(copilot_base_url(Some("")), "https://api.githubcopilot.com");
		assert_eq!(copilot_base_url(Some("github.com")), "https://api.githubcopilot.com");
		assert_eq!(copilot_base_url(Some("api.github.com")), "https://api.githubcopilot.com");
		assert_eq!(copilot_base_url(Some("company.ghe.com")), "https://copilot-api.company.ghe.com");
		assert_eq!(
			copilot_base_url(Some("https://copilot-api.company.ghe.com/")),
			"https://copilot-api.company.ghe.com"
		);
	}
}
