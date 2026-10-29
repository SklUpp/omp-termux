//! Provider login/refresh implementations.
//!
//! Each provider keeps the upstream flow shape (`registry/*.ts`,
//! `registry/oauth/*.ts`) instead of being forced through one config language:
//! the shared engines (`api_key`, `oauth_code`, `device_flow`) cover every flow
//! whose steps really are the same, and the remaining providers are explicit
//! functions. Callbacks surfaced to the host are `open_url`/`input`/`notice`;
//! tokens never travel through them.

pub mod api_key;
pub mod device_flow;
pub mod oauth_code;

use std::sync::Arc;

use super::callback::with_query;
use super::error::{AuthError, AuthErrorKind};
use super::http::{Cancel, HttpClient};
use super::registry::ProviderEntry;
use super::{InputPrompt, InputReply, LoginCallbacks};

/// Everything a provider flow may touch.
pub struct Flow<'a> {
	pub entry: &'static ProviderEntry,
	pub http: &'a HttpClient,
	pub callbacks: &'a Arc<dyn LoginCallbacks>,
	pub cancel: &'a Cancel,
}

impl Flow<'_> {
	#[must_use]
	pub fn provider(&self) -> &'static str {
		self.entry.id
	}

	pub fn notice(&self, message: impl AsRef<str>) {
		self.callbacks.notice(message.as_ref());
	}

	pub fn check(&self) -> Result<(), AuthError> {
		self.cancel.check(self.entry.id)
	}

	/// Ask for one value. Cancellation is an error, never an empty answer.
	pub fn ask(&self, message: &str, placeholder: &str, allow_empty: bool) -> Result<String, AuthError> {
		self.check()?;
		let prompt = InputPrompt {
			message: message.to_string(),
			placeholder: Some(placeholder.to_string()),
			allow_empty,
		};
		match self.callbacks.input(&prompt, self.cancel) {
			InputReply::Value(value) => {
				self.check()?;
				Ok(value.trim().to_string())
			},
			InputReply::Rejected(message) => Err(AuthError::new(AuthErrorKind::PromptRequired, message)),
			InputReply::Cancelled => Err(AuthError::cancelled(self.entry.id)),
		}
	}

	/// Require a non-empty answer (API keys, enterprise domains, user codes).
	pub fn require(&self, message: &str, placeholder: &str) -> Result<String, AuthError> {
		let value = self.ask(message, placeholder, false)?;
		if value.is_empty() {
			return Err(AuthError::new(AuthErrorKind::Validation, "A value is required").for_provider(self.entry.id));
		}
		Ok(value)
	}

	/// Surface the authorization URL: the host opens the browser, and the plain
	/// URL is always echoed so a headless RPC client can hand it to the user.
	pub fn open(&self, url: &str, launch_url: Option<&str>, instructions: Option<&str>) {
		let info = super::AuthUrlInfo {
			url: url.to_string(),
			launch_url: launch_url.map(ToString::to_string),
			instructions: instructions.map(ToString::to_string),
		};
		if let Err(err) = self.callbacks.open_url(&info) {
			self.notice(format!("Could not open the browser ({err}). Open this URL manually: {url}"));
		}
		if let Some(instructions) = instructions {
			self.notice(instructions);
		}
	}
}

/// Authorization-code request parameters shared by the simple flows.
pub struct AuthorizeParams<'a> {
	pub client_id: &'a str,
	pub redirect_uri: &'a str,
	pub scopes: Option<&'a str>,
	pub pkce_challenge: Option<&'a str>,
	pub state: &'a str,
	pub extra: &'a [(&'a str, &'a str)],
}

/// Build `authorize_url?client_id=…&state=…` with the standard OAuth parameters.
#[must_use]
pub fn authorize_url(base: &str, params: &AuthorizeParams<'_>) -> String {
	let mut query: Vec<(&str, &str)> = vec![
		("client_id", params.client_id),
		("redirect_uri", params.redirect_uri),
		("response_type", "code"),
	];
	if let Some(scopes) = params.scopes {
		query.push(("scope", scopes));
	}
	if let Some(challenge) = params.pkce_challenge {
		query.push(("code_challenge", challenge));
		query.push(("code_challenge_method", "S256"));
	}
	query.push(("state", params.state));
	query.extend_from_slice(params.extra);
	with_query(base, &query)
}

/// `LoginOutcome`-ready credential plus the provider it was stored under.
#[derive(Debug, Clone)]
pub struct LoginResult {
	pub credential: super::store::Credential,
	pub stored_as: String,
}

/// Dispatch a login to its provider implementation.
pub async fn login(flow: &Flow<'_>) -> Result<super::store::Credential, AuthError> {
	if let Some(spec) = flow.entry.api_key {
		return api_key::login(flow, spec).await;
	}
	match flow.entry.id {
		"openai-codex" => oauth_code::codex::login(flow).await,
		"openai-codex-device" => device_flow::codex_device::login(flow).await,
		"anthropic" => oauth_code::anthropic::login(flow).await,
		"zai-coding-plan" => oauth_code::zai::login(flow).await,
		"openrouter" => oauth_code::openrouter::login(flow).await,
		"devin" => oauth_code::devin::login(flow).await,
		"gitlab-duo" => oauth_code::gitlab::login(flow, false).await,
		"gitlab-duo-agent" => oauth_code::gitlab::login(flow, true).await,
		"google-gemini-cli" => oauth_code::google::login(flow, oauth_code::google::Client::GeminiCli).await,
		"google-antigravity" => oauth_code::google::login(flow, oauth_code::google::Client::Antigravity).await,
		"cursor" => oauth_code::cursor::login(flow).await,
		"perplexity" => oauth_code::perplexity::login(flow).await,
		"alibaba-coding-plan" => api_key::alibaba::login(flow, false).await,
		"alibaba-token-plan" => api_key::alibaba::login(flow, true).await,
		"qwen-portal" => api_key::qwen_portal::login(flow).await,
		"cloudflare-ai-gateway" => api_key::cloudflare::login(flow).await,
		"xiaomi" => api_key::xiaomi::login(flow, None).await,
		"xiaomi-token-plan-sgp" => api_key::xiaomi::login(flow, Some(super::registry::XiaomiRegion::Sgp)).await,
		"xiaomi-token-plan-ams" => api_key::xiaomi::login(flow, Some(super::registry::XiaomiRegion::Ams)).await,
		"xiaomi-token-plan-cn" => api_key::xiaomi::login(flow, Some(super::registry::XiaomiRegion::Cn)).await,
		"kimi-code" => device_flow::kimi::login(flow).await,
		"github-copilot" => device_flow::copilot::login(flow).await,
		"kilo" => device_flow::kilo::login(flow).await,
		"xai-oauth" => device_flow::xai::login(flow).await,
		other => Err(AuthError::unknown_provider(other)),
	}
}

/// Dispatch a token refresh. Providers without a refresh grant return
/// `AuthErrorKind::Configuration` rather than a silently reused stale token.
pub async fn refresh(
	flow: &Flow<'_>,
	credentials: &super::store::OAuthCredentials,
) -> Result<super::store::Credential, AuthError> {
	match flow.entry.id {
		"openai-codex" | "openai-codex-device" => oauth_code::codex::refresh(flow, credentials).await,
		"anthropic" => oauth_code::anthropic::refresh(flow, credentials).await,
		"openrouter" => Err(AuthError::config("openrouter", "OpenRouter credentials use a static API key, not a refresh grant")),
		"gitlab-duo" | "gitlab-duo-agent" => oauth_code::gitlab::refresh(flow, credentials).await,
		"google-gemini-cli" | "google-antigravity" => oauth_code::google::refresh(flow, credentials).await,
		"cursor" => oauth_code::cursor::refresh(flow, credentials).await,
		"github-copilot" => device_flow::copilot::refresh(flow, credentials).await,
		"kimi-code" => device_flow::kimi::refresh(flow, credentials).await,
		"xai-oauth" => device_flow::xai::refresh(flow, credentials).await,
		other => Err(AuthError::new(
			AuthErrorKind::Configuration,
			"this provider has no refresh grant; run login again",
		)
		.for_provider(other)),
	}
}

/// Refresh-capable providers (`registry/*.ts` define `refreshToken`).
#[must_use]
pub fn supports_refresh(provider: &str) -> bool {
	matches!(
		provider,
		"openai-codex"
			| "openai-codex-device" | "anthropic"
			| "cursor"
			| "github-copilot"
			| "gitlab-duo"
			| "gitlab-duo-agent"
			| "google-antigravity"
			| "google-gemini-cli"
			| "kimi-code"
			| "xai-oauth"
	)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn authorize_url_orders_parameters_like_upstream() {
		let url = authorize_url(
			"https://example.test/authorize",
			&AuthorizeParams {
				client_id: "cid",
				redirect_uri: "http://localhost:1/callback",
				scopes: Some("api"),
				pkce_challenge: Some("ch"),
				state: "st",
				extra: &[("prompt", "login")],
			},
		);
		assert_eq!(
			url,
			"https://example.test/authorize?client_id=cid&redirect_uri=http%3A%2F%2Flocalhost%3A1%2Fcallback&response_type=code&scope=api&code_challenge=ch&code_challenge_method=S256&state=st&prompt=login"
		);
	}

	#[test]
	fn refresh_capability_matches_upstream() {
		assert!(supports_refresh("anthropic"));
		assert!(supports_refresh("xai-oauth"));
		assert!(!supports_refresh("openrouter"));
		assert!(!supports_refresh("devin"));
	}
}
