//! Z.ai / GLM "Coding Plan · Sign in" (`registry/oauth/zai.ts`).
//!
//! Authorization-code flow without PKCE against `chat.z.ai`, then a business
//! sequence that mints a durable `apiKey.secretKey` pair: business login →
//! default organization/project → find-or-create the `oh-my-pi` key → copy its
//! secret. The minted key never expires, so the stored grant carries
//! `NEVER_EXPIRES` and no refresh token — `getOAuthApiKey` hands `access`
//! straight to the `zai` provider.

use serde_json::{Map, Value, json};

use super::super::super::callback::CallbackConfig;
use super::super::super::error::{AuthError, AuthErrorKind};
use super::super::super::store::{Credential, OAuthCredentials};
use super::{CodeFlowSpec, Flow, run};

const DEFAULT_CLIENT_ID: &str = "client_P8X5CMWmlaRO9gyO-KSqtg";
const DEFAULT_AUTHORIZE_URL: &str = "https://chat.z.ai/api/oauth/authorize";
const DEFAULT_TOKEN_URL: &str = "https://zcode.z.ai/api/v1/oauth/token";
const DEFAULT_BIZ_BASE: &str = "https://api.z.ai";
const DEFAULT_BUSINESS_LOGIN_URL: &str = "https://api.z.ai/api/auth/z/login";
/// OMP's own key name so sign-in never mutates ZCode's `zcode-api-key`.
const KEY_NAME: &str = "oh-my-pi";
const CALLBACK_PORT: u16 = 54548;
const CALLBACK_PATH: &str = "/callback";
/// Durable minted key never expires (`NEVER_EXPIRES` in the TS flow).
const NEVER_EXPIRES: i64 = 8_640_000_000_000_000;

fn env_or(key: &str, fallback: &'static str) -> String {
	std::env::var(key).ok().filter(|value| !value.is_empty()).unwrap_or_else(|| fallback.to_string())
}

pub async fn login(flow: &Flow<'_>) -> Result<Credential, AuthError> {
	let client_id = env_or("ZAI_OAUTH_CLIENT_ID", DEFAULT_CLIENT_ID);
	let authorize_url = env_or("ZAI_OAUTH_AUTHORIZE_URL", DEFAULT_AUTHORIZE_URL);
	let state = super::super::super::pkce::random_state()?;
	// `OAuthCallbackFlow` here keeps the default fallback: only `chat.z.ai` is
	// registered loosely, and upstream does not pin the redirect URI.
	let spec = CodeFlowSpec::new(CallbackConfig::new(CALLBACK_PORT, CALLBACK_PATH)).with_instructions(
		"Complete Z.ai login in your browser. If the browser cannot reach this machine, paste the final redirect URL or authorization code when prompted.",
	);

	let exchange = run(flow, &spec, &state, |redirect_uri| {
		let url = super::super::super::callback::with_query(
			&authorize_url,
			&[
				("redirect_uri", redirect_uri),
				("response_type", "code"),
				("client_id", client_id.as_str()),
				("state", state.as_str()),
			],
		);
		(url, None)
	})
	.await?;

	let token_url = env_or("ZAI_OAUTH_TOKEN_URL", DEFAULT_TOKEN_URL);
	let body = json!({
		"provider": "zai",
		"code": exchange.code,
		"redirect_uri": exchange.redirect_uri,
		"state": exchange.state,
	});
	let payload = flow
		.http
		.post_json(token_url.as_str(), body, Vec::new(), flow.cancel, "zai", AuthErrorKind::TokenExchange)
		.await
		.map_err(|err| {
			AuthError::new(
				AuthErrorKind::TokenExchange,
				format!("Token exchange request failed. url={token_url}; details={}", err.message),
			)
			.for_provider("zai")
		})?;
	let data = unwrap_envelope(&payload, "token exchange")?;

	let oauth_access = trimmed(data.get("zai").and_then(|zai| zai.get("access_token"))).ok_or_else(|| {
		AuthError::new(AuthErrorKind::Validation, "Z.ai token response missing access token").for_provider("zai")
	})?;
	let minted = mint_api_key(flow, &oauth_access).await?;
	let user = data.get("user");
	Ok(Credential::OAuth(OAuthCredentials {
		access: minted,
		refresh: String::new(),
		expires: NEVER_EXPIRES,
		email: trimmed(user.and_then(|user| user.get("email"))),
		account_id: user.and_then(|user| user.get("id")).and_then(|id| match id {
			Value::String(value) => Some(value.clone()),
			Value::Number(number) => Some(number.to_string()),
			_ => None,
		}),
		..OAuthCredentials::default()
	}))
}

/// `{ code, msg, data, success }` envelope. `code` 0/200 or `success !== false`
/// is success; bodies without a status wrapper pass through unchanged.
fn unwrap_envelope(body: &Value, operation: &str) -> Result<Map<String, Value>, AuthError> {
	let Some(object) = body.as_object() else {
		return Err(AuthError::new(
			AuthErrorKind::TokenExchange,
			format!("Z.ai {operation} response was not a JSON object"),
		)
		.for_provider("zai"));
	};
	let wrapped = object.contains_key("code") || object.contains_key("success");
	if !wrapped {
		return Ok(object.clone());
	}
	let success = object.get("success").and_then(Value::as_bool);
	let code_ok = match object.get("code") {
		None | Some(Value::Null) => true,
		Some(Value::Number(number)) => matches!(number.as_i64(), Some(0 | 200)),
		Some(Value::String(value)) => value == "0" || value == "200",
		Some(_) => false,
	};
	if success == Some(false) || !code_ok {
		let reason = object
			.get("msg")
			.and_then(Value::as_str)
			.map(ToString::to_string)
			.or_else(|| object.get("code").map(Value::to_string))
			.unwrap_or_else(|| "unknown error".to_string());
		return Err(AuthError::new(
			AuthErrorKind::TokenExchange,
			format!("Z.ai {operation} failed: {reason}"),
		)
		.for_provider("zai"));
	}
	match object.get("data") {
		Some(Value::Object(data)) => Ok(data.clone()),
		Some(Value::Null) | None => Ok(Map::new()),
		Some(other) => Err(AuthError::new(
			AuthErrorKind::TokenExchange,
			format!("Z.ai {operation} returned unexpected data: {}", redact_shape(other)),
		)
		.for_provider("zai")),
	}
}

fn redact_shape(value: &Value) -> &'static str {
	// Never echo business payloads: they can carry key material.
	let _ = value;
	"non-object payload"
}

fn trimmed(value: Option<&Value>) -> Option<String> {
	value
		.and_then(Value::as_str)
		.map(str::trim)
		.filter(|value| !value.is_empty())
		.map(ToString::to_string)
}

/// Exchange the short-lived OAuth token for a durable `apiKey.secretKey`.
async fn mint_api_key(flow: &Flow<'_>, oauth_access: &str) -> Result<String, AuthError> {
	let biz_base = env_or("ZAI_BIZ_BASE", DEFAULT_BIZ_BASE);
	let login_url = env_or("ZAI_BUSINESS_LOGIN_URL", DEFAULT_BUSINESS_LOGIN_URL);

	let payload = flow
		.http
		.post_json(
			login_url.as_str(),
			json!({ "token": oauth_access }),
			Vec::new(),
			flow.cancel,
			"zai",
			AuthErrorKind::TokenExchange,
		)
		.await?;
	let data = unwrap_envelope(&payload, "business login")?;
	let biz_token = trimmed(data.get("access_token"))
		.or_else(|| trimmed(data.get("accessToken")))
		.ok_or_else(|| {
			AuthError::new(AuthErrorKind::TokenExchange, "Z.ai business login returned no access token")
				.for_provider("zai")
		})?;

	let customer = get_biz(flow, format!("{biz_base}/api/biz/customer/getCustomerInfo"), &biz_token, "customer lookup").await?;
	let organizations = customer.get("organizations").and_then(Value::as_array).cloned().unwrap_or_default();
	let org = pick_default(&organizations);
	let projects = org.and_then(|org| org.get("projects")).and_then(Value::as_array).cloned().unwrap_or_default();
	let project = pick_default(&projects);
	let organization_id = trimmed(org.and_then(|org| org.get("organizationId")));
	let project_id = trimmed(project.and_then(|project| project.get("projectId")));
	let (Some(organization_id), Some(project_id)) = (organization_id, project_id) else {
		return Err(AuthError::new(
			AuthErrorKind::TokenExchange,
			"Z.ai key provisioning failed: no organization/project on account",
		)
		.for_provider("zai"));
	};

	let keys_url = format!("{biz_base}/api/biz/v1/organization/{organization_id}/projects/{project_id}/api_keys");
	let listed = get_biz(flow, keys_url.clone(), &biz_token, "api key list").await?;
	let existing = as_key_array(&listed).into_iter().find(|key| key.get("name").and_then(Value::as_str) == Some(KEY_NAME));
	let record = match existing {
		Some(record) => Value::Object(record),
		None => {
			let created = flow
				.http
				.post_json(
					keys_url.as_str(),
					json!({ "name": KEY_NAME }),
					vec![("Authorization".to_string(), format!("Bearer {biz_token}"))],
					flow.cancel,
					"zai",
					AuthErrorKind::TokenExchange,
				)
				.await?;
			Value::Object(unwrap_envelope(&created, "api key create")?)
		},
	};
	let api_key = trimmed(record.get("apiKey")).ok_or_else(|| {
		AuthError::new(AuthErrorKind::TokenExchange, "Z.ai key provisioning returned no apiKey").for_provider("zai")
	})?;

	let copy_url = format!(
		"{keys_url}/copy/{}",
		super::super::super::callback::encode_component(&api_key)
	);
	let copied = get_biz(flow, copy_url, &biz_token, "api key copy").await?;
	let secret = trimmed(copied.get("secretKey")).ok_or_else(|| {
		AuthError::new(AuthErrorKind::TokenExchange, "Z.ai key provisioning returned no secretKey").for_provider("zai")
	})?;
	Ok(format!("{api_key}.{secret}"))
}

async fn get_biz(flow: &Flow<'_>, url: String, biz_token: &str, operation: &str) -> Result<Map<String, Value>, AuthError> {
	let payload = flow
		.http
		.get_json(
			super::super::super::http::HttpRequest::get(url).bearer(biz_token).header("Accept", "application/json"),
			flow.cancel,
			"zai",
		)
		.await?;
	unwrap_envelope(&payload, operation)
}

fn pick_default(entries: &[Value]) -> Option<&Value> {
	entries
		.iter()
		.find(|entry| entry.get("isDefault").and_then(Value::as_bool) == Some(true))
		.or_else(|| entries.first())
}

/// `api_keys` list may be a bare array or wrapped in `list`/`keys`/`apiKeys`/`records`.
fn as_key_array(value: &Map<String, Value>) -> Vec<Map<String, Value>> {
	let direct: Vec<Value> = value.values().cloned().collect();
	let candidate = value
		.get("list")
		.or_else(|| value.get("keys"))
		.or_else(|| value.get("apiKeys"))
		.or_else(|| value.get("records"))
		.cloned()
		.or_else(|| direct.into_iter().find(Value::is_array));
	match candidate {
		Some(Value::Array(items)) => items.into_iter().filter_map(|item| item.as_object().cloned()).collect(),
		_ => Vec::new(),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn envelope_accepts_both_success_codes() {
		let zero = json!({ "code": 0, "data": { "zai": { "access_token": "a" } } });
		assert!(unwrap_envelope(&zero, "token exchange").is_ok());
		let two_hundred = json!({ "code": 200, "data": { "access_token": "b" } });
		assert!(unwrap_envelope(&two_hundred, "business login").is_ok());
		let bare = json!({ "access_token": "c" });
		assert!(unwrap_envelope(&bare, "business login").is_ok());
	}

	#[test]
	fn envelope_rejects_failure_codes_without_tokens() {
		let failed = json!({ "code": 401, "msg": "expired", "data": { "access_token": "leak" } });
		let err = unwrap_envelope(&failed, "token exchange").expect_err("fails");
		assert_eq!(err.kind, AuthErrorKind::TokenExchange);
		assert!(err.message.contains("expired"));
		assert!(!err.message.contains("leak"));

		let success_false = json!({ "success": false, "msg": "nope" });
		assert!(unwrap_envelope(&success_false, "business login").is_err());
	}

	#[test]
	fn key_list_accepts_wrappers() {
		let wrapped = json!({ "list": [{ "name": "oh-my-pi", "apiKey": "x" }] });
		let map = wrapped.as_object().expect("object");
		assert_eq!(as_key_array(map).len(), 1);
		let bare = json!([{ "name": "oh-my-pi" }]);
		let map = bare.as_object().cloned().map(|_| Map::new()).unwrap_or_default();
		assert!(as_key_array(&map).is_empty());
	}

	#[test]
	fn default_entries_prefer_is_default_then_first() {
		let entries = vec![json!({ "organizationId": "a" }), json!({ "organizationId": "b", "isDefault": true })];
		assert_eq!(trimmed(pick_default(&entries).and_then(|entry| entry.get("organizationId"))).as_deref(), Some("b"));
		let no_default = vec![json!({ "organizationId": "a" }), json!({ "organizationId": "b" })];
		assert_eq!(trimmed(pick_default(&no_default).and_then(|entry| entry.get("organizationId"))).as_deref(), Some("a"));
	}
}
