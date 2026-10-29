//! Perplexity login (`registry/oauth/perplexity.ts`).
//!
//! Upstream tries the macOS native app (`defaults read ai.perplexity.mac
//! authToken`) and otherwise runs the HTTP email OTP flow, which needs a cookie
//! jar: `GET /api/auth/csrf` seeds it, and both POSTs echo it back. There is no
//! browser/loopback step and no refresh grant — the JWT is both `access` and
//! `refresh`, expiring only if the claim says so (`NEVER_EXPIRES` otherwise).

use std::collections::BTreeMap;

use serde_json::json;

use super::super::super::error::{AuthError, AuthErrorKind};
use super::super::super::http::{HttpRequest, HttpResponse};
use super::super::super::store::{Credential, OAuthCredentials};
use super::Flow;

const API_VERSION: &str = "2.18";
const APP_USER_AGENT: &str = "Perplexity/641 CFNetwork/1568 Darwin/25.2.0";
const CSRF_URL: &str = "https://www.perplexity.ai/api/auth/csrf";
const SIGNIN_EMAIL_URL: &str = "https://www.perplexity.ai/api/auth/signin-email";
const SIGNIN_OTP_URL: &str = "https://www.perplexity.ai/api/auth/signin-otp";
/// Same sentinel the TS flow uses when the JWT carries no `exp`.
const NEVER_EXPIRES: i64 = 8_640_000_000_000_000;
const EXPIRY_SKEW_MS: i64 = 5 * 60 * 1000;

pub async fn login(flow: &Flow<'_>) -> Result<Credential, AuthError> {
	let mut jar = CookieJar::default();

	// The native-app shortcut is macOS-only upstream (`defaults` binary); on
	// Android there is no such store, so the OTP flow is the only path.
	flow.notice("Fetching Perplexity CSRF token...");
	let csrf = send(flow, &mut jar, HttpRequest::get(CSRF_URL), "CSRF request").await?;
	if !csrf.is_success() {
		return Err(csrf.error_for(AuthErrorKind::Http, "perplexity", "Perplexity CSRF request"));
	}
	let csrf_token = csrf
		.json("perplexity")?
		.get("csrfToken")
		.and_then(serde_json::Value::as_str)
		.map(ToString::to_string)
		.ok_or_else(|| {
			AuthError::new(AuthErrorKind::Validation, "Perplexity CSRF response missing csrfToken")
				.for_provider("perplexity")
		})?;

	let email = flow.require("Enter your Perplexity email address", "user@example.com")?;
	flow.notice("Sending login code to your email...");
	let mut request = HttpRequest::json(
		SIGNIN_EMAIL_URL,
		json!({ "email": email, "csrfToken": csrf_token }),
	)
	.header("X-App-ApiVersion", API_VERSION)
	.header("User-Agent", APP_USER_AGENT);
	jar.apply(&mut request);
	let sent = send(flow, &mut jar, request, "send login code").await?;
	if !sent.is_success() {
		return Err(sent.error_for(AuthErrorKind::Http, "perplexity", "Perplexity send login code"));
	}

	let otp = flow.require("Enter the code sent to your email", "123456")?;
	flow.notice("Verifying login code...");
	let mut request = HttpRequest::json(
		SIGNIN_OTP_URL,
		json!({ "email": email, "otp": otp, "csrfToken": csrf_token }),
	)
	.header("X-App-ApiVersion", API_VERSION)
	.header("User-Agent", APP_USER_AGENT);
	jar.apply(&mut request);
	let verified = send(flow, &mut jar, request, "OTP verification").await?;
	let payload = verified.json("perplexity")?;

	let error_code = payload.get("error_code").and_then(serde_json::Value::as_str).unwrap_or_default();
	let status = payload.get("status").and_then(serde_json::Value::as_str).unwrap_or_default();
	let token = payload
		.get("challenge_token")
		.and_then(serde_json::Value::as_str)
		.filter(|value| !value.is_empty())
		.or_else(|| payload.get("token").and_then(serde_json::Value::as_str))
		.unwrap_or_default()
		.to_string();
	if !verified.is_success() || token.is_empty() || !error_code.is_empty() || (!status.is_empty() && status != "success") {
		let reason = payload
			.get("text")
			.and_then(serde_json::Value::as_str)
			.filter(|value| !value.is_empty())
			.unwrap_or(if error_code.is_empty() { status } else { error_code });
		let reason = if reason.is_empty() { "OTP verification failed".to_string() } else { reason.to_string() };
		return Err(AuthError::new(
			AuthErrorKind::Validation,
			format!("Perplexity OTP verification failed: {reason}"),
		)
		.for_provider("perplexity")
		.with_status(verified.status));
	}

	Ok(Credential::OAuth(OAuthCredentials {
		expires: jwt_expiry(&token),
		access: token.clone(),
		refresh: token,
		email: Some(email.to_lowercase()),
		..OAuthCredentials::default()
	}))
}

async fn send(
	flow: &Flow<'_>,
	jar: &mut CookieJar,
	mut request: HttpRequest,
	operation: &str,
) -> Result<HttpResponse, AuthError> {
	jar.apply(&mut request);
	let response = flow.http.send(request, flow.cancel, "perplexity").await?;
	jar.remember(&response)?;
	if response.status >= 500 {
		return Err(response.error_for(AuthErrorKind::Http, "perplexity", operation));
	}
	Ok(response)
}

/// Minimal RFC 6265 name=value jar: enough for the CSRF/OTP handshake, and no
/// cookie ever leaves the login (nothing is written to disk).
#[derive(Default)]
struct CookieJar {
	values: BTreeMap<String, String>,
}

impl CookieJar {
	fn remember(&mut self, response: &HttpResponse) -> Result<(), AuthError> {
		for header in &response.set_cookies {
			let pair = header.split(';').next().unwrap_or_default().trim();
			let Some((name, value)) = pair.split_once('=') else { continue };
			let name = name.trim();
			if name.is_empty() {
				continue;
			}
			let expired = header
				.split(';')
				.skip(1)
				.any(|attribute| attribute.trim().eq_ignore_ascii_case("max-age=0"));
			if expired {
				self.values.remove(name);
			} else {
				self.values.insert(name.to_string(), value.trim().to_string());
			}
		}
		Ok(())
	}

	fn apply(&self, request: &mut HttpRequest) {
		if self.values.is_empty() {
			return;
		}
		let header = self
			.values
			.iter()
			.map(|(name, value)| format!("{name}={value}"))
			.collect::<Vec<_>>()
			.join("; ");
		request.headers.push(("Cookie".to_string(), header));
	}
}

/// `getJwtExpiry`: an `exp` claim wins (minus the 5-minute safety margin), and
/// a token without one is treated as non-expiring.
fn jwt_expiry(token: &str) -> i64 {
	let Some(exp) = jwt_claims(token).and_then(|payload| payload.get("exp").and_then(serde_json::Value::as_i64)) else {
		return NEVER_EXPIRES;
	};
	exp.saturating_mul(1000).saturating_sub(EXPIRY_SKEW_MS)
}

fn jwt_claims(token: &str) -> Option<serde_json::Value> {
	use base64::Engine as _;
	let payload = token.split('.').nth(1)?;
	let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload).ok()?;
	serde_json::from_slice(&bytes).ok()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn jar_keeps_last_value_and_drops_expired() {
		let mut jar = CookieJar::default();
		jar.remember(&HttpResponse {
			status: 200,
			body: String::new(),
			set_cookies: vec!["a=1; Path=/; HttpOnly".to_string(), "b=2; Path=/".to_string()],
		})
		.expect("jar");
		assert_eq!(jar.values.get("a").map(String::as_str), Some("1"));
		jar.remember(&HttpResponse {
			status: 200,
			body: String::new(),
			set_cookies: vec!["b=; Max-Age=0".to_string()],
		})
		.expect("jar");
		assert!(jar.values.get("b").is_none());

		let mut request = HttpRequest::get("https://example.test");
		jar.apply(&mut request);
		assert!(request.headers.iter().any(|(name, value)| name == "Cookie" && value == "a=1"));
	}

	#[test]
	fn jwt_without_exp_never_expires() {
		assert_eq!(jwt_expiry("opaque-token"), NEVER_EXPIRES);
		assert_eq!(jwt_expiry("a.b.c"), NEVER_EXPIRES);
	}

	#[test]
	fn jwt_exp_is_skewed() {
		use base64::Engine as _;
		let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
		let token = format!("h.{}.s", engine.encode(br#"{"exp":1700000000}"#));
		assert_eq!(jwt_expiry(&token), 1_700_000_000_000 - EXPIRY_SKEW_MS);
	}
}
