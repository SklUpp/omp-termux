//! Focused tests for the RPC auth bridge: frame shapes, single-flight login,
//! explicit cancellation, bounded shutdown, and the runtime resolver seam.
//!
//! No test touches the network: the login flows that would open a browser are
//! only ever cancelled or blocked on the store lock before any request.

use std::sync::{Arc, Barrier};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::*;
use crate::rpc::types::RpcExtensionUiResponse;

#[derive(Clone, Default)]
struct FakeSink {
	frames: Arc<Mutex<Vec<Value>>>,
}

impl FakeSink {
	fn new() -> Self {
		Self::default()
	}

	fn frames(&self) -> Vec<Value> {
		lock_poisoned(&self.frames).clone()
	}

	fn find(&self, predicate: impl Fn(&Value) -> bool) -> Option<Value> {
		self.frames().into_iter().find(|frame| predicate(frame))
	}

	fn find_response(&self, command: &str) -> Option<Value> {
		self.find(|frame| frame["type"] == json!("response") && frame["command"] == json!(command))
	}
}

impl FrameSink for FakeSink {
	fn send(&self, frame: Value) {
		lock_poisoned(&self.frames).push(frame);
	}
}

fn bridge() -> (AuthBridge, FakeSink, Arc<ExtensionUiBridge>) {
	let sink = FakeSink::new();
	let sink_dyn: Arc<dyn FrameSink> = Arc::new(sink.clone());
	let ui = Arc::new(ExtensionUiBridge::new(Arc::clone(&sink_dyn)));
	let bridge = AuthBridge::with_backend(
		AuthBackend::in_memory().expect("in-memory store"),
		sink_dyn,
		Arc::clone(&ui),
	);
	(bridge, sink, ui)
}

fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) -> bool {
	let deadline = Instant::now() + timeout;
	while Instant::now() < deadline {
		if predicate() {
			return true;
		}
		std::thread::sleep(Duration::from_millis(10));
	}
	predicate()
}

/// Answer the first `method:"input"` frame with a value or a cancellation.
fn answer_input(ui: Arc<ExtensionUiBridge>, sink: FakeSink, cancelled: bool) -> JoinHandle<()> {
	std::thread::spawn(move || {
		let deadline = Instant::now() + Duration::from_secs(5);
		while Instant::now() < deadline {
			if let Some(frame) = sink.find(|frame| frame["method"] == json!("input")) {
				let id = frame["id"].as_str().expect("input frame id").to_string();
				let _ = ui.handle(RpcExtensionUiResponse {
					id,
					value: (!cancelled).then(|| "pasted-code".to_string()),
					confirmed: None,
					cancelled,
					timed_out: None,
				});
				return;
			}
			std::thread::sleep(Duration::from_millis(10));
		}
	})
}

fn url_info() -> AuthUrlInfo {
	AuthUrlInfo {
		url: "https://app.devin.ai/auth/cli/continue?state=s".to_string(),
		launch_url: Some("http://127.0.0.1:59653/launch".to_string()),
		instructions: Some("Sign in to Devin in your browser.".to_string()),
	}
}

#[test]
fn providers_frame_matches_ts_shape() {
	let (bridge, sink, _ui) = bridge();
	let value = bridge
		.providers(Some("p1"))
		.expect("idle store answers on the calling thread")
		.into_value();
	assert_eq!(value["type"], json!("response"));
	assert_eq!(value["id"], json!("p1"));
	assert_eq!(value["command"], json!("get_login_providers"));
	assert_eq!(value["success"], json!(true));
	let providers = value["data"]["providers"].as_array().expect("providers array");
	assert_eq!(providers.len(), 68, "same filter as proto:login && showInLoginList !== false");
	for provider in providers {
		assert!(provider["id"].is_string());
		assert!(provider["name"].is_string());
		assert_eq!(provider["available"], json!(true));
		assert_eq!(provider["authenticated"], json!(false));
		// The protocol never leaks the login kind or credential material.
		assert!(provider.get("authType").is_none());
		assert!(provider.get("configured").is_none());
	}
	assert!(sink.frames().is_empty(), "a synchronous answer writes no frame");
}

#[test]
fn unknown_provider_is_immediate_error() {
	let (bridge, sink, _ui) = bridge();
	let response = bridge.login(Some("l1"), "nope").expect("immediate rejection");
	let value = response.into_value();
	assert_eq!(value["success"], json!(false));
	assert_eq!(value["command"], json!("login"));
	assert_eq!(value["error"], json!("Unknown OAuth provider: nope"));
	assert!(!bridge.is_logging_in());
	assert!(sink.frames().is_empty(), "no frame is written for a rejected login");
}

#[test]
fn login_is_single_flight_and_credential_commands_defer() {
	let (bridge, sink, _ui) = bridge();
	// Hold the store so the login worker cannot start a flow: the only outcome
	// left is the cancellation path, and no browser or socket is ever opened.
	let guard = lock_backend(&bridge.backend, None, "test").expect("store lock");

	assert!(bridge.login(Some("l1"), "devin").is_none(), "deferred response");
	assert!(bridge.is_logging_in());

	// `get_login_providers` never waits for credential I/O: a login owner yields
	// the typed busy code, with no worker spawned and no stdin stall.
	let started = Instant::now();
	let busy = bridge
		.providers(Some("p1"))
		.expect("typed busy, not a deferred wait")
		.into_value();
	assert!(
		started.elapsed() < Duration::from_millis(500),
		"providers returned without touching the held store lock"
	);
	assert_eq!(busy["success"], json!(false));
	assert_eq!(busy["code"], json!(LOGIN_IN_PROGRESS));
	assert!(
		busy["error"].as_str().expect("message").contains("devin"),
		"busy message names the login owner"
	);

	let second = bridge.login(Some("l2"), "devin").expect("second login rejected");
	let second = second.into_value();
	assert_eq!(second["code"], json!(LOGIN_IN_PROGRESS));
	assert_eq!(second["success"], json!(false));
	assert!(
		second["error"].as_str().expect("message").contains("devin"),
		"busy message names the provider that owns the login"
	);

	assert!(bridge.cancel_login());
	drop(guard);

	assert!(
		wait_until(Duration::from_secs(5), || !bridge.is_logging_in()),
		"cancelled login releases the single-flight slot"
	);
	let response = sink.find_response("login").expect("deferred response was written");
	assert_eq!(response["success"], json!(false));
	assert_eq!(response["id"], json!("l1"));
	assert!(
		response["error"].as_str().expect("error text").to_lowercase().contains("cancel"),
		"cancellation surfaces as an error, never an empty success: {response}"
	);

	assert!(
		sink.find_response("get_login_providers").is_none(),
		"no providers worker outlives a busy rejection"
	);
}

#[test]
fn deferred_providers_is_single_flight_and_tracked_by_shutdown() {
	let (bridge, sink, _ui) = bridge();
	// Simulate a token refresh holding the store: no login owns the slot, so the
	// providers command must defer exactly once.
	let guard = lock_backend(&bridge.backend, None, "test").expect("store lock");

	assert!(bridge.providers(Some("p1")).is_none(), "one deferred scan");
	let second = bridge
		.providers(Some("p2"))
		.expect("second concurrent scan is rejected, not queued")
		.into_value();
	assert_eq!(second["success"], json!(false));
	assert_eq!(second["code"], json!(LOGIN_IN_PROGRESS));
	assert_eq!(second["id"], json!("p2"));
	assert!(
		sink.find_response("get_login_providers").is_none(),
		"nothing is answered while the store is busy"
	);

	// Teardown cancels and joins the deferred scan instead of leaving it parked.
	assert!(bridge.shutdown(Duration::from_secs(5)), "idle login, providers joined");
	let response = sink
		.find_response("get_login_providers")
		.expect("teardown still answers the deferred command");
	assert_eq!(response["id"], json!("p1"));
	assert_eq!(response["success"], json!(false));
	assert!(
		response["error"].as_str().expect("error text").contains("cancelled"),
		"cancellation is a typed error: {response}"
	);

	// The single-flight slot was released by the worker, not leaked.
	drop(guard);
	let fresh = bridge
		.providers(Some("p3"))
		.expect("idle store answers again")
		.into_value();
	assert_eq!(fresh["success"], json!(true));
	assert_eq!(fresh["data"]["providers"].as_array().expect("providers").len(), 68);
}

#[test]
fn cancelled_login_stores_nothing() {
	let (bridge, sink, _ui) = bridge();
	assert!(bridge.login(Some("l1"), "devin").is_none());
	// The flow is cancelled before any exchange, so no credential may persist.
	assert!(bridge.cancel_login());
	assert!(wait_until(Duration::from_secs(5), || !bridge.is_logging_in()));
	assert!(sink.find_response("login").is_some());
	let resolver = bridge.resolver();
	assert_eq!(
		resolver
			.api_key("devin", Some("session-a"), &Cancel::new())
			.expect("lookup"),
		None
	);
}

#[test]
fn shutdown_cancels_and_joins_bounded() {
	let (bridge, sink, _ui) = bridge();
	assert!(bridge.login(None, "devin").is_none());
	assert!(
		bridge.shutdown(Duration::from_secs(5)),
		"shutdown joins a cancelled login within the budget"
	);
	assert!(!bridge.is_logging_in());
	assert!(bridge.shutdown(Duration::from_millis(10)), "idempotent when idle");
	let response = sink.find_response("login").expect("teardown still answers the host");
	assert_eq!(response["success"], json!(false));
}

#[test]
fn callbacks_emit_ts_frames_and_gate_early_prompts() {
	let (bridge, sink, ui) = bridge();
	let callbacks = RpcAuthCallbacks::new("devin", Arc::clone(&bridge.sink), Arc::clone(&ui));

	// Before `onAuth` an interactive provider cannot work over RPC: the prompt is
	// rejected with its reason (a `PromptRequired` login error), never silently
	// cancelled and never turned into a blank value.
	let early = callbacks.input(
		&InputPrompt {
			message: "Custom endpoint".to_string(),
			placeholder: None,
			allow_empty: false,
		},
		&Cancel::new(),
	);
	match early {
		InputReply::Rejected(message) => assert_eq!(
			message,
			"Provider 'devin' requires interactive prompts which are not supported in RPC mode. Use the terminal UI to log in.",
			"rejection text matches rpc-mode.ts"
		),
		other => panic!("expected a rejected prompt, got {other:?}"),
	}
	assert!(
		sink.find(|frame| frame["method"] == json!("notify")).is_none(),
		"the rejection is an error, not an informational notice"
	);

	callbacks.open_url(&url_info()).expect("open_url accepted");
	let frame = sink.find(|frame| frame["method"] == json!("open_url")).expect("open_url frame");
	assert_eq!(frame["type"], json!("extension_ui_request"));
	assert!(frame["id"].as_str().expect("frame id").starts_with("auth-devin-"));
	assert_eq!(frame["url"], json!("https://app.devin.ai/auth/cli/continue?state=s"));
	assert_eq!(frame["launchUrl"], json!("http://127.0.0.1:59653/launch"));
	assert_eq!(frame["instructions"], json!("Sign in to Devin in your browser."));
}

#[test]
fn pasted_code_prompt_returns_the_value() {
	let (bridge, sink, ui) = bridge();
	let callbacks = RpcAuthCallbacks::new("devin", Arc::clone(&bridge.sink), Arc::clone(&ui));
	callbacks.open_url(&url_info()).expect("open_url accepted");
	let responder = answer_input(Arc::clone(&ui), sink.clone(), false);
	let reply = callbacks.input(
		&InputPrompt {
			message: "Paste the authorization code".to_string(),
			placeholder: Some("code".to_string()),
			allow_empty: false,
		},
		&Cancel::new(),
	);
	responder.join().expect("responder");
	assert_eq!(reply, InputReply::Value("pasted-code".to_string()));
}

#[test]
fn cancelled_prompt_is_never_an_empty_value() {
	let (bridge, sink, ui) = bridge();
	let callbacks = RpcAuthCallbacks::new("devin", Arc::clone(&bridge.sink), Arc::clone(&ui));
	callbacks.open_url(&url_info()).expect("open_url accepted");
	let responder = answer_input(Arc::clone(&ui), sink.clone(), true);
	let reply = callbacks.input(
		&InputPrompt {
			message: "Paste the authorization code".to_string(),
			placeholder: Some("code".to_string()),
			allow_empty: true,
		},
		&Cancel::new(),
	);
	responder.join().expect("responder");
	assert_eq!(reply, InputReply::Cancelled, "allow_empty still cannot fake a pasted key");
}

fn prompt() -> InputPrompt {
	InputPrompt {
		message: "Paste the authorization code".to_string(),
		placeholder: Some("code".to_string()),
		allow_empty: false,
	}
}

#[test]
fn login_prompt_cancel_closes_only_its_own_dialog() {
	let (bridge, sink, ui) = bridge();
	// An unrelated host dialog is open while the login prompt is shown.
	let unrelated = {
		let ui = Arc::clone(&ui);
		std::thread::spawn(move || {
			ui.request(
				json!({ "method": "confirm", "title": "Delete?", "message": "Confirm" }),
				Some(Duration::from_secs(10)),
			)
		})
	};
	assert!(wait_until(Duration::from_secs(2), || ui.pending_dialogs() == 1));

	let cancel = Cancel::new();
	let callbacks = Arc::new(RpcAuthCallbacks::new("devin", Arc::clone(&bridge.sink), Arc::clone(&ui)));
	callbacks.open_url(&url_info()).expect("armed");
	let worker = {
		let callbacks: Arc<dyn LoginCallbacks> = callbacks.clone();
		let cancel = cancel.clone();
		std::thread::spawn(move || callbacks.input(&prompt(), &cancel))
	};
	assert!(wait_until(Duration::from_secs(2), || ui.pending_dialogs() == 2));

	cancel.cancel();
	assert_eq!(worker.join().expect("join"), InputReply::Cancelled);
	assert_eq!(ui.pending_dialogs(), 1, "only the login's own dialog was closed");

	let input_id = sink
		.find(|frame| frame["method"] == json!("input"))
		.expect("login prompt frame")["id"]
		.as_str()
		.expect("frame id")
		.to_string();
	let cancel_frame = sink
		.find(|frame| frame["method"] == json!("cancel"))
		.expect("upstream cancel notification");
	assert_eq!(cancel_frame["targetId"], json!(input_id), "cancel targets the login dialog");
	assert_eq!(cancel_frame["type"], json!("extension_ui_request"));

	// Teardown with no active login still touches no dialog.
	assert!(bridge.shutdown(Duration::from_secs(1)));
	assert_eq!(ui.pending_dialogs(), 1, "shutdown without a login closes nothing");

	let confirm_id = sink
		.find(|frame| frame["method"] == json!("confirm"))
		.expect("confirm frame")["id"]
		.as_str()
		.expect("frame id")
		.to_string();
	assert!(ui.handle(RpcExtensionUiResponse {
		id: confirm_id,
		value: None,
		confirmed: Some(true),
		cancelled: false,
		timed_out: None,
	}));
	assert_eq!(ui.pending_dialogs(), 0);
	assert_eq!(unrelated.join().expect("join").expect("answered").confirmed, Some(true));
}

#[test]
fn prompt_cancelled_before_registration_emits_no_dialog() {
	let (bridge, sink, ui) = bridge();
	let callbacks = Arc::new(RpcAuthCallbacks::new("devin", Arc::clone(&bridge.sink), Arc::clone(&ui)));
	callbacks.open_url(&url_info()).expect("armed");
	let callbacks: Arc<dyn LoginCallbacks> = callbacks;
	let cancel = Cancel::new();
	cancel.cancel();
	assert_eq!(callbacks.input(&prompt(), &cancel), InputReply::Cancelled);
	assert_eq!(ui.pending_dialogs(), 0);
	assert!(sink.find(|frame| frame["method"] == json!("input")).is_none(), "no dialog frame is sent");
	assert!(sink.find(|frame| frame["method"] == json!("cancel")).is_none(), "nothing to dismiss");
}

#[test]
fn late_cancel_racing_prompt_registration_never_leaks_a_dialog() {
	// The cancel can land before, during or after the pending-map insert; every
	// interleaving must end with no dialog and no answer for the caller.
	for _ in 0..25 {
		let (bridge, _sink, ui) = bridge();
		let callbacks = Arc::new(RpcAuthCallbacks::new("devin", Arc::clone(&bridge.sink), Arc::clone(&ui)));
		callbacks.open_url(&url_info()).expect("armed");
		let callbacks: Arc<dyn LoginCallbacks> = callbacks;
		let cancel = Cancel::new();
		let barrier = Arc::new(Barrier::new(2));
		let worker = {
			let cancel = cancel.clone();
			let barrier = Arc::clone(&barrier);
			std::thread::spawn(move || {
				barrier.wait();
				callbacks.input(&prompt(), &cancel)
			})
		};
		barrier.wait();
		cancel.cancel();
		assert_eq!(worker.join().expect("join"), InputReply::Cancelled);
		assert_eq!(ui.pending_dialogs(), 0, "cancel never leaves a live dialog behind");
	}
}

#[test]
fn resolver_returns_none_without_a_credential() {
	let (bridge, _sink, _ui) = bridge();
	let resolver = bridge.resolver();
	let cancel = Cancel::new();
	// An empty store plus an absent env key is a genuine absence, never an error.
	// SAFETY: no other test in this module reads this variable.
	let previous = std::env::var("XAI_API_KEY").ok();
	unsafe { std::env::remove_var("XAI_API_KEY") };
	assert_eq!(resolver.api_key("xai", Some("session-a"), &cancel).expect("lookup"), None);
	assert_eq!(resolver.peek_api_key("xai").expect("peek"), None);
	if let Some(previous) = previous {
		// SAFETY: as above.
		unsafe { std::env::set_var("XAI_API_KEY", previous) };
	}
	assert_eq!(resolver.api_key("not-a-provider", None, &cancel).expect("lookup"), None);
}
