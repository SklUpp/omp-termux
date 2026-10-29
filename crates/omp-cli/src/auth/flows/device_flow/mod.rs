//! Device-code logins (`pollOAuthDeviceCodeFlow` consumers).
//!
//! Each provider keeps its own wire shape (`github-copilot` and `openai-codex`
//! use JSON bodies, `kimi`/`xai` the RFC 8628 form encoding, `kilo` a bespoke
//! 202/approved poll) but they share the same host contract: publish the
//! verification URL with the user code, poll until approval, and treat
//! cancellation as a typed `Cancelled` error rather than a timeout.

pub mod codex_device;
pub mod copilot;
pub mod kilo;
pub mod kimi;
pub mod xai;

use serde_json::Value;

use super::Flow;
use super::super::error::{AuthError, AuthErrorKind};

/// `onAuth(... instructions)`: the URL is opened by the host and the code is
/// echoed as a notice so a headless client can still complete the login.
pub(super) fn publish(flow: &Flow<'_>, url: &str, user_code: &str) {
	let instructions = format!("Enter code: {user_code}");
	flow.open(url, None, Some(instructions.as_str()));
}

pub(super) fn string_field(value: &Value, key: &str) -> Option<String> {
	value.get(key).and_then(Value::as_str).map(ToString::to_string)
}

pub(super) fn number_field(value: &Value, key: &str) -> Option<f64> {
	match value.get(key) {
		Some(Value::Number(number)) => number.as_f64(),
		Some(Value::String(text)) => text.trim().parse::<f64>().ok(),
		_ => None,
	}
}

pub(super) fn non_empty(value: Option<String>) -> Option<String> {
	value.map(|value| value.trim().to_string()).filter(|value| !value.is_empty())
}

/// JWT payload without verification — identity decoration only, never a trust
/// decision (`decode_jwt` in the Codex flow does the same).
pub(super) fn jwt_claims(token: &str) -> Option<Value> {
	use base64::Engine as _;
	let payload = token.split('.').nth(1)?;
	let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload).ok()?;
	serde_json::from_slice(&bytes).ok()
}

pub(super) fn now_ms() -> i64 {
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
		.unwrap_or(0)
}

/// Map an OAuth `error` code to the engine's poll signal, or to a fatal error.
pub(super) fn classify_poll_error(provider: &str, code: &str, status: u16) -> AuthError {
	// Only an allowlisted identifier is quoted; provider prose is dropped
	// because it can echo the device code back into a log.
	let detail = crate::auth::error::oauth_error_identifier(code)
		.unwrap_or_else(|| "provider rejected the device request".to_string());
	AuthError::new(AuthErrorKind::Polling, format!("Device flow failed: {detail}"))
		.for_provider(provider)
		.with_status(status)
}

/// Guard used by the providers that poll on their own deadline.
pub(super) fn check(flow: &Flow<'_>) -> Result<(), AuthError> {
	flow.check()
}

/// Sleep in cancellation-aware slices so a cancel is observed promptly.
pub(super) async fn sleep(flow: &Flow<'_>, duration: std::time::Duration) -> Result<(), AuthError> {
	let mut slept = std::time::Duration::ZERO;
	while slept < duration {
		flow.check()?;
		let slice = (duration - slept).min(std::time::Duration::from_millis(100));
		tokio::time::sleep(slice).await;
		slept += slice;
	}
	flow.check()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn jwt_claims_reads_payload() {
		use base64::Engine as _;
		let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
		let token = format!("h.{}.s", engine.encode(br#"{"user_id":"u1","sub":"s1"}"#));
		let claims = jwt_claims(&token).expect("claims");
		assert_eq!(string_field(&claims, "user_id").as_deref(), Some("u1"));
		assert!(jwt_claims("opaque").is_none());
	}

	#[test]
	fn number_field_accepts_string_and_number() {
		let value = serde_json::json!({ "a": 5, "b": "6", "c": "x" });
		assert_eq!(number_field(&value, "a"), Some(5.0));
		assert_eq!(number_field(&value, "b"), Some(6.0));
		assert_eq!(number_field(&value, "c"), None);
	}

	#[test]
	fn poll_error_is_polling_kind_with_status() {
		let err = classify_poll_error("github-copilot", "access_denied", 400);
		assert_eq!(err.kind, AuthErrorKind::Polling);
		assert_eq!(err.status, Some(400));
		assert!(err.message.contains("access_denied"));
	}

	#[test]
	fn poll_error_never_echoes_unlisted_provider_codes() {
		let err = classify_poll_error("github-copilot", "grant 1234567890abcdef", 400);
		assert!(!err.message.contains("1234567890abcdef"));
		assert!(err.message.contains("provider rejected the device request"));
	}
}
