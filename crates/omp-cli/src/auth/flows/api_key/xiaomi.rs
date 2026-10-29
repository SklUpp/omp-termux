//! Xiaomi MiMo logins (`registry/oauth/xiaomi.ts`).
//!
//! `xiaomi` validates a standard key against the public API; a `tp-` key and
//! every `xiaomi-token-plan-*` provider validate against the region endpoints
//! (Singapore → Europe → China) because token-plan keys are region-locked.
//! `401` on one endpoint moves to the next; any other status fails immediately.

use serde_json::json;

use super::super::Flow;
use super::validate;
use super::super::super::error::{AuthError, AuthErrorKind};
use super::super::super::http::HttpRequest;
use super::super::super::registry::{Validation, XiaomiRegion};
use super::super::super::store::Credential;

const STANDARD_AUTH_URL: &str = "https://platform.xiaomimimo.com/#/console/api-keys";
const TOKEN_PLAN_AUTH_URL: &str = "https://platform.xiaomimimo.com/console/plan-manage";
const STANDARD_API_BASE_URL: &str = "https://api.xiaomimimo.com/v1";
const VALIDATION_MODEL: &str = "mimo-v2.5";
const TOKEN_PLAN_PREFIX: &str = "tp-";
const VALIDATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

pub async fn login(flow: &Flow<'_>, region: Option<XiaomiRegion>) -> Result<Credential, AuthError> {
	let (auth_url, instructions, prompt, placeholder) = match region {
		Some(region) => (
			TOKEN_PLAN_AUTH_URL,
			format!("Copy your token-plan API key for the {} region", region.display_name()),
			format!("Paste your Xiaomi Token Plan {} API key (tp-...)", region.display_name()),
			"tp-...".to_string(),
		),
		None => (
			STANDARD_AUTH_URL,
			"Copy your API key from the Xiaomi MiMo console".to_string(),
			"Paste your Xiaomi API key (sk-... or token-plan tp-...)".to_string(),
			"sk-... or tp-...".to_string(),
		),
	};
	flow.open(auth_url, None, Some(&instructions));

	let key = flow.require(&prompt, &placeholder)?;
	flow.notice(format!("Validating {} API key...", flow.entry.name));
	validate(flow, Validation::Xiaomi { token_plan_region: region }, &key).await?;
	Ok(Credential::ApiKey { key, source_login: true })
}

fn endpoints(region: Option<XiaomiRegion>, key: &str) -> Vec<(&'static str, &'static str)> {
	match region {
		Some(region) => vec![(region.api_base_url(), VALIDATION_MODEL)],
		None if key.starts_with(TOKEN_PLAN_PREFIX) => vec![
			(XiaomiRegion::Sgp.api_base_url(), VALIDATION_MODEL),
			(XiaomiRegion::Ams.api_base_url(), VALIDATION_MODEL),
			(XiaomiRegion::Cn.api_base_url(), VALIDATION_MODEL),
		],
		None => vec![(STANDARD_API_BASE_URL, VALIDATION_MODEL)],
	}
}

pub(super) async fn validate_region(
	flow: &Flow<'_>,
	region: Option<XiaomiRegion>,
	key: &str,
) -> Result<(), AuthError> {
	let mut last_error: Option<AuthError> = None;
	for (base_url, model) in endpoints(region, key) {
		let request = HttpRequest::json(
			format!("{base_url}/chat/completions"),
			json!({
				"model": model,
				"max_tokens": 1,
				"messages": [{ "role": "user", "content": "ping" }],
			}),
		)
		.bearer(key)
		.timeout(VALIDATION_TIMEOUT);
		match flow.http.send(request, flow.cancel, flow.provider()).await {
			Ok(response) if response.is_success() => return Ok(()),
			Ok(response) if response.status == 401 => {
				last_error = Some(
					response.error_for(AuthErrorKind::Validation, flow.provider(), &format!("{} API key validation", flow.entry.name)),
				);
			},
			Ok(response) => {
				return Err(response.error_for(
					AuthErrorKind::Validation,
					flow.provider(),
					&format!("{} API key validation", flow.entry.name),
				));
			},
			Err(err) if err.kind == AuthErrorKind::Cancelled => return Err(err),
			Err(err) => last_error = Some(err),
		}
	}
	Err(last_error.unwrap_or_else(|| {
		AuthError::new(AuthErrorKind::Validation, format!("{} API key validation failed", flow.entry.name))
			.for_provider(flow.provider())
	}))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn token_plan_keys_probe_every_region_in_order() {
		assert_eq!(endpoints(Some(XiaomiRegion::Ams), "tp-x").len(), 1);
		assert_eq!(endpoints(None, "tp-x").len(), 3);
		assert_eq!(endpoints(None, "sk-x"), vec![(STANDARD_API_BASE_URL, VALIDATION_MODEL)]);
		assert_eq!(endpoints(None, "tp-x")[0].0, XiaomiRegion::Sgp.api_base_url());
	}
}
