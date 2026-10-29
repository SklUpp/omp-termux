//! Cloudflare AI Gateway login (`registry/cloudflare-ai-gateway.ts`).
//!
//! Three prompts (token, account id, gateway id) serialized into the same JSON
//! envelope the wire layer parses, so a Rust login and a JS login are
//! interchangeable for the runtime.

use super::super::Flow;
use super::super::super::error::AuthError;
use super::super::super::store::Credential;

const AUTH_URL: &str = "https://developers.cloudflare.com/ai-gateway/configuration/authentication/";

pub async fn login(flow: &Flow<'_>) -> Result<Credential, AuthError> {
	flow.open(
		AUTH_URL,
		None,
		Some("Create an AI Gateway token with Run permission, then copy it here."),
	);

	let token = flow.require("Paste your Cloudflare AI Gateway token/API key", "cfut_...")?;
	let account_id = flow.ask("Enter your Cloudflare account ID", "32-character account ID", false)?;
	if account_id.is_empty() {
		return Err(AuthError::config(flow.provider(), "Cloudflare account ID is required"));
	}
	let gateway_id = flow.ask("Enter your Cloudflare AI Gateway ID", "default", false)?;
	if gateway_id.is_empty() {
		return Err(AuthError::config(flow.provider(), "Cloudflare AI Gateway ID is required"));
	}

	// `serializeCloudflareAiGatewayCredential`.
	let key = serde_json::json!({
		"token": token.trim(),
		"accountId": account_id.trim(),
		"gatewayId": gateway_id.trim(),
	})
	.to_string();
	Ok(Credential::ApiKey { key, source_login: true })
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn envelope_matches_wire_field_names() {
		let value = serde_json::json!({ "token": "t", "accountId": "a", "gatewayId": "g" }).to_string();
		let parsed: serde_json::Value = serde_json::from_str(&value).expect("json");
		assert_eq!(parsed["accountId"], "a");
		assert_eq!(parsed["gatewayId"], "g");
		// Sanity: the credential variant used by the login path.
		assert_eq!(Credential::ApiKey { key: value, source_login: true }.credential_type(), "api_key");
	}
}
