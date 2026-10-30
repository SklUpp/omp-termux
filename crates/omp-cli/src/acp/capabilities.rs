//! Advertised ACP surface: `initialize`, `authenticate`, speech catalogue and
//! `_omp/*` extension payloads.
//!
//! Ground truth is `modes/acp/acp-agent.ts` (`initialize` 631-675,
//! `authenticate` 678-689, `extMethod` 1128-1199) plus the speech registries
//! (`stt/models.ts`, `tts/models.ts`). Only what this build actually serves is
//! advertised: `loadSession`, MCP `http`/`sse`, embedded context/images and the
//! list/fork/resume/close session capabilities. No audio prompt capability, no
//! client-side `fs`/`terminal`/`elicitation` claim (those are client gates).

use serde_json::{Value, json};

use super::protocol::{
	AgentInfo, ClientCapabilities, INTERNAL_ERROR, PROTOCOL_VERSION, RequestError,
};

/// Flag the client passes when it launches this binary for terminal auth
/// (`terminal-auth.ts`).
pub const ACP_TERMINAL_AUTH_FLAG: &str = "--acp-terminal-auth";

/// Auth method ids this build accepts.
pub const AUTH_AGENT: &str = "agent";
pub const AUTH_TERMINAL: &str = "terminal";

/// `initialize` result. Capabilities are the TS-advertised set, which is
/// exactly what the Rust driver serves.
#[must_use]
pub fn initialize_result(client: &ClientCapabilities) -> Value {
	let mut auth_methods = vec![json!({
		"id": AUTH_AGENT,
		"name": "Use existing local credentials",
		"description": "Authenticate via the provider keys/OAuth state already configured under ~/.omp.",
	})];
	if client.auth.terminal {
		auth_methods.push(json!({
			"type": AUTH_TERMINAL,
			"id": AUTH_TERMINAL,
			"name": "Set up Oh My Pi in terminal",
			"description": "Run `omp login` to add provider keys and select models.",
			"args": [ACP_TERMINAL_AUTH_FLAG],
		}));
	}
	json!({
		"protocolVersion": PROTOCOL_VERSION,
		"agentInfo": AgentInfo::default(),
		"authMethods": auth_methods,
		"agentCapabilities": {
			"loadSession": true,
			"mcpCapabilities": { "http": true, "sse": true },
			"promptCapabilities": { "embeddedContext": true, "image": true },
			"sessionCapabilities": {
				"list": {},
				"fork": {},
				"resume": {},
				"close": {},
			},
		},
	})
}

/// `authenticate` result.
///
/// `agent` is the TS-exact no-op: credentials are resolved from the existing
/// store at turn time, so there is nothing to verify here. `terminal` is
/// advertised only when the client can launch this binary in a terminal, but
/// this build has no TUI: reporting success would claim setup that did not
/// happen, so it names the out-of-band command instead.
pub fn authenticate(method_id: &str, client: &ClientCapabilities) -> Result<Value, RequestError> {
	match method_id {
		AUTH_AGENT => Ok(json!({})),
		AUTH_TERMINAL if client.auth.terminal => Err(RequestError::new(
			INTERNAL_ERROR,
			"ACP terminal auth requires `omp login`; this build has no interactive TUI",
		)),
		other => Err(RequestError::new(INTERNAL_ERROR, format!("Unknown ACP auth method: {other}"))),
	}
}

/// `speech.models.list` payload.
///
/// Value tables copied from the generated settings surface: the build has no
/// speech backend for ACP, and the client only uses this catalog to render
/// settings pickers.
#[must_use]
pub fn speech_models_catalog() -> Value {
	json!({
		"settings": {
			"speechToTextModel": "stt.modelName",
			"textToSpeechModel": "tts.localModel",
			"textToSpeechVoice": "tts.localVoice",
			"speechVoice": "speech.voice",
		},
		"defaults": {
			"speechToTextModel": "parakeet",
			"textToSpeechModel": "kokoro",
			"voice": "af_heart",
		},
		"speechToText": {
			"setting": "stt.modelName",
			"defaultValue": "parakeet",
			"models": [
				{
					"value": "fast",
					"label": "Fast (Whisper base)",
					"description": "Whisper base, multilingual. Smallest + fastest; lowest accuracy. Best for low-resource machines."
				},
				{
					"value": "balanced",
					"label": "Balanced (Whisper small)",
					"description": "Whisper small, multilingual. More accurate than Fast, still light on CPU/RAM."
				},
				{
					"value": "turbo",
					"label": "Turbo (Whisper large-v3)",
					"description": "Whisper large-v3-turbo, 99 languages. Widest language coverage; large download, slower."
				},
				{
					"value": "parakeet",
					"label": "Parakeet TDT v3 (SoTA)",
					"description": "NVIDIA Parakeet TDT 0.6B v3, 25 languages. Open ASR Leaderboard leader — best accuracy and far fastest decoding. Default."
				}
			],
		},
		"textToSpeech": {
			"modelSetting": "tts.localModel",
			"voiceSetting": "tts.localVoice",
			"speechVoiceSetting": "speech.voice",
			"defaultModel": "kokoro",
			"defaultVoice": "af_heart",
			"models": [
				{
					"value": "kokoro",
					"label": "Kokoro-82M",
					"description": "Kokoro-82M neural TTS — SoTA on-device quality, multi-voice, fully local",
					"voices": tts_voices(),
				}
			],
			"voices": tts_voices(),
		},
	})
}

fn tts_voices() -> Value {
	json!([
		{ "value": "af_heart", "label": "Heart (American female)" },
		{ "value": "af_bella", "label": "Bella (American female)" },
		{ "value": "af_nicole", "label": "Nicole (American female)" },
		{ "value": "af_aoede", "label": "Aoede (American female)" },
		{ "value": "af_kore", "label": "Kore (American female)" },
		{ "value": "af_sarah", "label": "Sarah (American female)" },
		{ "value": "am_michael", "label": "Michael (American male)" },
		{ "value": "am_fenrir", "label": "Fenrir (American male)" },
		{ "value": "am_puck", "label": "Puck (American male)" },
		{ "value": "bf_emma", "label": "Emma (British female)" },
		{ "value": "bm_george", "label": "George (British male)" },
		{ "value": "bm_fable", "label": "Fable (British male)" }
	])
}

/// Extension listing. No extension loader exists in this build; the TS
/// boundary already excludes plugin/extension execution, so the list is empty
/// rather than invented.
#[must_use]
pub fn extensions_payload() -> Value {
	json!({ "extensions": [] })
}

/// Extension toggling reuses the excluded-verb boundary text instead of
/// pretending to enable a provider. `enabled` is echoed only when the toggle
/// actually applies, so neither branch fabricates state.
pub fn extension_toggle_error(provider_id: &str) -> RequestError {
	RequestError::new(
		INTERNAL_ERROR,
		format!(
			"Cannot toggle ACP extension `{provider_id}`: plugin and extension execution is outside the approved boundary"
		),
	)
}

/// Unknown extension method, matching the TS default arm (`-32603` at the
/// transport boundary).
pub fn unknown_ext_method(method: &str) -> RequestError {
	RequestError::new(INTERNAL_ERROR, format!("Unknown ACP ext method: {method}"))
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::acp::protocol::{AGENT_NAME, AGENT_TITLE};

	#[test]
	fn initialize_advertises_exact_ts_surface() {
		let result = initialize_result(&ClientCapabilities::default());
		assert_eq!(result["protocolVersion"], json!(1));
		assert_eq!(result["agentInfo"]["name"], json!(AGENT_NAME));
		assert_eq!(result["agentInfo"]["title"], json!(AGENT_TITLE));
		assert_eq!(result["agentCapabilities"]["loadSession"], json!(true));
		assert_eq!(result["agentCapabilities"]["mcpCapabilities"]["http"], json!(true));
		assert_eq!(result["agentCapabilities"]["promptCapabilities"]["image"], json!(true));
		assert_eq!(result["agentCapabilities"]["sessionCapabilities"]["fork"], json!({}));
		assert_eq!(result["authMethods"].as_array().map(Vec::len), Some(1));
	}

	#[test]
	fn terminal_auth_method_requires_client_capability() {
		let absent = ClientCapabilities::default();
		let result = initialize_result(&absent);
		assert_eq!(result["authMethods"].as_array().map(Vec::len), Some(1));
		assert!(authenticate("terminal", &absent).is_err());

		let present: ClientCapabilities =
			serde_json::from_value(json!({ "auth": { "terminal": true } })).expect("caps");
		let result = initialize_result(&present);
		let methods = result["authMethods"].as_array().expect("methods");
		assert_eq!(methods.len(), 2);
		assert_eq!(methods[1]["id"], json!("terminal"));
		assert!(authenticate("terminal", &present).is_err());
	}

	#[test]
	fn unknown_auth_method_is_typed() {
		let error = authenticate("nope", &ClientCapabilities::default()).expect_err("unknown");
		assert_eq!(error.message, "Unknown ACP auth method: nope");
	}
}
