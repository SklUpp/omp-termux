//! API-key logins (`registry/api-key-login.ts` + `api-key-validation.ts`).
//!
//! One engine, one config table ([`super::super::registry::ApiKeySpec`]): every
//! `createApiKeyLogin({…})` provider differs only in prompts, URL and which
//! probe validates the key. Providers whose login is not a single pasted value
//! live in the submodules below.

pub mod alibaba;
pub mod cloudflare;
pub mod qwen_portal;
pub mod xiaomi;

use serde_json::json;

use super::Flow;
use super::super::error::{AuthError, AuthErrorKind};
use super::super::http::{Cancel, HttpRequest};
use super::super::registry::{ApiKeySpec, ModelsUrl, Validation};
use super::super::store::Credential;

const USER_AGENT_PROBE: &str = "Accept: application/json";

pub async fn login(flow: &Flow<'_>, spec: &ApiKeySpec) -> Result<Credential, AuthError> {
	if let Some(url) = spec.auth_url {
		flow.open(url, None, spec.instructions);
	}

	let raw = flow.ask(spec.prompt, spec.placeholder, spec.allow_empty)?;
	let mut key = if flow.entry.id == "deepseek" { normalize_deepseek_key(&raw)? } else { raw };

	if key.is_empty() {
		match spec.empty_fallback {
			Some(fallback) => key = fallback.to_string(),
			None => return Err(AuthError::api_key_required(flow.entry.id)),
		}
	}

	validate(flow, spec.validation, &key).await?;
	Ok(Credential::ApiKey { key, source_login: true })
}

/// `normalizeDeepSeekApiKey`: users commonly paste `Authorization: Bearer …`.
fn normalize_deepseek_key(raw: &str) -> Result<String, AuthError> {
	let trimmed = raw.trim();
	if trimmed.is_empty() {
		return Ok(String::new());
	}
	let lowered_len = trimmed.len().min(6);
	if trimmed[..lowered_len].eq_ignore_ascii_case("bearer") {
		let stripped = trimmed[6..].trim();
		if stripped.is_empty() {
			return Err(AuthError::api_key_required("deepseek"));
		}
		return Ok(stripped.to_string());
	}
	Ok(trimmed.to_string())
}

/// Run the configured probe. `401`/`403` means the key is wrong; other failures
/// are reported but only `Validation::Tolerant` treats them as success.
pub async fn validate(flow: &Flow<'_>, validation: Validation, key: &str) -> Result<(), AuthError> {
	let provider = flow.provider();
	match validation {
		Validation::None => Ok(()),
		Validation::Chat { provider: label, base_url, model } => {
			let request = chat_request(base_url, model, key);
			let response = flow.http.send(request, flow.cancel, provider).await?;
			ensure_ok(&response, provider, label, false)
		},
		Validation::Anthropic { provider: label, base_url, model } => {
			let request = HttpRequest::json(
				format!("{base_url}/v1/messages"),
				json!({
					"model": model,
					"max_tokens": 1,
					"messages": [{ "role": "user", "content": "Hi" }],
				}),
			)
			.header("anthropic-version", "2023-06-01")
			.header("x-api-key", key)
			.header("Accept", USER_AGENT_PROBE)
			.timeout(std::time::Duration::from_secs(30));
			let response = flow.http.send(request, flow.cancel, provider).await?;
			ensure_ok(&response, provider, label, false)
		},
		Validation::Models { provider: label, url, project_header } => {
			let url = match url {
				ModelsUrl::Static(url) => url.to_string(),
				ModelsUrl::Moonshot => {
					let base = std::env::var("MOONSHOT_BASE_URL")
						.ok()
						.map(|value| value.trim().trim_end_matches('/').to_string())
						.filter(|value| !value.is_empty())
						.unwrap_or_else(|| "https://api.moonshot.ai/v1".to_string());
					format!("{base}/models")
				},
			};
			let mut request = HttpRequest::get(url).bearer(key).header("Accept", USER_AGENT_PROBE);
			if project_header {
				let project = std::env::var("COREWEAVE_PROJECT")
					.ok()
					.map(|value| value.trim().to_string())
					.filter(|value| !value.is_empty())
					.ok_or_else(|| {
						AuthError::config(
							provider,
							"COREWEAVE_PROJECT is not set. Add `export COREWEAVE_PROJECT=<team>/<project>` to your shell startup file, restart the shell, and retry.",
						)
					})?;
				request = request.header("OpenAI-Project", project);
			}
			let response = flow.http.send(request, flow.cancel, provider).await?;
			ensure_ok(&response, provider, label, false)
		},
		Validation::Tolerant { provider: label, base_url, model } => {
			let response = flow.http.send(chat_request(base_url, model, key), flow.cancel, provider).await?;
			ensure_ok(&response, provider, label, true)
		},
		Validation::Xiaomi { token_plan_region } => xiaomi::validate_region(flow, token_plan_region, key).await,
	}
}

fn chat_request(base_url: &str, model: &str, key: &str) -> HttpRequest {
	HttpRequest::json(
		format!("{}/chat/completions", base_url.trim_end_matches('/')),
		json!({
			"model": model,
			"messages": [{ "role": "user", "content": "Hi" }],
			"max_tokens": 1,
			"stream": false,
		}),
	)
	.bearer(key)
	.header("Accept", USER_AGENT_PROBE)
	.timeout(std::time::Duration::from_secs(30))
}

/// Map a probe response to pass/fail. Auth failures are definitive; anything
/// else is reported with its status and treated as pass only for `tolerant`.
fn ensure_ok(
	response: &super::super::http::HttpResponse,
	provider: &str,
	label: &str,
	tolerant: bool,
) -> Result<(), AuthError> {
	if response.is_success() {
		return Ok(());
	}
	if matches!(response.status, 401 | 403) {
		return Err(AuthError::new(
			AuthErrorKind::Validation,
			format!("API key rejected by {label}. Check the key and try again."),
		)
		.for_provider(provider)
		.with_status(response.status));
	}
	if tolerant {
		return Ok(());
	}
	Err(response.error_for(AuthErrorKind::Validation, provider, &format!("{label} key validation")))
}

/// Shared helper for the providers that store an OAuth-shaped record whose
/// `access` is an API key (`alibaba-coding-plan`, `alibaba-token-plan`) —
/// `storeCredentialsAsOAuth` in the upstream registry.
pub(super) fn api_key_as_oauth(key: &str, expires: i64) -> Credential {
	Credential::OAuth(super::super::store::OAuthCredentials {
		access: key.to_string(),
		refresh: key.to_string(),
		expires,
		..super::super::store::OAuthCredentials::default()
	})
}

pub(super) fn check_cancel(cancel: &Cancel, provider: &str) -> Result<(), AuthError> {
	cancel.check(provider)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn deepseek_normalization_strips_bearer_prefix() {
		assert_eq!(normalize_deepseek_key("  Bearer sk-abc \n").expect("key"), "sk-abc");
		assert_eq!(normalize_deepseek_key("sk-abc").expect("key"), "sk-abc");
		assert!(normalize_deepseek_key("bearer   ").is_err());
	}

	#[test]
	fn chat_probe_targets_openai_compatible_path() {
		let request = chat_request("https://api.example/v1/", "m", "k");
		assert_eq!(request.url, "https://api.example/v1/chat/completions");
		assert!(request.headers.iter().any(|(name, value)| name == "Authorization" && value == "Bearer k"));
	}

	#[test]
	fn api_key_as_oauth_mirrors_key_into_both_tokens() {
		let credential = api_key_as_oauth("k", 1);
		match credential {
			Credential::OAuth(creds) => {
				assert_eq!(creds.access, "k");
				assert_eq!(creds.refresh, "k");
				assert_eq!(creds.expires, 1);
			},
			other => panic!("unexpected {other:?}"),
		}
	}
}
