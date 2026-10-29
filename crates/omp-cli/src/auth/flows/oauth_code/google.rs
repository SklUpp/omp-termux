//! Google Cloud Code Assist logins (`registry/oauth/google-oauth-shared.ts`,
//! `google-gemini-cli.ts`, `google-antigravity.ts`).
//!
//! Both providers share one authorization-code shape (offline access,
//! `prompt=consent`, loopback callback on `127.0.0.1`, form-encoded token
//! exchange) and differ only in client ids, scopes and project discovery.
//! A login is only complete once a `cloudaicompanionProject` is known, so the
//! provisioning phase is part of `exchange`, not a later best-effort step.

use serde_json::{Value, json};

use super::super::super::callback::CallbackConfig;
use super::super::super::error::{AuthError, AuthErrorKind};
use super::super::super::http::HttpRequest;
use super::super::super::store::{Credential, OAuthCredentials};
use super::{CodeFlowSpec, Flow, run};

const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const USERINFO_URL: &str = "https://www.googleapis.com/oauth2/v1/userinfo?alt=json";
const GRANT_TTL_SKEW_MS: i64 = 5 * 60 * 1000;
const TIER_FREE: &str = "free-tier";
const TIER_LEGACY: &str = "legacy-tier";
const TIER_STANDARD: &str = "standard-tier";
/// `pollOperation` in the Gemini CLI flow: 24 attempts, 5 s apart.
const GEMINI_POLL_ATTEMPTS: u32 = 24;
const GEMINI_POLL_INTERVAL_MS: u64 = 5_000;
/// `ONBOARD_TIMEOUT_MS` / `ONBOARD_POLL_INTERVAL_MS` for Antigravity.
const ANTIGRAVITY_ONBOARD_TIMEOUT_MS: u64 = 30_000;
const ANTIGRAVITY_POLL_INTERVAL_MS: u64 = 1_000;
const DEFAULT_ANTIGRAVITY_VERSION: &str = "2.8.0";
const DEFAULT_GEMINI_CLI_VERSION: &str = "0.46.0";
const DEFAULT_GEMINI_CLI_MODEL: &str = "gemini-3.1-pro-preview";

/// Which Cloud Code Assist client a login targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Client {
	GeminiCli,
	Antigravity,
}

struct Config {
	provider: &'static str,
	client_id: &'static str,
	client_secret: &'static str,
	scopes: &'static str,
	callback_port: u16,
	callback_path: &'static str,
}

impl Client {
	fn config(self) -> Config {
		match self {
			// `atob(...)` in the TS source resolves to these public client ids.
			Self::GeminiCli => Config {
				provider: "google-gemini-cli",
				client_id: "681255809395-oo8ft2oprdrnp9e3aqf6av3hmdib135j.apps.googleusercontent.com",
				client_secret: "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",
				scopes: "https://www.googleapis.com/auth/cloud-platform \
					https://www.googleapis.com/auth/userinfo.email \
					https://www.googleapis.com/auth/userinfo.profile",
				callback_port: 8085,
				callback_path: "/oauth2callback",
			},
			Self::Antigravity => Config {
				provider: "google-antigravity",
				client_id: "1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com",
				client_secret: "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",
				scopes: "https://www.googleapis.com/auth/cloud-platform \
					https://www.googleapis.com/auth/userinfo.email \
					https://www.googleapis.com/auth/userinfo.profile \
					https://www.googleapis.com/auth/cclog \
					https://www.googleapis.com/auth/experimentsandconfigs",
				callback_port: 51121,
				callback_path: "/oauth-callback",
			},
		}
	}
}

pub async fn login(flow: &Flow<'_>, client: Client) -> Result<Credential, AuthError> {
	let config = client.config();
	let state = super::super::super::pkce::random_state()?;
	let spec = CodeFlowSpec::new(
		CallbackConfig::new(config.callback_port, config.callback_path).with_hostname("127.0.0.1"),
	)
	.with_instructions("Complete the sign-in in your browser.");

	let exchange = run(flow, &spec, &state, |redirect_uri| {
		let url = crate::auth::callback::with_query(
			AUTH_URL,
			&[
				("client_id", config.client_id),
				("response_type", "code"),
				("redirect_uri", redirect_uri),
				("scope", config.scopes),
				("state", state.as_str()),
				("access_type", "offline"),
				("prompt", "consent"),
			],
		);
		(url, None)
	})
	.await?;

	flow.notice("Exchanging authorization code for tokens...");
	let fields = vec![
		("client_id".to_string(), config.client_id.to_string()),
		("client_secret".to_string(), config.client_secret.to_string()),
		("code".to_string(), exchange.code),
		("grant_type".to_string(), "authorization_code".to_string()),
		("redirect_uri".to_string(), exchange.redirect_uri),
	];
	let payload = flow
		.http
		.post_form(TOKEN_URL, fields, flow.cancel, config.provider, AuthErrorKind::TokenExchange)
		.await?;
	let access = string_field(&payload, "access_token").unwrap_or_default();
	let refresh = string_field(&payload, "refresh_token").unwrap_or_default();
	let expires_in = payload.get("expires_in").and_then(Value::as_i64);
	let (Some(expires_in), false) = (expires_in, access.is_empty()) else {
		return Err(AuthError::new(AuthErrorKind::Validation, "Google token response missing required fields")
			.for_provider(config.provider));
	};
	if refresh.is_empty() {
		return Err(AuthError::new(AuthErrorKind::Validation, "No refresh token received. Please try again.")
			.for_provider(config.provider));
	}

	flow.notice("Getting user info...");
	let email = user_email(flow, &access, config.provider).await;
	flow.check()?;

	flow.notice(if client == Client::Antigravity {
		"Checking Cloud Code Assist account status..."
	} else {
		"Checking for existing Cloud Code Assist project..."
	});
	let project_id = match client {
		Client::GeminiCli => discover_gemini_project(flow, &access).await?,
		Client::Antigravity => discover_antigravity_project(flow, &access).await?,
	};

	Ok(Credential::OAuth(OAuthCredentials {
		access,
		refresh,
		expires: now_ms() + expires_in * 1000 - GRANT_TTL_SKEW_MS,
		project_id: Some(project_id),
		email,
		..OAuthCredentials::default()
	}))
}

pub async fn refresh(flow: &Flow<'_>, credentials: &OAuthCredentials) -> Result<Credential, AuthError> {
	let Some(project_id) = credentials.project_id.clone() else {
		return Err(AuthError::config(flow.provider(), "Google Cloud credentials missing projectId"));
	};
	if credentials.refresh.is_empty() {
		return Err(AuthError::new(AuthErrorKind::TokenRefresh, "stored credential has no refresh token")
			.for_provider(flow.provider()));
	}
	let config = match flow.provider() {
		"google-antigravity" => Client::Antigravity.config(),
		_ => Client::GeminiCli.config(),
	};
	let fields = vec![
		("client_id".to_string(), config.client_id.to_string()),
		("client_secret".to_string(), config.client_secret.to_string()),
		("refresh_token".to_string(), credentials.refresh.clone()),
		("grant_type".to_string(), "refresh_token".to_string()),
	];
	let payload = flow
		.http
		.post_form(TOKEN_URL, fields, flow.cancel, config.provider, AuthErrorKind::TokenRefresh)
		.await?;
	let access = string_field(&payload, "access_token").unwrap_or_default();
	let expires_in = payload.get("expires_in").and_then(Value::as_i64).unwrap_or_default();
	if access.is_empty() {
		return Err(AuthError::new(AuthErrorKind::Validation, "Google token refresh response missing access_token")
			.for_provider(config.provider));
	}
	Ok(Credential::OAuth(OAuthCredentials {
		access,
		refresh: string_field(&payload, "refresh_token")
			.filter(|value| !value.is_empty())
			.unwrap_or_else(|| credentials.refresh.clone()),
		expires: now_ms() + expires_in * 1000 - GRANT_TTL_SKEW_MS,
		project_id: Some(project_id),
		email: credentials.email.clone(),
		..OAuthCredentials::default()
	}))
}

async fn user_email(flow: &Flow<'_>, access: &str, provider: &'static str) -> Option<String> {
	let request = HttpRequest::get(USERINFO_URL)
		.bearer(access)
		.header("Accept", "application/json");
	match flow.http.send(request, flow.cancel, provider).await {
		Ok(response) if response.is_success() => {
			let payload = response.json(provider).ok()?;
			string_field(&payload, "email").filter(|value| !value.is_empty())
		},
		// Identity is decoration; a flaky userinfo endpoint never fails a login.
		_ => None,
	}
}

fn gemini_headers(access: &str) -> Vec<(String, String)> {
	let version = env_or("PI_AI_GEMINI_CLI_VERSION", DEFAULT_GEMINI_CLI_VERSION);
	let model = env_or("PI_AI_GEMINI_CLI_MODEL", DEFAULT_GEMINI_CLI_MODEL);
	let arch = match std::env::consts::ARCH {
		"x86_64" => "x64",
		other => other,
	};
	let user_agent = format!(
		"GeminiCLI/{version}/{model} ({}; {arch}; terminal)",
		std::env::consts::OS
	);
	vec![
		("Authorization".to_string(), format!("Bearer {access}")),
		("Content-Type".to_string(), "application/json".to_string()),
		("User-Agent".to_string(), user_agent),
		(
			"Client-Metadata".to_string(),
			"ideType=IDE_UNSPECIFIED,platform=PLATFORM_UNSPECIFIED,pluginType=GEMINI".to_string(),
		),
	]
}

/// `ANTIGRAVITY_VERSION_MANIFEST_URL` / fetch timeout from `gemini-headers.ts`.
const ANTIGRAVITY_VERSION_MANIFEST_URL: &str =
	"https://antigravity-hub-auto-updater-974169037036.us-central1.run.app/manifest/latest-arm64-mac.yml";
const ANTIGRAVITY_VERSION_FETCH_TIMEOUT_MS: u64 = 5_000;

/// Process-lifetime cache for the manifest-discovered version
/// (`discoveredAntigravityVersion`).
static DISCOVERED_ANTIGRAVITY_VERSION: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// `ensureAntigravityVersion`: refresh the version from the update manifest once
/// per process, skipping everything when `PI_AI_ANTIGRAVITY_VERSION` is set.
///
/// The backend gates newer models on the client version, so a newer release must
/// be picked up without shipping a new binary. Failures are silent — the pinned
/// [`DEFAULT_ANTIGRAVITY_VERSION`] stays valid — and retried on the next call.
async fn ensure_antigravity_version(flow: &Flow<'_>) {
	if std::env::var_os("PI_AI_ANTIGRAVITY_VERSION").is_some() {
		return;
	}
	{
		let cached = DISCOVERED_ANTIGRAVITY_VERSION
			.lock()
			.unwrap_or_else(std::sync::PoisonError::into_inner);
		if cached.is_some() {
			return;
		}
	}
	let request = HttpRequest::get(ANTIGRAVITY_VERSION_MANIFEST_URL)
		.header("Cache-Control", "no-cache")
		.header("User-Agent", "electron-builder")
		.timeout(std::time::Duration::from_millis(ANTIGRAVITY_VERSION_FETCH_TIMEOUT_MS));
	let Ok(response) = flow.http.send(request, flow.cancel, "google-antigravity").await else { return };
	if !response.is_success() {
		return;
	}
	if let Some(version) = parse_antigravity_manifest_version(&response.body) {
		let mut cached = DISCOVERED_ANTIGRAVITY_VERSION
			.lock()
			.unwrap_or_else(std::sync::PoisonError::into_inner);
		*cached = Some(version);
	}
}

/// `parseAntigravityManifestVersion`: first well-formed `version:` line of an
/// electron-builder update manifest; anything else is `None`.
fn parse_antigravity_manifest_version(manifest: &str) -> Option<String> {
	for line in manifest.lines() {
		let Some((key, value)) = line.split_once(':') else { continue };
		if key.trim() != "version" {
			continue;
		}
		let value = value.trim();
		let value = if let Some(quoted) = value.strip_prefix('"') {
			quoted.split('"').next().unwrap_or_default()
		} else if let Some(quoted) = value.strip_prefix('\'') {
			quoted.split('\'').next().unwrap_or_default()
		} else {
			value.split([' ', '\t', '#']).next().unwrap_or_default()
		};
		let value = value.trim();
		return is_semver_triplet(value).then(|| value.to_string());
	}
	None
}

fn is_semver_triplet(value: &str) -> bool {
	let parts: Vec<&str> = value.split('.').collect();
	parts.len() == 3 && parts.iter().all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
}

/// `getAntigravityVersion`: env override → manifest-discovered → pinned fallback.
fn antigravity_version() -> String {
	if let Ok(override_version) = std::env::var("PI_AI_ANTIGRAVITY_VERSION") {
		if !override_version.trim().is_empty() {
			return override_version.trim().to_string();
		}
	}
	let discovered = DISCOVERED_ANTIGRAVITY_VERSION
		.lock()
		.unwrap_or_else(std::sync::PoisonError::into_inner)
		.clone();
	discovered.unwrap_or_else(|| DEFAULT_ANTIGRAVITY_VERSION.to_string())
}

fn antigravity_headers(access: &str) -> Vec<(String, String)> {
	let version = antigravity_version();
	let cl = env_or("PI_AI_ANTIGRAVITY_CL", "963137146");
	let os = env_or("PI_AI_ANTIGRAVITY_OS", "darwin");
	let arch = env_or("PI_AI_ANTIGRAVITY_ARCH", "arm64");
	vec![
		("Authorization".to_string(), format!("Bearer {access}")),
		("Content-Type".to_string(), "application/json".to_string()),
		("User-Agent".to_string(), format!("antigravity/hub/{version} (aidev_client; os_type={os}; arch={arch}; cl={cl})")),
	]
}

fn cloud_code_request(url: &str, body: Value, headers: Vec<(String, String)>) -> HttpRequest {
	let mut request = HttpRequest::json(url, body).timeout(std::time::Duration::from_secs(30));
	request.headers.extend(headers);
	request
}

/// `discoverProject` for Gemini CLI: loadCodeAssist → onboard (when tierless) →
/// poll the LRO → project id.
async fn discover_gemini_project(flow: &Flow<'_>, access: &str) -> Result<String, AuthError> {
	const ENDPOINT: &str = "https://cloudcode-pa.googleapis.com";
	let provider = "google-gemini-cli";
	let headers = gemini_headers(access);
	let env_project = gemini_env_project();

	let load = cloud_code_request(
		&format!("{ENDPOINT}/v1internal:loadCodeAssist"),
		json!({
			"cloudaicompanionProject": env_project.clone(),
			"metadata": {
				"ideType": "IDE_UNSPECIFIED",
				"platform": "PLATFORM_UNSPECIFIED",
				"pluginType": "GEMINI",
				"duetProject": env_project.clone(),
			},
		}),
		headers.clone(),
	);
	let response = flow.http.send(load, flow.cancel, provider).await?;
	let payload = if response.is_success() {
		response.json(provider)?
	} else if is_vpc_sc_affected(&response) {
		json!({ "currentTier": { "id": TIER_STANDARD } })
	} else {
		return Err(response.error_for(AuthErrorKind::Discovery, provider, "loadCodeAssist"));
	};

	if payload.get("currentTier").is_some_and(|tier| !tier.is_null()) {
		if let Some(project) = string_field(&payload, "cloudaicompanionProject").filter(|value| !value.is_empty()) {
			return Ok(project);
		}
		return env_project.ok_or_else(gemini_project_env_error);
	}

	let tier = payload
		.get("allowedTiers")
		.and_then(Value::as_array)
		.and_then(|tiers| {
			tiers
				.iter()
				.find(|tier| tier.get("isDefault").and_then(Value::as_bool) == Some(true))
				.or_else(|| tiers.first())
		})
		.and_then(|tier| string_field(tier, "id"))
		.unwrap_or_else(|| TIER_LEGACY.to_string());
	if tier != TIER_FREE && env_project.is_none() {
		return Err(gemini_project_env_error());
	}

	flow.notice("Provisioning Cloud Code Assist project (this may take a moment)...");
	let mut onboard = json!({
		"tierId": tier,
		"metadata": {
			"ideType": "IDE_UNSPECIFIED",
			"platform": "PLATFORM_UNSPECIFIED",
			"pluginType": "GEMINI",
		},
	});
	if let Some(project) = env_project.as_ref() {
		onboard["cloudaicompanionProject"] = json!(project);
		if let Some(metadata) = onboard.get_mut("metadata") {
			metadata["duetProject"] = json!(project);
		}
	}
	let request = cloud_code_request(
		&format!("{ENDPOINT}/v1internal:onboardUser"),
		onboard,
		headers.clone(),
	);
	let response = flow.http.send(request, flow.cancel, provider).await?;
	if !response.is_success() {
		return Err(response.error_for(AuthErrorKind::Provisioning, provider, "onboardUser"));
	}
	let mut operation = response.json(provider)?;
	if operation.get("done").and_then(Value::as_bool) != Some(true) {
		let name = string_field(&operation, "name").unwrap_or_default();
		if name.is_empty() {
			return Err(AuthError::new(AuthErrorKind::Provisioning, "onboardUser returned an operation without a name")
				.for_provider(provider));
		}
		operation = poll_gemini_operation(flow, ENDPOINT, &name, headers).await?;
	}

	if let Some(project) = operation
		.get("response")
		.and_then(|response| response.get("cloudaicompanionProject"))
		.and_then(|project| project.get("id"))
		.and_then(Value::as_str)
		.filter(|value| !value.is_empty())
	{
		return Ok(project.to_string());
	}
	env_project.ok_or_else(|| {
		AuthError::new(
			AuthErrorKind::Validation,
			"Could not discover or provision a Google Cloud project. Try setting the GOOGLE_CLOUD_PROJECT or GOOGLE_CLOUD_PROJECT_ID environment variable. See https://goo.gle/gemini-cli-auth-docs#workspace-gca",
		)
		.for_provider(provider)
	})
}

async fn poll_gemini_operation(
	flow: &Flow<'_>,
	endpoint: &str,
	name: &str,
	headers: Vec<(String, String)>,
) -> Result<Value, AuthError> {
	let provider = "google-gemini-cli";
	for attempt in 1..=GEMINI_POLL_ATTEMPTS {
		flow.check()?;
		tokio::time::sleep(std::time::Duration::from_millis(GEMINI_POLL_INTERVAL_MS)).await;
		flow.notice(format!("Waiting for project provisioning (attempt {attempt}/{GEMINI_POLL_ATTEMPTS})..."));
		let mut request = HttpRequest::get(format!("{endpoint}/v1internal/{name}"));
		request.headers.extend(headers.clone());
		let response = flow.http.send(request, flow.cancel, provider).await?;
		if !response.is_success() {
			return Err(response.error_for(AuthErrorKind::Polling, provider, "Failed to poll operation"));
		}
		let payload = response.json(provider)?;
		if payload.get("done").and_then(Value::as_bool) == Some(true) {
			return Ok(payload);
		}
	}
	Err(AuthError::new(
		AuthErrorKind::Timeout,
		format!("Project provisioning did not complete after {GEMINI_POLL_ATTEMPTS} attempts"),
	)
	.for_provider(provider))
}

/// `discoverProject` for Antigravity: loadCodeAssist ➜ free-tier eligibility ➜
/// onboardUser (bounded 30 s) ➜ re-load until a project id appears.
async fn discover_antigravity_project(flow: &Flow<'_>, access: &str) -> Result<String, AuthError> {
	const ENDPOINT: &str = "https://daily-cloudcode-pa.googleapis.com";
	let provider = "google-antigravity";
	// Upstream refreshes the version before building the header set, so a newer
	// client version reaches the discovery call in the same login.
	ensure_antigravity_version(flow).await;
	let headers = antigravity_headers(access);

	let mut payload = load_code_assist(flow, ENDPOINT, &headers).await?;
	if payload.get("paidTier").is_none_or(Value::is_null) {
		if let Some(project) = string_field(&payload, "cloudaicompanionProject").filter(|value| !value.is_empty()) {
			payload = post_load_code_assist(flow, ENDPOINT, &headers, json!({
				"cloudaicompanionProject": project,
				"metadata": { "ideType": "ANTIGRAVITY" },
			}))
			.await?;
		}
	}
	assert_free_tier_eligible(&payload)?;

	if payload.get("currentTier").is_none_or(Value::is_null) {
		flow.notice("Provisioning the Antigravity free tier...");
		onboard_antigravity(flow, ENDPOINT, &headers).await?;
	}

	flow.notice("Refreshing Cloud Code Assist project...");
	let refreshed = load_code_assist(flow, ENDPOINT, &headers).await?;
	string_field(&refreshed, "cloudaicompanionProject")
		.filter(|value| !value.is_empty())
		.ok_or_else(|| {
			AuthError::new(AuthErrorKind::Provisioning, "loadCodeAssist did not return a cloudaicompanionProject")
				.for_provider(provider)
		})
}

async fn load_code_assist(
	flow: &Flow<'_>,
	endpoint: &str,
	headers: &[(String, String)],
) -> Result<Value, AuthError> {
	post_load_code_assist(flow, endpoint, headers, json!({ "metadata": { "ideType": "ANTIGRAVITY" } })).await
}

async fn post_load_code_assist(
	flow: &Flow<'_>,
	endpoint: &str,
	headers: &[(String, String)],
	body: Value,
) -> Result<Value, AuthError> {
	let mut request = HttpRequest::json(format!("{endpoint}/v1internal:loadCodeAssist"), body)
		.timeout(std::time::Duration::from_secs(30));
	request.headers.extend(headers.iter().cloned());
	let response = flow.http.send(request, flow.cancel, "google-antigravity").await?;
	if response.status != 200 {
		return Err(response.error_for(AuthErrorKind::Provisioning, "google-antigravity", "loadCodeAssist"));
	}
	response.json("google-antigravity")
}

fn assert_free_tier_eligible(payload: &Value) -> Result<(), AuthError> {
	let allowed = payload
		.get("allowedTiers")
		.and_then(Value::as_array)
		.is_some_and(|tiers| tiers.iter().any(|tier| string_field(tier, "id").as_deref() == Some(TIER_FREE)));
	if allowed {
		return Ok(());
	}
	let ineligible = payload
		.get("ineligibleTiers")
		.and_then(Value::as_array)
		.and_then(|tiers| tiers.iter().find(|tier| string_field(tier, "tierId").as_deref() == Some(TIER_FREE)));
	let Some(tier) = ineligible else {
		return Ok(());
	};
	let reason = string_field(tier, "reasonMessage").unwrap_or_default();
	if reason.is_empty() {
		return Ok(());
	}
	let validation = string_field(tier, "validationUrl").filter(|value| !value.is_empty());
	let message = match validation {
		Some(url) => format!("{reason}\n{url}"),
		None => reason,
	};
	Err(AuthError::new(AuthErrorKind::Provisioning, message).for_provider("google-antigravity"))
}

async fn onboard_antigravity(
	flow: &Flow<'_>,
	endpoint: &str,
	headers: &[(String, String)],
) -> Result<(), AuthError> {
	let started = std::time::Instant::now();
	let deadline = std::time::Duration::from_millis(ANTIGRAVITY_ONBOARD_TIMEOUT_MS);
	let mut request = HttpRequest::json(
		format!("{endpoint}/v1internal:onboardUser"),
		json!({ "tierId": TIER_FREE, "metadata": { "ideType": "ANTIGRAVITY" } }),
	)
	.timeout(deadline);
	request.headers.extend(headers.iter().cloned());
	let response = flow.http.send(request, flow.cancel, "google-antigravity").await?;
	if response.status != 200 {
		return Err(response.error_for(AuthErrorKind::Provisioning, "google-antigravity", "onboardUser"));
	}
	let mut operation = response.json("google-antigravity")?;

	loop {
		if operation.get("done").and_then(Value::as_bool) == Some(true) {
			if let Some(error) = operation.get("error").filter(|error| !error.is_null()) {
				// Untrusted provider prose: the operation name and kind are
				// already fixed text, so nothing from `error` is echoed.
				let _ = error;
				return Err(AuthError::new(AuthErrorKind::Provisioning, "OnboardUser operation failed")
					.for_provider("google-antigravity"));
			}
			if operation.get("response").is_none_or(Value::is_null) {
				return Err(AuthError::new(AuthErrorKind::Provisioning, "failed to unmarshal OnboardUserResponse")
					.for_provider("google-antigravity"));
			}
			return Ok(());
		}

		let remaining = deadline.checked_sub(started.elapsed()).filter(|remaining| !remaining.is_zero()).ok_or_else(|| {
			AuthError::new(
				AuthErrorKind::Timeout,
				format!("onboardUser timed out after {ANTIGRAVITY_ONBOARD_TIMEOUT_MS}ms"),
			)
			.for_provider("google-antigravity")
		})?;
		flow.check()?;
		tokio::time::sleep(remaining.min(std::time::Duration::from_millis(ANTIGRAVITY_POLL_INTERVAL_MS))).await;
		flow.check()?;

		let name = string_field(&operation, "name").unwrap_or_default();
		if name.is_empty() {
			return Err(AuthError::new(
				AuthErrorKind::Provisioning,
				"onboardUser returned an operation without a name",
			)
			.for_provider("google-antigravity"));
		}
		let remaining = deadline.checked_sub(started.elapsed()).filter(|remaining| !remaining.is_zero()).ok_or_else(|| {
			AuthError::new(
				AuthErrorKind::Timeout,
				format!("onboardUser timed out after {ANTIGRAVITY_ONBOARD_TIMEOUT_MS}ms"),
			)
			.for_provider("google-antigravity")
		})?;
		let mut request = HttpRequest::get(format!("{endpoint}/v1internal/{name}")).timeout(remaining);
		request.headers.extend(headers.iter().cloned());
		let response = flow.http.send(request, flow.cancel, "google-antigravity").await?;
		if response.status != 200 {
			return Err(response.error_for(AuthErrorKind::Provisioning, "google-antigravity", "onboardUser operation"));
		}
		operation = response.json("google-antigravity")?;
	}
}

/// `isVpcScAffectedUser`: a `SECURITY_POLICY_VIOLATED` detail means the account
/// is already provisioned inside a VPC-SC perimeter.
fn is_vpc_sc_affected(response: &crate::auth::http::HttpResponse) -> bool {
	let Ok(payload) = serde_json::from_str::<Value>(&response.body) else {
		return false;
	};
	payload
		.get("error")
		.and_then(|error| error.get("details"))
		.and_then(Value::as_array)
		.is_some_and(|details| {
			details.iter().any(|detail| string_field(detail, "reason").as_deref() == Some("SECURITY_POLICY_VIOLATED"))
		})
}

fn gemini_env_project() -> Option<String> {
	for key in ["GOOGLE_CLOUD_PROJECT", "GOOGLE_CLOUD_PROJECT_ID"] {
		if let Ok(value) = std::env::var(key) {
			let value = value.trim();
			if !value.is_empty() {
				return Some(value.to_string());
			}
		}
	}
	None
}

fn gemini_project_env_error() -> AuthError {
	AuthError::new(
		AuthErrorKind::Configuration,
		"This account requires setting the GOOGLE_CLOUD_PROJECT or GOOGLE_CLOUD_PROJECT_ID environment variable. See https://goo.gle/gemini-cli-auth-docs#workspace-gca",
	)
	.for_provider("google-gemini-cli")
}

fn env_or(key: &str, fallback: &'static str) -> String {
	std::env::var(key).ok().map(|value| value.trim().to_string()).filter(|value| !value.is_empty()).unwrap_or_else(|| fallback.to_string())
}

fn string_field(value: &Value, key: &str) -> Option<String> {
	value.get(key).and_then(Value::as_str).map(ToString::to_string)
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
	fn clients_match_upstream_endpoints() {
		let gemini = Client::GeminiCli.config();
		assert_eq!(gemini.callback_port, 8085);
		assert_eq!(gemini.callback_path, "/oauth2callback");
		assert!(gemini.client_id.ends_with(".apps.googleusercontent.com"));
		assert!(gemini.scopes.contains("cloud-platform") && !gemini.scopes.contains("cclog"));

		let antigravity = Client::Antigravity.config();
		assert_eq!(antigravity.callback_port, 51121);
		assert_eq!(antigravity.callback_path, "/oauth-callback");
		assert!(antigravity.scopes.contains("cclog"));
		assert!(antigravity.scopes.contains("experimentsandconfigs"));
	}

	#[test]
	fn authorize_params_use_offline_consent() {
		let config = Client::GeminiCli.config();
		let url = crate::auth::callback::with_query(
			AUTH_URL,
			&[
				("client_id", config.client_id),
				("response_type", "code"),
				("redirect_uri", "http://127.0.0.1:8085/oauth2callback"),
				("scope", config.scopes),
				("state", "st"),
				("access_type", "offline"),
				("prompt", "consent"),
			],
		);
		assert!(url.starts_with("https://accounts.google.com/o/oauth2/v2/auth?client_id=681255809395-"));
		assert!(url.contains("access_type=offline") && url.contains("prompt=consent"));
		assert!(url.contains("response_type=code"));
	}

	#[test]
	fn vpc_sc_detail_is_recognised() {
		let response = crate::auth::http::HttpResponse {
			status: 403,
			body: json!({ "error": { "details": [{ "reason": "SECURITY_POLICY_VIOLATED" }] } }).to_string(),
			set_cookies: Vec::new(),
		};
		assert!(is_vpc_sc_affected(&response));
		let clean = crate::auth::http::HttpResponse {
			status: 403,
			body: json!({ "error": { "details": [{ "reason": "OTHER" }] } }).to_string(),
			set_cookies: Vec::new(),
		};
		assert!(!is_vpc_sc_affected(&clean));
	}

	#[test]
	fn free_tier_ineligibility_surfaces_reason_and_url() {
		let payload = json!({
			"allowedTiers": [{ "id": "standard-tier" }],
			"ineligibleTiers": [{ "tierId": "free-tier", "reasonMessage": "not eligible", "validationUrl": "https://example.test" }]
		});
		let err = assert_free_tier_eligible(&payload).expect_err("ineligible");
		assert_eq!(err.kind, AuthErrorKind::Provisioning);
		assert!(err.message.contains("not eligible") && err.message.contains("https://example.test"));

		let allowed = json!({ "allowedTiers": [{ "id": "free-tier" }] });
		assert!(assert_free_tier_eligible(&allowed).is_ok());
		assert!(assert_free_tier_eligible(&json!({})).is_ok());
	}

	#[test]
	fn parses_antigravity_manifest_versions() {
		assert_eq!(
			parse_antigravity_manifest_version("version: 2.9.1\nfiles: []\n").as_deref(),
			Some("2.9.1")
		);
		assert_eq!(
			parse_antigravity_manifest_version("  version: \"2.10.0\"  # stable\n").as_deref(),
			Some("2.10.0")
		);
		assert_eq!(parse_antigravity_manifest_version("version: latest\n"), None);
		assert_eq!(parse_antigravity_manifest_version("version: 2.9\n"), None);
		assert_eq!(parse_antigravity_manifest_version(""), None);
	}
}
