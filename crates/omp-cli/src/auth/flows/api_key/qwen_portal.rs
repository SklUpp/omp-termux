//! Qwen Portal login (`registry/qwen-portal.ts`): one pasted OAuth token/API
//! key validated against the portal's OpenAI-compatible endpoint.

use super::super::Flow;
use super::super::super::error::AuthError;
use super::super::super::registry::Validation;
use super::super::super::store::Credential;
use super::validate;

const AUTH_URL: &str = "https://chat.qwen.ai";
const API_BASE_URL: &str = "https://portal.qwen.ai/v1";
const VALIDATION_MODEL: &str = "coder-model";

pub async fn login(flow: &Flow<'_>) -> Result<Credential, AuthError> {
	flow.open(AUTH_URL, None, Some("Copy your Qwen OAuth token or API key"));
	let key = flow.require("Paste your Qwen OAuth token or API key", "sk-...")?;
	flow.notice("Validating credentials...");
	validate(
		flow,
		Validation::Chat { provider: "qwen-portal", base_url: API_BASE_URL, model: VALIDATION_MODEL },
		&key,
	)
	.await?;
	Ok(Credential::ApiKey { key, source_login: true })
}
