//! Authorization-code flows: loopback callback server, optional pasted code, and
//! the provider-specific token exchange.
//!
//! Engine anchors: `callback-server.ts#OAuthCallbackFlow.login` — start the
//! listener, publish the authorize URL, then race the listener against manual
//! paste, honouring cancellation and the 5-minute timeout.

pub mod anthropic;
pub mod codex;
pub mod cursor;
pub mod devin;
pub mod gitlab;
pub mod google;
pub mod openrouter;
pub mod perplexity;
pub mod zai;

use std::sync::mpsc::{self, TryRecvError};
use std::time::{Duration, Instant};

use super::super::callback::{CallbackConfig, CallbackServer, parse_callback_input};
use super::super::error::{AuthError, AuthErrorKind};
use super::super::http::Cancel;
use super::super::pkce::constant_time_eq;
use super::Flow;

/// What the flow needs before the code is exchanged.
pub struct CodeFlowSpec {
	pub callback: CallbackConfig,
	pub instructions: Option<String>,
	/// Prompt shown while waiting; `None` disables the pasted-code race.
	pub manual_prompt: Option<String>,
	pub manual_placeholder: String,
	pub timeout: Duration,
}

impl CodeFlowSpec {
	#[must_use]
	pub fn new(callback: CallbackConfig) -> Self {
		Self {
			callback,
			instructions: None,
			manual_prompt: None,
			manual_placeholder: "http://localhost:.../callback?code=...".to_string(),
			timeout: super::super::callback::DEFAULT_TIMEOUT,
		}
	}

	#[must_use]
	pub fn with_manual(mut self, prompt: &str) -> Self {
		self.manual_prompt = Some(prompt.to_string());
		self
	}

	#[must_use]
	pub fn with_instructions(mut self, instructions: &str) -> Self {
		self.instructions = Some(instructions.to_string());
		self
	}
}

#[derive(Debug, Clone)]
pub struct CodeExchange {
	pub code: String,
	pub state: String,
	pub redirect_uri: String,
}

/// Run the browser/paste wait. `authorize` receives the redirect URI that the
/// listener actually serves and returns the URL to open.
pub async fn run(
	flow: &Flow<'_>,
	spec: &CodeFlowSpec,
	expected_state: &str,
	authorize: impl FnOnce(&str) -> (String, Option<String>),
) -> Result<CodeExchange, AuthError> {
	let provider = flow.provider();
	flow.check()?;
	let server = CallbackServer::start(&spec.callback, expected_state, provider)?;
	let redirect_uri = server.redirect_uri().to_string();
	let (url, extra_instructions) = authorize(&redirect_uri);
	server.set_pending_auth_url(&url);
	let instructions = extra_instructions.or_else(|| spec.instructions.clone());
	flow.open(&url, server.launch_url(), instructions.as_deref());

	// `PASTE_CODE_LOGIN_PROVIDERS` upstream: a provider that declares the paste
	// path always offers it, even when the flow definition forgot a prompt — a
	// browser that cannot reach this loopback listener would otherwise dead-end.
	let manual_prompt = spec.manual_prompt.clone().or_else(|| {
		flow.entry.paste_code.then(|| {
			format!(
				"Paste the authorization code or the full redirect URL for {} if the browser cannot reach this machine",
				flow.entry.name
			)
		})
	});
	let manual = manual_prompt.as_ref().map(|prompt| {
		let (sender, receiver) = mpsc::channel();
		let callbacks = flow.callbacks.clone();
		let cancel = Cancel::new();
		let worker_cancel = cancel.clone();
		let prompt = super::super::InputPrompt {
			message: prompt.clone(),
			placeholder: Some(spec.manual_placeholder.clone()),
			allow_empty: false,
		};
		let worker = std::thread::spawn(move || {
			// Bound to this flow's own token: when the browser callback wins the
			// prompt is abandoned (its RPC dialog dismissed) instead of sitting
			// until the 600 s host budget. The thread owns no listener or token.
			match callbacks.input(&prompt, &worker_cancel) {
				super::super::InputReply::Value(value) => {
					let _ = sender.send(Ok(value));
				},
				super::super::InputReply::Rejected(message) => {
					let _ = sender.send(Err(AuthError::new(AuthErrorKind::PromptRequired, message)));
				},
				super::super::InputReply::Cancelled => {
					let _ = sender.send(Err(AuthError::new(AuthErrorKind::Cancelled, "Login cancelled")));
				},
			}
		});
		ManualWait { receiver, worker: Some(worker), cancel }
	});

	let started = Instant::now();
	// Every exit path runs through `ManualWait::finish`, so a winning browser
	// callback cannot leave the pasted-code prompt (or its worker) behind.
	let result: Result<CodeExchange, AuthError> = async {
		loop {
			flow.check()?;

			if let Some(signal) = server.poll() {
				let outcome = signal?;
				if !constant_time_eq(&outcome.state, expected_state) && !expected_state.is_empty() {
					return Err(
						AuthError::new(AuthErrorKind::Validation, "OAuth state mismatch").for_provider(provider)
					);
				}
				return Ok(CodeExchange { code: outcome.code, state: outcome.state, redirect_uri });
			}

			if let Some(manual) = &manual {
				match manual.receiver.try_recv() {
					Ok(Ok(value)) => {
						let (code, state) = parse_callback_input(&value);
						let code = code
							.map(|code| code.trim().to_string())
							.filter(|code| !code.is_empty())
							.ok_or_else(|| {
								AuthError::new(
									AuthErrorKind::Validation,
									"No authorization code found in the pasted value",
								)
								.for_provider(provider)
							})?;
						if let Some(state) = state.as_deref().filter(|state| !state.is_empty()) {
							if !constant_time_eq(state, expected_state) && !expected_state.is_empty() {
								return Err(
									AuthError::new(AuthErrorKind::Validation, "OAuth state mismatch")
										.for_provider(provider),
								);
							}
						}
						return Ok(CodeExchange { code, state: state.unwrap_or_default(), redirect_uri });
					},
					Ok(Err(err)) => return Err(err.for_provider(provider)),
					Err(TryRecvError::Disconnected) => return Err(AuthError::cancelled(provider)),
					Err(TryRecvError::Empty) => {},
				}
			}

			if started.elapsed() >= spec.timeout {
				return Err(AuthError::new(AuthErrorKind::Timeout, "OAuth flow timed out").for_provider(provider));
			}
			tokio::time::sleep(Duration::from_millis(40)).await;
		}
	}
	.await;
	// `ManualWait::drop` cancels the prompt and joins its worker, so this runs on
	// every exit path - including a future dropped mid-flow.
	drop(manual);
	result
}

/// The pasted-code prompt thread, owned by one `run` call.
///
/// Dropping it cancels the prompt and joins the worker. The join is unbounded on
/// purpose: [`LoginCallbacks::input`] is contractually cooperative, so the worker
/// leaves within its host-side poll after the token fires, and the login never
/// retires with a prompt thread still alive.
struct ManualWait {
	receiver: mpsc::Receiver<Result<String, AuthError>>,
	worker:   Option<std::thread::JoinHandle<()>>,
	cancel:   Cancel,
}

impl Drop for ManualWait {
	fn drop(&mut self) {
		self.cancel.cancel();
		if let Some(worker) = self.worker.take() {
			let _ = worker.join();
		}
	}
}

/// Split `code#state` the way the Anthropic exchange does before posting.
#[must_use]
pub fn split_code_fragment(code: &str, state: &str) -> (String, String) {
	match code.split_once('#') {
		Some((code, fragment)) if !fragment.is_empty() => (code.to_string(), fragment.to_string()),
		Some((code, _)) => (code.to_string(), state.to_string()),
		None => (code.to_string(), state.to_string()),
	}
}

#[cfg(test)]
mod tests {
	use std::io::{Read, Write};
	use std::net::TcpStream;
	use std::sync::atomic::{AtomicBool, Ordering};
	use std::sync::{Arc, Mutex};

	use super::*;
	use crate::auth::http::HttpClient;
	use crate::auth::registry;
	use crate::auth::{AuthUrlInfo, InputPrompt, InputReply, LoginCallbacks};

	const TEST_STATE: &str = "state-1";

	/// Records the pasted-code prompt. `reply == None` means "an open dialog that
	/// only the flow's cancellation token can close".
	#[derive(Default)]
	struct RecordingCallbacks {
		prompt_seen:     AtomicBool,
		prompt_returned: AtomicBool,
		reply:           Mutex<Option<InputReply>>,
	}

	impl LoginCallbacks for RecordingCallbacks {
		fn open_url(&self, _info: &AuthUrlInfo) -> Result<(), String> {
			Ok(())
		}

		fn notice(&self, _message: &str) {}

		fn input(&self, _prompt: &InputPrompt, cancel: &Cancel) -> InputReply {
			self.prompt_seen.store(true, Ordering::SeqCst);
			let reply = self.reply.lock().expect("reply").take().unwrap_or(InputReply::Cancelled);
			if matches!(reply, InputReply::Cancelled) {
				while !cancel.is_cancelled() {
					std::thread::sleep(Duration::from_millis(5));
				}
			}
			self.prompt_returned.store(true, Ordering::SeqCst);
			reply
		}
	}

	fn callback_config() -> CallbackConfig {
		CallbackConfig::new(0, "/callback").with_hostname("127.0.0.1")
	}

	/// One real loopback callback hit, exactly what a browser would do.
	fn hit_callback(redirect_uri: &str, code: &str) {
		let rest = redirect_uri.trim_start_matches("http://");
		let (authority, path) = rest.split_once('/').expect("authority and path");
		let path = format!("/{path}");
		let mut stream = TcpStream::connect(authority).expect("connect loopback callback");
		stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
		let request = format!(
			"GET {path}?code={code}&state={TEST_STATE} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n"
		);
		stream.write_all(request.as_bytes()).expect("write request");
		let mut response = String::new();
		let _ = stream.read_to_string(&mut response);
	}

	#[tokio::test]
	async fn browser_callback_wins_and_joins_the_open_paste_prompt() {
		let entry = registry::find("devin").expect("devin entry");
		let http = HttpClient::new();
		let recorder = Arc::new(RecordingCallbacks::default());
		let callbacks: Arc<dyn LoginCallbacks> = recorder.clone();
		let cancel = Cancel::new();
		let redirect = Arc::new(Mutex::new(None::<String>));
		let driver = {
			let redirect = Arc::clone(&redirect);
			let recorder = Arc::clone(&recorder);
			std::thread::spawn(move || {
				let deadline = Instant::now() + Duration::from_secs(5);
				let uri = loop {
					if let Some(uri) = redirect.lock().expect("redirect").clone() {
						break uri;
					}
					assert!(Instant::now() < deadline, "the flow never published a redirect URI");
					std::thread::sleep(Duration::from_millis(10));
				};
				// The paste prompt must be open before the browser answers, or the
				// flow could finish before its manual thread ever started.
				while !recorder.prompt_seen.load(Ordering::SeqCst) {
					assert!(Instant::now() < deadline, "the paste prompt never opened");
					std::thread::sleep(Duration::from_millis(5));
				}
				hit_callback(&uri, "auth-code");
			})
		};
		let flow = Flow { entry, http: &http, callbacks: &callbacks, cancel: &cancel };
		let mut spec = CodeFlowSpec::new(callback_config());
		spec.manual_prompt = Some("Paste the authorization code".to_string());
		spec.timeout = Duration::from_secs(10);

		let exchange = {
			let redirect = Arc::clone(&redirect);
			run(&flow, &spec, TEST_STATE, move |redirect_uri| {
				*redirect.lock().expect("redirect") = Some(redirect_uri.to_string());
				(
					format!("https://example.invalid/authorize?redirect_uri={redirect_uri}"),
					None,
				)
			})
			.await
			.expect("the browser callback wins")
		};
		driver.join().expect("driver");
		assert_eq!(exchange.code, "auth-code");
		assert!(recorder.prompt_seen.load(Ordering::SeqCst), "the paste prompt was open");
		assert!(
			recorder.prompt_returned.load(Ordering::SeqCst),
			"the prompt worker is joined before the flow returns"
		);
	}

	#[tokio::test]
	async fn pasted_code_wins_when_the_browser_does_not() {
		let entry = registry::find("devin").expect("devin entry");
		let http = HttpClient::new();
		let recorder = Arc::new(RecordingCallbacks::default());
		*recorder.reply.lock().expect("reply") = Some(InputReply::Value(format!("pasted-code#{TEST_STATE}")));
		let callbacks: Arc<dyn LoginCallbacks> = recorder.clone();
		let cancel = Cancel::new();
		let flow = Flow { entry, http: &http, callbacks: &callbacks, cancel: &cancel };
		let mut spec = CodeFlowSpec::new(callback_config());
		spec.manual_prompt = Some("Paste the authorization code".to_string());
		spec.timeout = Duration::from_secs(10);

		let exchange = run(&flow, &spec, TEST_STATE, |redirect_uri| {
			(format!("https://example.invalid/authorize?redirect_uri={redirect_uri}"), None)
		})
		.await
		.expect("the pasted code wins");
		assert_eq!(exchange.code, "pasted-code");
		assert_eq!(exchange.state, TEST_STATE);
		assert!(recorder.prompt_returned.load(Ordering::SeqCst));
	}

	#[tokio::test]
	async fn flow_cancel_closes_the_open_paste_prompt() {
		let entry = registry::find("devin").expect("devin entry");
		let http = HttpClient::new();
		let recorder = Arc::new(RecordingCallbacks::default());
		let callbacks: Arc<dyn LoginCallbacks> = recorder.clone();
		let cancel = Cancel::new();
		let canceller = {
			let recorder = Arc::clone(&recorder);
			let cancel = cancel.clone();
			std::thread::spawn(move || {
				let deadline = Instant::now() + Duration::from_secs(5);
				while !recorder.prompt_seen.load(Ordering::SeqCst) {
					assert!(Instant::now() < deadline, "the pasted-code prompt never opened");
					std::thread::sleep(Duration::from_millis(5));
				}
				cancel.cancel();
			})
		};
		let flow = Flow { entry, http: &http, callbacks: &callbacks, cancel: &cancel };
		let mut spec = CodeFlowSpec::new(callback_config());
		spec.manual_prompt = Some("Paste the authorization code".to_string());
		spec.timeout = Duration::from_secs(10);

		let error = run(&flow, &spec, TEST_STATE, |redirect_uri| {
			(format!("https://example.invalid/authorize?redirect_uri={redirect_uri}"), None)
		})
		.await
		.expect_err("the flow is cancelled");
		canceller.join().expect("canceller");
		assert_eq!(error.kind, AuthErrorKind::Cancelled);
		assert!(recorder.prompt_returned.load(Ordering::SeqCst), "the prompt worker is joined on cancel");
	}

	#[test]
	fn code_fragment_overrides_state_only_when_present() {
		assert_eq!(split_code_fragment("abc#xyz", "state"), ("abc".to_string(), "xyz".to_string()));
		assert_eq!(split_code_fragment("abc#", "state"), ("abc".to_string(), "state".to_string()));
		assert_eq!(split_code_fragment("abc", "state"), ("abc".to_string(), "state".to_string()));
	}
}
