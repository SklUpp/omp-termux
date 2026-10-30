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

#[cfg(test)]
mod tests {
	use std::sync::{Arc, Mutex};

	use super::*;
	use crate::auth::error::AuthErrorKind;
	use crate::auth::http::{Cancel, HttpClient};
	use crate::auth::registry;
	use crate::auth::{AuthUrlInfo, InputPrompt, InputReply, LoginCallbacks};

	/// Records what the host was asked to open/prompt/announce and answers the
	/// pasted-key prompt with one scripted reply.
	#[derive(Default)]
	struct Recorder {
		opened:           Mutex<Vec<AuthUrlInfo>>,
		prompts:          Mutex<Vec<InputPrompt>>,
		notices:          Mutex<Vec<String>>,
		reply:            Mutex<Option<InputReply>>,
		/// Cancel from inside the prompt: the user pasted *and* the login died.
		cancel_on_prompt: bool,
	}

	impl LoginCallbacks for Recorder {
		fn open_url(&self, info: &AuthUrlInfo) -> Result<(), String> {
			self.opened.lock().expect("lock").push(info.clone());
			Ok(())
		}

		fn notice(&self, message: &str) {
			self.notices.lock().expect("lock").push(message.to_string());
		}

		fn input(&self, prompt: &InputPrompt, cancel: &Cancel) -> InputReply {
			self.prompts.lock().expect("lock").push(prompt.clone());
			if self.cancel_on_prompt {
				cancel.cancel();
			}
			self
				.reply
				.lock()
				.expect("lock")
				.take()
				.unwrap_or(InputReply::Cancelled)
		}
	}

	fn scripted(reply: InputReply) -> Arc<Recorder> {
		Arc::new(Recorder { reply: Mutex::new(Some(reply)), ..Recorder::default() })
	}

	/// Every case below stops before the network probe, so no test depends on
	/// `portal.qwen.ai` being reachable.
	async fn attempted_login(
		recorder: &Arc<Recorder>,
		cancel: &Cancel,
	) -> Result<Credential, AuthError> {
		let entry = registry::find("qwen-portal").expect("qwen-portal entry");
		let http = HttpClient::new();
		let callbacks: Arc<dyn LoginCallbacks> = recorder.clone();
		let flow = Flow { entry, http: &http, callbacks: &callbacks, cancel };
		login(&flow).await
	}

	fn notices(recorder: &Recorder) -> Vec<String> {
		recorder.notices.lock().expect("lock").clone()
	}

	fn asked_validation(recorder: &Recorder) -> bool {
		notices(recorder)
			.iter()
			.any(|notice| notice.contains("Validating"))
	}

	#[tokio::test]
	async fn portal_url_and_prompt_match_the_documented_login() {
		let recorder = Arc::new(Recorder::default());
		let cancel = Cancel::new();
		let err = attempted_login(&recorder, &cancel)
			.await
			.expect_err("dismissed prompt");

		assert_eq!(err.kind, AuthErrorKind::Cancelled);
		assert_eq!(err.provider.as_deref(), Some("qwen-portal"));

		let opened = recorder.opened.lock().expect("lock").clone();
		assert_eq!(opened.len(), 1, "one portal page, opened once");
		assert_eq!(opened[0].url, AUTH_URL);
		assert_eq!(opened[0].launch_url, None, "the portal page is the only launch target");
		assert_eq!(opened[0].instructions.as_deref(), Some("Copy your Qwen OAuth token or API key"));

		let prompts = recorder.prompts.lock().expect("lock").clone();
		assert_eq!(prompts.len(), 1);
		assert_eq!(prompts[0].message, "Paste your Qwen OAuth token or API key");
		assert_eq!(prompts[0].placeholder.as_deref(), Some("sk-..."));
		assert!(!prompts[0].allow_empty, "an empty Qwen key is never a valid answer");
	}

	#[tokio::test]
	async fn whitespace_paste_is_rejected_as_empty_and_never_probed() {
		let recorder = scripted(InputReply::Value(" \n\t ".to_string()));
		let cancel = Cancel::new();
		let err = attempted_login(&recorder, &cancel)
			.await
			.expect_err("blank key");

		assert_eq!(err.kind, AuthErrorKind::Validation);
		assert_eq!(err.provider.as_deref(), Some("qwen-portal"));
		assert_eq!(err.message, "A value is required");
		assert!(
			!asked_validation(&recorder),
			"a blank paste must not claim the credentials are being validated"
		);
	}

	#[tokio::test]
	async fn headless_host_rejection_is_prompt_required() {
		let recorder =
			scripted(InputReply::Rejected("interactive prompts are unavailable".to_string()));
		let cancel = Cancel::new();
		let err = attempted_login(&recorder, &cancel)
			.await
			.expect_err("rejected prompt");

		assert_eq!(err.kind, AuthErrorKind::PromptRequired);
		assert_eq!(err.message, "interactive prompts are unavailable");
		assert!(!asked_validation(&recorder));
	}

	#[tokio::test]
	async fn cancellation_after_the_paste_never_echoes_the_token() {
		const PASTED: &str = "sk-live-9f2c-do-not-log-me";
		let recorder = Arc::new(Recorder {
			reply: Mutex::new(Some(InputReply::Value(PASTED.to_string()))),
			cancel_on_prompt: true,
			..Recorder::default()
		});
		let cancel = Cancel::new();
		let err = attempted_login(&recorder, &cancel)
			.await
			.expect_err("cancelled mid-prompt");

		assert_eq!(err.kind, AuthErrorKind::Cancelled);
		assert!(
			!err.message.contains("do-not-log-me"),
			"the pasted token is a secret: {}",
			err.kind.as_str()
		);
		assert!(!err.to_string().contains("do-not-log-me"));
		assert!(
			!notices(&recorder)
				.iter()
				.any(|notice| notice.contains("do-not-log-me")),
			"nothing shown to the user echoes the token"
		);
		assert!(!asked_validation(&recorder), "a cancelled login never starts the probe");
	}

	#[tokio::test]
	async fn pre_cancelled_login_opens_the_page_but_asks_nothing() {
		let recorder = Arc::new(Recorder::default());
		let cancel = Cancel::new();
		cancel.cancel();
		let err = attempted_login(&recorder, &cancel)
			.await
			.expect_err("cancelled");

		assert_eq!(err.kind, AuthErrorKind::Cancelled);
		assert_eq!(recorder.opened.lock().expect("lock").len(), 1, "the URL is still surfaced");
		assert!(recorder.prompts.lock().expect("lock").is_empty(), "no dialog outlives the cancel");
	}

	#[test]
	fn probe_targets_the_portals_openai_compatible_endpoint() {
		assert_eq!(API_BASE_URL, "https://portal.qwen.ai/v1", "validate appends /chat/completions");
		assert_eq!(VALIDATION_MODEL, "coder-model");
		assert_eq!(AUTH_URL, "https://chat.qwen.ai");
	}
}
