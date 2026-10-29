//! Minimal HTTP surface the login/refresh flows need.
//!
//! The workspace already ships one HTTP client (`reqwest`, rustls-only for
//! Android/bionic) — this module is a thin façade, not a second stack: JSON and
//! form posts, GET, per-request timeouts, and cancellation that drops the
//! in-flight request instead of waiting for it to finish.

use std::sync::{
	Arc,
	atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use serde_json::Value;

use super::error::{AuthError, AuthErrorKind, redact, summarize_error_body};

/// Cooperative cancellation. Created by the host (RPC `cancel`, CLI Ctrl-C) and
/// polled at every gate plus inside in-flight requests. An optional external
/// predicate lets the host-supplied [`super::LoginCallbacks`] own the decision
/// without this module knowing about callbacks.
#[derive(Clone, Default)]
pub struct Cancel {
	flag: Arc<AtomicBool>,
	external: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
}

impl std::fmt::Debug for Cancel {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Cancel").field("cancelled", &self.is_cancelled()).finish()
	}
}

impl Cancel {
	#[must_use]
	pub fn new() -> Self {
		Self::default()
	}

	/// A token that is also cancelled whenever `predicate` returns `true`.
	#[must_use]
	pub fn with_predicate(predicate: Arc<dyn Fn() -> bool + Send + Sync>) -> Self {
		Self { flag: Arc::new(AtomicBool::new(false)), external: Some(predicate) }
	}

	pub fn cancel(&self) {
		self.flag.store(true, Ordering::SeqCst);
	}

	#[must_use]
	pub fn is_cancelled(&self) -> bool {
		self.flag.load(Ordering::SeqCst) || self.external.as_ref().is_some_and(|predicate| predicate())
	}

	pub fn check(&self, provider: &str) -> Result<(), AuthError> {
		if self.is_cancelled() {
			return Err(AuthError::cancelled(provider));
		}
		Ok(())
	}
}

#[derive(Debug, Clone, Default)]
pub struct HttpRequest {
	pub method: &'static str,
	pub url: String,
	pub headers: Vec<(String, String)>,
	pub body: Option<ReqBody>,
	pub timeout: Option<Duration>,
}

#[derive(Debug, Clone)]
pub enum ReqBody {
	Json(Value),
	Form(Vec<(String, String)>),
}

impl HttpRequest {
	#[must_use]
	pub fn get(url: impl Into<String>) -> Self {
		Self { method: "GET", url: url.into(), ..Self::default() }
	}

	#[must_use]
	pub fn json(url: impl Into<String>, body: Value) -> Self {
		Self { method: "POST", url: url.into(), body: Some(ReqBody::Json(body)), ..Self::default() }
	}

	#[must_use]
	pub fn form(url: impl Into<String>, body: Vec<(String, String)>) -> Self {
		Self { method: "POST", url: url.into(), body: Some(ReqBody::Form(body)), ..Self::default() }
	}

	#[must_use]
	pub fn header(mut self, name: &str, value: impl Into<String>) -> Self {
		self.headers.push((name.to_string(), value.into()));
		self
	}

	#[must_use]
	pub fn bearer(mut self, token: &str) -> Self {
		self.headers.push(("Authorization".to_string(), format!("Bearer {token}")));
		self
	}

	#[must_use]
	pub fn timeout(mut self, timeout: Duration) -> Self {
		self.timeout = Some(timeout);
		self
	}
}

#[derive(Debug, Clone)]
pub struct HttpResponse {
	pub status: u16,
	pub body: String,
	/// Raw `Set-Cookie` values, in wire order. Only cookie-driven logins
	/// (Perplexity CSRF/OTP) read them; nothing is persisted.
	pub set_cookies: Vec<String>,
}

impl HttpResponse {
	#[must_use]
	pub fn is_success(&self) -> bool {
		(200..300).contains(&self.status)
	}

	pub fn json(&self, provider: &str) -> Result<Value, AuthError> {
		serde_json::from_str(&self.body).map_err(|err| {
			AuthError::new(
				AuthErrorKind::Validation,
				format!("invalid JSON from provider endpoint: {err}; body={}", summarize_error_body(&self.body)),
			)
			.for_provider(provider)
		})
	}

	/// Non-2xx → typed error whose message never ships the raw body.
	///
	/// Only a 400/401/403 carrying an `invalid_grant`-class identifier marks the
	/// grant dead; rate limits, outages and malformed bodies stay retryable.
	pub fn error_for(&self, kind: AuthErrorKind, provider: &str, operation: &str) -> AuthError {
		let summary = summarize_error_body(&self.body);
		let suffix = if summary.is_empty() { String::new() } else { format!(": {summary}") };
		let error = AuthError::new(kind, format!("{operation} failed (HTTP {}){suffix}", self.status))
			.for_provider(provider)
			.with_status(self.status);
		if matches!(self.status, 400 | 401 | 403)
			&& matches!(
				kind,
				AuthErrorKind::TokenRefresh | AuthErrorKind::TokenExchange | AuthErrorKind::Http | AuthErrorKind::Validation
			)
			&& super::error::is_invalid_grant_body(&self.body)
		{
			return error.with_grant_dead();
		}
		error
	}
}

#[derive(Clone)]
pub struct HttpClient {
	inner: reqwest::Client,
}

impl std::fmt::Debug for HttpClient {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		// A reqwest client can hold cookie/proxy configuration; never print it.
		f.write_str("HttpClient")
	}
}

impl Default for HttpClient {
	fn default() -> Self {
		Self::new()
	}
}

impl HttpClient {
	/// Same TLS backend as the model transport (rustls, no OpenSSL on bionic).
	#[must_use]
	pub fn new() -> Self {
		let inner = reqwest::Client::builder()
			.user_agent(concat!("omp/", env!("CARGO_PKG_VERSION")))
			.build()
			.unwrap_or_default();
		Self { inner }
	}

	/// Send one request, aborting it when `cancel` fires.
	///
	/// reqwest has no external abort handle for a request future, so the future
	/// is raced against a short poll: cancellation drops the future, which closes
	/// the socket — a cancelled login never keeps a token request alive.
	pub async fn send(
		&self,
		request: HttpRequest,
		cancel: &Cancel,
		provider: &str,
	) -> Result<HttpResponse, AuthError> {
		cancel.check(provider)?;
		let mut builder = self
			.inner
			.request(
				reqwest::Method::from_bytes(request.method.as_bytes())
					.map_err(|_| AuthError::config(provider, "unsupported HTTP method"))?,
				&request.url,
			)
			.timeout(request.timeout.unwrap_or(Duration::from_secs(30)));
		for (name, value) in &request.headers {
			builder = builder.header(name, value);
		}
		builder = match request.body {
			None => builder,
			Some(ReqBody::Json(value)) => builder.json(&value),
			Some(ReqBody::Form(fields)) => builder.form(&fields),
		};

		let send = async move {
			let response = builder.send().await.map_err(|err| {
				// `redact` strips credentials from URLs and message fragments.
				AuthError::new(AuthErrorKind::Http, format!("request failed: {}", redact(&err.to_string())))
					.for_provider(provider)
			})?;
			let status = response.status().as_u16();
			let set_cookies = response
				.headers()
				.get_all(reqwest::header::SET_COOKIE)
				.iter()
				.filter_map(|value| value.to_str().ok())
				.map(ToString::to_string)
				.collect();
			// The body read is part of the raced future: cancellation drops the
			// response and closes the socket instead of waiting out the timeout.
			let body = response.text().await.map_err(|err| {
				AuthError::new(AuthErrorKind::Http, format!("cannot read response: {err}")).for_provider(provider)
			})?;
			Ok::<HttpResponse, AuthError>(HttpResponse { status, body, set_cookies })
		};
		tokio::pin!(send);
		loop {
			tokio::select! {
				biased;
				result = &mut send => return result,
				() = tokio::time::sleep(Duration::from_millis(40)) => {
					if cancel.is_cancelled() {
						return Err(AuthError::cancelled(provider));
					}
				}
			}
		}
	}

	/// POST JSON, require 2xx, return the parsed body.
	pub async fn post_json(
		&self,
		url: &str,
		body: Value,
		extra: Vec<(String, String)>,
		cancel: &Cancel,
		provider: &str,
		kind: AuthErrorKind,
	) -> Result<Value, AuthError> {
		let mut request = HttpRequest::json(url, body);
		for (name, value) in extra {
			request.headers.push((name, value));
		}
		let response = self.send(request, cancel, provider).await?;
		if !response.is_success() {
			return Err(response.error_for(kind, provider, "token endpoint request"));
		}
		response.json(provider)
	}

	/// POST `application/x-www-form-urlencoded`, require 2xx, return the parsed body.
	pub async fn post_form(
		&self,
		url: &str,
		fields: Vec<(String, String)>,
		cancel: &Cancel,
		provider: &str,
		kind: AuthErrorKind,
	) -> Result<Value, AuthError> {
		let response = self.send(HttpRequest::form(url, fields), cancel, provider).await?;
		if !response.is_success() {
			return Err(response.error_for(kind, provider, "token endpoint request"));
		}
		response.json(provider)
	}

	pub async fn get_json(
		&self,
		request: HttpRequest,
		cancel: &Cancel,
		provider: &str,
	) -> Result<Value, AuthError> {
		let response = self.send(request, cancel, provider).await?;
		if !response.is_success() {
			return Err(response.error_for(AuthErrorKind::Http, provider, "GET"));
		}
		response.json(provider)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn cancel_is_observable_and_terminal() {
		let cancel = Cancel::new();
		assert!(!cancel.is_cancelled());
		assert!(cancel.check("anthropic").is_ok());
		cancel.cancel();
		assert!(cancel.is_cancelled());
		let err = cancel.check("anthropic").expect_err("cancelled");
		assert_eq!(err.kind, AuthErrorKind::Cancelled);
		assert!(err.message.contains("cancelled"));
	}

	#[test]
	fn http_error_never_echoes_token_body() {
		let response = HttpResponse {
			status: 400,
			body: r#"{"error":"invalid_grant","access_token":"sk-secretvalue"}"#.to_string(),
			set_cookies: Vec::new(),
		};
		let error = response.error_for(AuthErrorKind::TokenExchange, "anthropic", "exchange");
		assert_eq!(error.status, Some(400));
		assert!(error.message.contains("invalid_grant"));
		assert!(!error.message.contains("secretvalue"));
	}
}
