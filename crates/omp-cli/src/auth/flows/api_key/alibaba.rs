//! Alibaba logins: `alibaba-coding-plan` and `alibaba-token-plan`.
//!
//! Both ask for a region (1=international, 2=China, 3=custom), validate the key
//! against the region's endpoint, and differ in what they store:
//! coding-plan stores an OAuth-shaped record whose tokens are the key
//! (`getApiKey: credentials => credentials.access`), token-plan stores a key
//! string — sometimes a JSON envelope carrying the optional usage cookie and a
//! non-default region base URL.

use serde_json::json;

use super::super::api_key::{api_key_as_oauth, validate};
use super::super::Flow;
use super::super::super::callback::encode_component;
use super::super::super::error::{AuthError, AuthErrorKind};
use super::super::super::http::HttpRequest;
use super::super::super::registry::Validation;
use super::super::super::store::{Credential, OAuthCredentials};

const CODING_PLAN_DEFAULT_AUTH_URL: &str = "https://modelstudio.console.alibabacloud.com/";
const CODING_PLAN_CHINA_AUTH_URL: &str = "https://bailian.console.aliyun.com/?tab=model#/api-key";
const CODING_PLAN_DEFAULT_BASE_URL: &str = "https://coding-intl.dashscope.aliyuncs.com/v1";
const CODING_PLAN_CHINA_BASE_URL: &str = "https://coding.dashscope.aliyuncs.com/v1";
const CODING_PLAN_MODEL: &str = "qwen3.5-plus";

const TOKEN_PLAN_INTERNATIONAL_AUTH_URL: &str =
	"https://home.qwencloud.com/billing/subscription/token-plan-individual";
const TOKEN_PLAN_CHINA_AUTH_URL: &str = "https://www.aliyun.com/benefit/scene/tokenplan";
const TOKEN_PLAN_BASE_URL: &str = "https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1";
const TOKEN_PLAN_CN_BASE_URL: &str = "https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1";

pub async fn login(flow: &Flow<'_>, token_plan: bool) -> Result<Credential, AuthError> {
	if token_plan { login_token_plan(flow).await } else { login_coding_plan(flow).await }
}

async fn login_coding_plan(flow: &Flow<'_>) -> Result<Credential, AuthError> {
	let choice = flow.ask(
		"Select Alibaba Coding Plan endpoint: 1=International (default), 2=China, 3=Custom — enter 1, 2, or 3",
		"1",
		true,
	)?;

	let (base_url, validation_base, auth_url, instructions, custom) = match choice.trim() {
		"2" => (
			CODING_PLAN_CHINA_BASE_URL.to_string(),
			CODING_PLAN_CHINA_BASE_URL,
			CODING_PLAN_CHINA_AUTH_URL,
			"Copy your API key from the Alibaba Cloud Bailian console (China mainland)",
			false,
		),
		"3" => {
			let custom_url = flow.require("Enter custom base URL", "https://your-proxy.com/v1")?;
			let trimmed = custom_url.trim_end_matches('/').to_string();
			if trimmed.is_empty() {
				return Err(AuthError::config(flow.provider(), "Custom URL is required for option 3"));
			}
			(
				trimmed,
				CODING_PLAN_DEFAULT_BASE_URL,
				CODING_PLAN_DEFAULT_AUTH_URL,
				"Copy your API key from the Alibaba Cloud DashScope console",
				true,
			)
		},
		_ => (
			CODING_PLAN_DEFAULT_BASE_URL.to_string(),
			CODING_PLAN_DEFAULT_BASE_URL,
			CODING_PLAN_DEFAULT_AUTH_URL,
			"Copy your API key from the Alibaba Cloud DashScope console (International)",
			false,
		),
	};

	flow.open(auth_url, None, Some(instructions));
	let key = flow.require("Paste your Alibaba Coding Plan API key", "sk-...")?;
	flow.notice("Validating API key...");

	if custom {
		validate_models(flow, &format!("{base_url}/models"), &key).await?;
	} else {
		validate(
			flow,
			Validation::Chat { provider: "Alibaba Coding Plan", base_url: validation_base, model: CODING_PLAN_MODEL },
			&key,
		)
		.await?;
	}

	let mut credential = api_key_as_oauth(&key, i64::MAX);
	if let Credential::OAuth(OAuthCredentials { enterprise_url, .. }) = &mut credential {
		*enterprise_url = Some(base_url);
	}
	Ok(credential)
}

async fn login_token_plan(flow: &Flow<'_>) -> Result<Credential, AuthError> {
	let choice = flow.ask(
		"Select QwenCloud Token Plan region: 1=International (default), 2=China (Beijing), 3=Custom — enter 1, 2, or 3",
		"1",
		true,
	)?;

	let (base_url, auth_url, instructions) = match choice.trim() {
		"2" => (
			TOKEN_PLAN_CN_BASE_URL.to_string(),
			TOKEN_PLAN_CHINA_AUTH_URL,
			"Subscribe to the China (Beijing) 百炼 Token Plan and copy its dedicated API key",
		),
		"3" => {
			let custom_url = flow.require(
				"Enter custom Token Plan base URL",
				"https://token-plan.<region>.maas.aliyuncs.com/compatible-mode/v1",
			)?;
			let trimmed = custom_url.trim_end_matches('/').to_string();
			if trimmed.is_empty() {
				return Err(AuthError::config(flow.provider(), "Custom URL is required for option 3"));
			}
			(
				trimmed,
				TOKEN_PLAN_INTERNATIONAL_AUTH_URL,
				"Copy your Token Plan API key for the custom endpoint",
			)
		},
		_ => (
			TOKEN_PLAN_BASE_URL.to_string(),
			TOKEN_PLAN_INTERNATIONAL_AUTH_URL,
			"Subscribe to Token Plan Individual and copy its dedicated API key. Keep this page open; the next prompt explains how to enable optional quota reporting.",
		),
	};

	flow.open(auth_url, None, Some(instructions));
	let key = flow.require("Paste your QwenCloud Token Plan API key", "sk-sp-...")?;
	flow.notice("Validating API key...");
	validate_models(flow, &format!("{base_url}/models"), &key).await?;

	let china = base_url == TOKEN_PLAN_CN_BASE_URL;
	let host = if china { "bailian-cs.console.aliyun.com" } else { "cs-data.qwencloud.com" };
	let cookie_prompt = format!(
		"Optional quota reporting: open browser DevTools → Network, reload the Token Plan page, filter for api.json, and select the {host}/data/api.json request whose api query ends in /tokenplan/personal/api/v2/usage. Copy Request Headers → Cookie, then paste the complete name=value; ... value here, or press Enter to skip."
	);
	let raw_cookie = flow.ask(&cookie_prompt, "name=value; name=value; ...", true)?;
	let cookie = raw_cookie
		.trim()
		.strip_prefix("Cookie:")
		.map(str::trim)
		.map(str::to_string)
		.unwrap_or_else(|| raw_cookie.trim().to_string());
	flow.check()?;

	if !cookie.is_empty() && !cookie.split(';').any(cookie_segment_is_valid) {
		return Err(AuthError::config(
			flow.provider(),
			format!(
				"Invalid QwenCloud Cookie header. Copy the complete Cookie request header from the {host} usage request, not a single cookie value."
			),
		));
	}

	let region_url = (base_url != TOKEN_PLAN_BASE_URL).then_some(base_url.as_str());
	let value = serialize_token_plan_credential(&key, &cookie, region_url);
	Ok(Credential::ApiKey { key: value, source_login: true })
}

fn cookie_segment_is_valid(segment: &str) -> bool {
	match segment.split_once('=') {
		Some((name, value)) => !name.trim().is_empty() && !value.trim().is_empty(),
		None => false,
	}
}

/// `serializeAlibabaTokenPlanCredential`: bare token when there is no envelope
/// data, otherwise `{"token":…,"cookie":…,"baseUrl":…}`.
fn serialize_token_plan_credential(token: &str, cookie: &str, base_url: Option<&str>) -> String {
	let cookie = cookie.trim();
	let base_url = base_url.map(str::trim).unwrap_or("");
	if cookie.is_empty() && base_url.is_empty() {
		return token.to_string();
	}
	let mut payload = serde_json::Map::new();
	payload.insert("token".to_string(), json!(token));
	if !cookie.is_empty() {
		payload.insert("cookie".to_string(), json!(cookie));
	}
	if !base_url.is_empty() {
		payload.insert("baseUrl".to_string(), json!(base_url));
	}
	serde_json::Value::Object(payload).to_string()
}

async fn validate_models(flow: &Flow<'_>, models_url: &str, key: &str) -> Result<(), AuthError> {
	let request = HttpRequest::get(models_url).bearer(key);
	let response = flow.http.send(request, flow.cancel, flow.provider()).await?;
	if response.is_success() {
		return Ok(());
	}
	if matches!(response.status, 401 | 403) {
		return Err(AuthError::new(AuthErrorKind::Validation, "API key rejected. Check the key and try again.")
			.for_provider(flow.provider())
			.with_status(response.status));
	}
	Err(response.error_for(AuthErrorKind::Validation, flow.provider(), "key validation"))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn token_plan_serialization_matches_wire_format() {
		assert_eq!(serialize_token_plan_credential("sk-1", "", None), "sk-1");
		assert_eq!(
			serialize_token_plan_credential("sk-1", "a=b", None),
			r#"{"token":"sk-1","cookie":"a=b"}"#
		);
		assert_eq!(
			serialize_token_plan_credential("sk-1", "", Some(TOKEN_PLAN_CN_BASE_URL)),
			format!(r#"{{"token":"sk-1","baseUrl":"{TOKEN_PLAN_CN_BASE_URL}"}}"#)
		);
	}

	#[test]
	fn cookie_segments_need_name_and_value() {
		assert!(cookie_segment_is_valid("a=b"));
		assert!(!cookie_segment_is_valid("a="));
		assert!(!cookie_segment_is_valid("=b"));
		assert!(!cookie_segment_is_valid("noequals"));
	}

	#[test]
	fn cookie_header_prefix_is_stripped_like_upstream() {
		let raw = " Cookie: a=b; c=d ";
		let stripped = raw.trim().strip_prefix("Cookie:").map(str::trim).unwrap_or(raw.trim());
		assert_eq!(stripped, "a=b; c=d");
		// `encode_component` is the same encoder used for authorize URLs.
		assert_eq!(encode_component("a b"), "a+b");
	}
}
