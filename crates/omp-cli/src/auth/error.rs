//! Typed login/refresh errors.
//!
//! Anchors: `packages/ai/src/error/oauth.ts:1` (`OAuthError{kind,…}`),
//! `packages/ai/src/error/auth-classify.ts:1` (`isDefinitiveOAuthFailure`).
//!
//! Two jobs beyond carrying a message:
//!
//! - **Redaction.** No token, code or secret ever reaches a formatted error.
//!   Token-endpoint bodies are summarised to their `error`/`message` fields and
//!   every long credential-looking run is masked, because the same message may
//!   land in stderr, an RPC frame or a log file.
//! - **Classification.** `is_definitive()` (a stale grant must be disabled) is
//!   distinct from `is_transient()` (a retry may succeed), mirroring
//!   `isDefinitiveOAuthFailure`/`isAuthRetryable`.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthErrorKind {
	/// Non-2xx from an endpoint that is not the token endpoint.
	Http,
	/// Response body/shape did not satisfy the flow.
	Validation,
	TokenExchange,
	TokenRefresh,
	Polling,
	Timeout,
	DeviceAuth,
	Configuration,
	Provisioning,
	Discovery,
	/// User (or host) cancelled; never a login fallback.
	Cancelled,
	/// Provider id absent from the registry: fail closed.
	UnknownProvider,
	ApiKeyRequired,
	PromptRequired,
	Storage,
}

impl AuthErrorKind {
	#[must_use]
	pub fn as_str(self) -> &'static str {
		match self {
			Self::Http => "http",
			Self::Validation => "validation",
			Self::TokenExchange => "token-exchange",
			Self::TokenRefresh => "token-refresh",
			Self::Polling => "polling",
			Self::Timeout => "timeout",
			Self::DeviceAuth => "device-auth",
			Self::Configuration => "configuration",
			Self::Provisioning => "provisioning",
			Self::Discovery => "discovery",
			Self::Cancelled => "cancelled",
			Self::UnknownProvider => "unknown-provider",
			Self::ApiKeyRequired => "api-key-required",
			Self::PromptRequired => "prompt-required",
			Self::Storage => "storage",
		}
	}
}

#[derive(Debug)]
pub struct AuthError {
	pub kind: AuthErrorKind,
	pub provider: Option<String>,
	pub status: Option<u16>,
	pub message: String,
	/// The provider explicitly said the grant is dead (`invalid_grant`,
	/// `invalid_token`, expired refresh token). Only then may a stored
	/// credential be disabled: a 429/5xx/WAF/parse/timeout answer never is.
	pub grant_dead: bool,
}

impl AuthError {
	pub fn new(kind: AuthErrorKind, message: impl AsRef<str>) -> Self {
		Self { kind, provider: None, status: None, message: redact(message.as_ref()), grant_dead: false }
	}

	pub fn for_provider(mut self, provider: &str) -> Self {
		self.provider = Some(provider.to_string());
		self
	}

	pub fn with_status(mut self, status: u16) -> Self {
		self.status = Some(status);
		self
	}

	/// Mark this failure as evidence that the stored grant is unusable.
	pub fn with_grant_dead(mut self) -> Self {
		self.grant_dead = true;
		self
	}

	#[must_use]
	pub fn cancelled(provider: &str) -> Self {
		Self::new(AuthErrorKind::Cancelled, "Login cancelled").for_provider(provider)
	}

	#[must_use]
	pub fn unknown_provider(provider: &str) -> Self {
		Self::new(AuthErrorKind::UnknownProvider, format!("Unknown login provider: {provider}"))
			.for_provider(provider)
	}

	#[must_use]
	pub fn config(provider: &str, message: impl AsRef<str>) -> Self {
		Self::new(AuthErrorKind::Configuration, message).for_provider(provider)
	}

	#[must_use]
	pub fn api_key_required(provider: &str) -> Self {
		Self::new(AuthErrorKind::ApiKeyRequired, "An API key is required").for_provider(provider)
	}

	#[must_use]
	pub fn prompt_required(provider: &str) -> Self {
		Self::new(AuthErrorKind::PromptRequired, "This login requires interactive input")
			.for_provider(provider)
	}

	/// A provider-declared dead grant (`invalid_grant`-class answer on the token
	/// endpoint) or a rejected credential (`401`/`403` with such an identifier).
	///
	/// Rate limits, provider outages, WAF denials, malformed bodies, network and
	/// cancellation failures are **not** definitive: the stored credential must
	/// survive them (`auth-classify.ts`, `flags.ts:253-257`).
	#[must_use]
	pub fn is_definitive(&self) -> bool {
		self.grant_dead
			&& !matches!(
				self.kind,
				AuthErrorKind::Cancelled | AuthErrorKind::Timeout | AuthErrorKind::Polling
			)
	}

	#[must_use]
	pub fn is_transient(&self) -> bool {
		!self.is_definitive()
			&& matches!(
				self.kind,
				AuthErrorKind::Timeout | AuthErrorKind::Polling | AuthErrorKind::Http | AuthErrorKind::Validation
			)
	}
}

impl fmt::Display for AuthError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "{}", self.kind.as_str())?;
		if let Some(provider) = &self.provider {
			write!(f, " ({provider})")?;
		}
		if let Some(status) = self.status {
			write!(f, " HTTP {status}")?;
		}
		write!(f, ": {}", self.message)
	}
}

impl std::error::Error for AuthError {}

impl From<rusqlite::Error> for AuthError {
	fn from(err: rusqlite::Error) -> Self {
		Self::new(AuthErrorKind::Storage, format!("credential store: {}", redact(&err.to_string())))
	}
}

impl From<std::io::Error> for AuthError {
	fn from(err: std::io::Error) -> Self {
		Self::new(AuthErrorKind::Storage, format!("io: {}", redact(&err.to_string())))
	}
}

/// Mask credentials in free-form text.
///
/// Covers the two shapes that actually leak: JSON fields whose name says
/// token/secret/key, and bare bearer/API-key-looking tokens in log lines.
pub fn redact(text: &str) -> String {
	const SECRET_FIELDS: &[&str] =
		&["access_token", "refresh_token", "id_token", "token", "secret", "password", "code", "code_verifier", "key"];

	let mut out = String::with_capacity(text.len());
	let mut cursor = 0;
	while let Some(rel) = text[cursor..].find('"') {
		let open = cursor + rel;
		out.push_str(&text[cursor..=open]);
		let Some(close_rel) = text[open + 1..].find('"') else {
			out.push_str(&text[open + 1..]);
			return redact_bearer_tokens(&out);
		};
		let close = open + 1 + close_rel;
		let quoted = &text[open + 1..close];
		out.push_str(quoted);
		out.push('"');
		cursor = close + 1;
		if !SECRET_FIELDS.contains(&quoted.to_ascii_lowercase().as_str()) {
			continue;
		}
		// Mask the next quoted string, keeping the separators between them.
		let tail = &text[cursor..];
		let Some(next_open_rel) = tail.find('"') else {
			// A secret field with no following quoted value: nothing to mask,
			// and `cursor` cannot advance, so stop instead of re-scanning.
			out.push_str(&text[cursor..]);
			cursor = text.len();
			break;
		};
		out.push_str(&tail[..next_open_rel]);
		let value_open = cursor + next_open_rel;
		let Some(value_close_rel) = text[value_open + 1..].find('"') else {
			out.push_str("[REDACTED]");
			return redact_bearer_tokens(&out);
		};
		out.push_str("[REDACTED]");
		cursor = value_open + 1 + value_close_rel + 1;
	}

	// Quote-free text never entered the loop above: keep the whole remainder.
	out.push_str(&text[cursor..]);
	redact_bearer_tokens(&redact_url_queries(&redact_jwts(&out)))
}

/// Mask secret query parameters (`?code=`, `&access_token=`) in free-form text:
/// a redirect URL echoed by a provider must not ship the grant.
fn redact_url_queries(text: &str) -> String {
	const SECRET_QUERY_KEYS: &[&str] = &[
		"access_token",
		"refresh_token",
		"id_token",
		"token",
		"code",
		"code_verifier",
		"client_secret",
		"secret",
		"password",
		"key",
		"api_key",
		"auth",
	];

	let mut out = String::with_capacity(text.len());
	let mut index = 0usize;
	while index < text.len() {
		let Some(ch) = text[index..].chars().next() else { break };
		if ch != '?' && ch != '&' {
			out.push(ch);
			index += ch.len_utf8();
			continue;
		}
		out.push(ch);
		let start = index + 1;
		let Some(eq_rel) = text[start..].find('=') else {
			out.push_str(&text[start..]);
			break;
		};
		let eq = start + eq_rel;
		let key = &text[start..eq];
		out.push_str(key);
		out.push('=');
		let value_start = eq + 1;
		let value_end = text[value_start..]
			.find(|c: char| c == '&' || c == '#' || c.is_whitespace() || matches!(c, '"' | '\'' | ')'))
			.map(|rel| value_start + rel)
			.unwrap_or(text.len());
		if SECRET_QUERY_KEYS.iter().any(|known| known.eq_ignore_ascii_case(key.trim())) {
			out.push_str("[REDACTED]");
		} else {
			out.push_str(&text[value_start..value_end]);
		}
		index = value_end;
	}
	out
}

/// Mask JWT-shaped runs (`eyJ…`) wherever they appear.
fn redact_jwts(text: &str) -> String {
	let mut out = String::with_capacity(text.len());
	let mut rest = text;
	while let Some(position) = rest.find("eyJ") {
		let (head, tail) = rest.split_at(position);
		out.push_str(head);
		let end = tail
			.find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ',' | ';' | ')' | '}'))
			.unwrap_or(tail.len());
		if end >= 20 {
			out.push_str("[REDACTED-JWT]");
		} else {
			out.push_str(&tail[..end]);
		}
		rest = &tail[end..];
	}
	out.push_str(rest);
	out
}

fn redact_bearer_tokens(text: &str) -> String {
	const SCHEMES: &[&str] = &["Bearer ", "sk-", "sk_", "xai-", "nvapi-", "gho_", "csk-", "fw_", "bt_", "fpk_"];

	let mut out = String::with_capacity(text.len());
	let mut rest = text;
	'outer: while !rest.is_empty() {
		for scheme in SCHEMES {
			if let Some(tail) = rest.strip_prefix(scheme) {
				out.push_str(scheme);
				out.push_str("[REDACTED]");
				let end = tail
					.find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ',' | ';' | ')'))
					.unwrap_or(tail.len());
				rest = &tail[end..];
				continue 'outer;
			}
		}
		let mut chars = rest.chars();
		let Some(first) = chars.next() else { break };
		out.push(first);
		rest = chars.as_str();
	}
	out
}

/// `invalid_grant`-class OAuth identifiers: the provider rejected the stored
/// grant itself, so disabling the row is safe (`auth-classify.ts`).
#[must_use]
pub fn is_invalid_grant_body(body: &str) -> bool {
	const DEAD: &[&str] = &[
		"invalid_grant",
		"invalid_token",
		"invalid_refresh_token",
		"expired_token",
		"invalid_client",
		"unauthorized_client",
	];
	let Ok(json) = serde_json::from_str::<serde_json::Value>(body.trim()) else { return false };
	["error", "error_code", "type"].iter().any(|field| {
		json.get(*field)
			.and_then(serde_json::Value::as_str)
			.map(|code| code.trim().to_ascii_lowercase())
			.is_some_and(|code| DEAD.contains(&code.as_str()))
	})
}

/// OAuth identifiers this client understands. Anything else in an error body
/// is treated as untrusted text: an attacker-shaped "identifier" must not be
/// copied into a message, and a grant is never mistaken for one by shape.
const KNOWN_OAUTH_ERROR_IDENTIFIERS: &[&str] = &[
	"invalid_request",
	"invalid_client",
	"invalid_grant",
	"invalid_grant_type",
	"invalid_token",
	"invalid_refresh_token",
	"expired_token",
	"invalid_scope",
	"invalid_target",
	"unauthorized_client",
	"unsupported_grant_type",
	"unsupported_response_type",
	"invalid_redirect_uri",
	"invalid_code_verifier",
	"invalid_state",
	"access_denied",
	"authorization_pending",
	"slow_down",
	"incorrect_device_code",
	"expired_device_code",
	"device_flow_disabled",
	"login_required",
	"consent_required",
	"interaction_required",
	"server_error",
	"temporarily_unavailable",
	"rate_limit_exceeded",
	"not_found",
	"forbidden",
];

/// The allowlisted lowercase identifier for `text`, if it is one of ours.
pub(crate) fn oauth_error_identifier(text: &str) -> Option<String> {
	let candidate = text.trim().to_ascii_lowercase();
	KNOWN_OAUTH_ERROR_IDENTIFIERS.contains(&candidate.as_str()).then_some(candidate)
}

/// Summarise a token-endpoint error: only allowlisted OAuth identifiers
/// survive. `error_description` is provider prose — it may quote the grant, so
/// it is dropped rather than heuristically filtered. A non-JSON or free-form
/// body is never echoed; the caller reports the operation plus HTTP status.
#[must_use]
pub fn summarize_error_body(body: &str) -> String {
	let trimmed = body.trim();
	if trimmed.is_empty() {
		return String::new();
	}
	let Ok(json) = serde_json::from_str::<serde_json::Value>(trimmed) else {
		return String::new();
	};
	let mut parts: Vec<String> = Vec::new();
	for field in ["error", "error_code", "type"] {
		let Some(code) = json.get(field).and_then(serde_json::Value::as_str).and_then(oauth_error_identifier) else {
			continue;
		};
		if !parts.contains(&code) {
			parts.push(code);
		}
	}
	parts.join(": ")
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn redacts_json_token_fields() {
		let text = r#"{"access_token":"sk-abc123","refresh_token":"r","expires_in":10}"#;
		let redacted = redact(text);
		assert!(!redacted.contains("abc123"));
		assert!(redacted.contains("[REDACTED]"));
		assert!(redacted.contains("expires_in"));
	}

	#[test]
	fn redacts_bearer_and_prefixed_keys() {
		let redacted = redact("Authorization: Bearer eyJhbGciOi.payload.sig next");
		assert!(!redacted.contains("eyJhbGciOi"));
		assert!(redacted.contains("Bearer [REDACTED]"));
	}

	#[test]
	fn summary_keeps_only_error_shape() {
		let body = r#"{"error":"invalid_grant","error_description":"Refresh token expired","access_token":"secret"}"#;
		let summary = summarize_error_body(body);
		assert_eq!(summary, "invalid_grant");
		assert!(!summary.contains("secret"));
	}

	#[test]
	fn only_provider_declared_dead_grants_are_definitive() {
		// Nothing disables a credential unless the provider said it is dead.
		assert!(!AuthError::new(AuthErrorKind::TokenRefresh, "invalid response").is_definitive());
		assert!(!AuthError::new(AuthErrorKind::Validation, "malformed body").is_definitive());
		assert!(!AuthError::new(AuthErrorKind::Http, "busy").with_status(429).is_definitive());
		assert!(!AuthError::new(AuthErrorKind::Http, "down").with_status(503).is_definitive());
		assert!(!AuthError::new(AuthErrorKind::Http, "waf").with_status(403).is_definitive());
		assert!(AuthError::new(AuthErrorKind::TokenRefresh, "revoked").with_grant_dead().is_definitive());
		assert!(!AuthError::cancelled("zai").with_grant_dead().is_definitive());

		assert!(AuthError::new(AuthErrorKind::Http, "busy").with_status(429).is_transient());
		assert!(AuthError::new(AuthErrorKind::Timeout, "slow").is_transient());
	}

	#[test]
	fn invalid_grant_bodies_are_recognised() {
		assert!(is_invalid_grant_body(r#"{"error":"invalid_grant"}"#));
		assert!(is_invalid_grant_body(r#"{"error":"expired_token"}"#));
		assert!(!is_invalid_grant_body(r#"{"error":"rate_limit_exceeded"}"#));
		assert!(!is_invalid_grant_body("<html>blocked</html>"));
	}

	#[test]
	fn free_form_bodies_are_never_echoed() {
		assert_eq!(summarize_error_body("<html>token=abc123</html>"), "");
		assert_eq!(summarize_error_body("plain text secret"), "");
		// A machine-shaped identifier survives; an opaque token does not.
		let body = r#"{"error":"invalid_token","error_description":"handler returned 0123456789abcdef0123456789abcdef"}"#;
		let summary = summarize_error_body(body);
		assert_eq!(summary, "invalid_token");

		let prefixed = r#"{"error":"invalid_grant","error_description":"token sk-abc123 rejected"}"#;
		assert!(!summarize_error_body(prefixed).contains("sk-abc123"));
	}

	#[test]
	fn redacts_pasted_redirect_urls_and_jwts() {
		let pasted = "vscode://gitlab.gitlab-workflow/authentication?code=secret-code&state=nonce";
		let redacted = redact(pasted);
		assert!(!redacted.contains("secret-code"));
		assert!(redacted.contains("state=nonce"));

		let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.c2lnbmF0dXJl";
		assert!(!redact(jwt).contains("eyJhbGciOiJIUzI1NiJ9"));
	}
}
