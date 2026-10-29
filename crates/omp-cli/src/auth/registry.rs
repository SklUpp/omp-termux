//! Login provider registry: the native mirror of
//! `packages/ai/src/registry/registry.ts:ALL` filtered to `login`-carrying
//! providers (`getOAuthProviders()`), in the same order as the interactive
//! `/login` list.
//!
//! Provider ids, display names, callback ports, paste-code flags and
//! `storeCredentialsAs` remaps are copied from each `registry/*.ts` definition;
//! the API-key prompts/endpoints come from `registry/api-key-login.ts` call
//! sites. The 14 wire/API shapes in `packages/ai/src/dialect` are transport
//! encoders, not login providers, and never appear here.

/// How a provider authenticates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginKind {
	/// Authorization-code flow with a loopback callback and/or pasted code.
	OAuthCode,
	/// RFC 8628 device-code (or provider-specific poll) flow.
	Device,
	/// Long-lived API key/token pasted by the user.
	ApiKey,
	/// Provider-specific interactive sequence that is not one of the above.
	Custom,
}

impl LoginKind {
	#[must_use]
	pub fn auth_type(self) -> &'static str {
		match self {
			Self::OAuthCode => "oauth",
			Self::Device => "device",
			Self::ApiKey | Self::Custom => "api_key",
		}
	}
}

/// API-key validation strategy (`api-key-validation.ts` call sites).
#[derive(Debug, Clone, Copy)]
pub enum Validation {
	/// No probe; the key is stored as pasted.
	None,
	/// `POST {base_url}/chat/completions` with 1 token.
	Chat { provider: &'static str, base_url: &'static str, model: &'static str },
	/// `POST {base_url}/v1/messages` with 1 token.
	Anthropic { provider: &'static str, base_url: &'static str, model: &'static str },
	/// Authenticated `GET {url}`.
	Models { provider: &'static str, url: ModelsUrl, project_header: bool },
	/// `nvidia`: a 401/403 is fatal, any other probe failure is skipped.
	Tolerant { provider: &'static str, base_url: &'static str, model: &'static str },
	/// `xiaomi` / token-plan: try the region endpoints in provider order.
	Xiaomi { token_plan_region: Option<XiaomiRegion> },
}

#[derive(Debug, Clone, Copy)]
pub enum ModelsUrl {
	Static(&'static str),
	/// `$MOONSHOT_BASE_URL` (default `https://api.moonshot.ai/v1`) + `/models`.
	Moonshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XiaomiRegion {
	Sgp,
	Ams,
	Cn,
}

impl XiaomiRegion {
	#[must_use]
	pub fn api_base_url(self) -> &'static str {
		match self {
			Self::Sgp => "https://token-plan-sgp.xiaomimimo.com/v1",
			Self::Ams => "https://token-plan-ams.xiaomimimo.com/v1",
			Self::Cn => "https://token-plan-cn.xiaomimimo.com/v1",
		}
	}

	#[must_use]
	pub fn display_name(self) -> &'static str {
		match self {
			Self::Sgp => "Singapore",
			Self::Ams => "Europe",
			Self::Cn => "China",
		}
	}
}

/// One `createApiKeyLogin({ … })` configuration.
#[derive(Debug, Clone, Copy)]
pub struct ApiKeySpec {
	/// `providerLabel`, used in error messages.
	pub label: &'static str,
	pub auth_url: Option<&'static str>,
	pub instructions: Option<&'static str>,
	pub prompt: &'static str,
	pub placeholder: &'static str,
	/// `allowEmpty` — the prompt accepts an empty answer.
	pub allow_empty: bool,
	/// `emptyKeyFallback`: the value stored when the answer is empty.
	pub empty_fallback: Option<&'static str>,
	pub validation: Validation,
}

/// Long prompt → placeholder pairs used by multi-step logins.
pub type Prompt = (&'static str, &'static str);

#[derive(Debug, Clone, Copy)]
pub struct ProviderEntry {
	pub id: &'static str,
	pub name: &'static str,
	/// Provider id the credential is stored under when it differs.
	pub store_as: Option<&'static str>,
	/// Preferred loopback port; `None` when the flow runs no callback server.
	pub callback_port: Option<u16>,
	/// `pasteCodeFlow`: the redirect cannot always reach this machine.
	pub paste_code: bool,
	pub kind: LoginKind,
	/// Present for `LoginKind::ApiKey`.
	pub api_key: Option<&'static ApiKeySpec>,
}

const fn key(id: &'static str, name: &'static str, spec: &'static ApiKeySpec) -> ProviderEntry {
	ProviderEntry {
		id,
		name,
		store_as: None,
		callback_port: None,
		paste_code: false,
		kind: LoginKind::ApiKey,
		api_key: Some(spec),
	}
}

const fn custom(id: &'static str, name: &'static str) -> ProviderEntry {
	ProviderEntry {
		id,
		name,
		store_as: None,
		callback_port: None,
		paste_code: false,
		kind: LoginKind::Custom,
		api_key: None,
	}
}

const fn oauth(
	id: &'static str,
	name: &'static str,
	port: Option<u16>,
	paste_code: bool,
	store_as: Option<&'static str>,
) -> ProviderEntry {
	ProviderEntry {
		id,
		name,
		store_as,
		callback_port: port,
		paste_code,
		kind: LoginKind::OAuthCode,
		api_key: None,
	}
}

const fn device(id: &'static str, name: &'static str, store_as: Option<&'static str>) -> ProviderEntry {
	ProviderEntry {
		id,
		name,
		store_as,
		callback_port: None,
		paste_code: false,
		kind: LoginKind::Device,
		api_key: None,
	}
}

// ── API-key specs ───────────────────────────────────────────────────────────

const AIAND: ApiKeySpec = ApiKeySpec {
	label: "ai&",
	auth_url: Some("https://console.aiand.com/api-keys"),
	instructions: Some("Copy your API key from the ai& console"),
	prompt: "Paste your ai& API key",
	placeholder: "sk-...",
	allow_empty: false,
	empty_fallback: None,
	validation: Validation::Models {
		provider: "ai&",
		url: ModelsUrl::Static("https://api.aiand.com/v1/models"),
		project_header: false,
	},
};

const XAI_API_KEY: ApiKeySpec = ApiKeySpec {
	label: "xAI",
	auth_url: Some("https://console.x.ai/team/default/api-keys"),
	instructions: Some("Create or copy your API key from the xAI Console"),
	prompt: "Paste your xAI API key",
	placeholder: "xai-...",
	allow_empty: false,
	empty_fallback: None,
	validation: Validation::Models {
		provider: "xAI",
		url: ModelsUrl::Static("https://api.x.ai/v1/models"),
		project_header: false,
	},
};

const ZHIPU_CODING_PLAN: ApiKeySpec = ApiKeySpec {
	label: "Zhipu Coding Plan",
	auth_url: Some("https://bigmodel.cn/coding-plan/personal/overview"),
	instructions: Some("Copy your API key from the Coding Plan dashboard"),
	prompt: "Paste your Zhipu API key",
	placeholder: "<id>.<secret>",
	allow_empty: false,
	empty_fallback: None,
	validation: Validation::Chat {
		provider: "Zhipu",
		base_url: "https://open.bigmodel.cn/api/coding/paas/v4",
		model: "glm-5.1",
	},
};

const UMANS: ApiKeySpec = ApiKeySpec {
	label: "Umans AI Coding Plan",
	auth_url: Some("https://app.umans.ai/billing"),
	instructions: Some("Create or copy your Umans API key from Dashboard → API Keys."),
	prompt: "Paste your Umans API key",
	placeholder: "sk-...",
	allow_empty: false,
	empty_fallback: None,
	validation: Validation::Anthropic {
		provider: "Umans AI Coding Plan",
		base_url: "https://api.code.umans.ai",
		model: "umans-coder",
	},
};

const SAKANA: ApiKeySpec = ApiKeySpec {
	label: "Sakana AI",
	auth_url: Some("https://console.sakana.ai/api-keys"),
	instructions: Some("Copy your API key from the Sakana AI console"),
	prompt: "Paste your Sakana AI API key",
	placeholder: "sk-...",
	allow_empty: false,
	empty_fallback: None,
	validation: Validation::Models {
		provider: "Sakana AI",
		url: ModelsUrl::Static("https://api.sakana.ai/v1/models"),
		project_header: false,
	},
};

const MINIMAX_CODE: ApiKeySpec = ApiKeySpec {
	label: "MiniMax Token Plan",
	auth_url: Some("https://platform.minimax.io/subscribe/token-plan"),
	instructions: Some("Subscribe to Token Plan and copy your API key"),
	prompt: "Paste your MiniMax Token Plan API key",
	placeholder: "sk-...",
	allow_empty: false,
	empty_fallback: None,
	validation: Validation::Chat {
		provider: "MiniMax Token Plan",
		base_url: "https://api.minimax.io/v1",
		model: "MiniMax-M3",
	},
};

const MINIMAX_CODE_CN: ApiKeySpec = ApiKeySpec {
	label: "MiniMax Token Plan",
	auth_url: Some("https://platform.minimaxi.com/subscribe/token-plan"),
	instructions: Some("Subscribe to Token Plan and copy your API key"),
	prompt: "Paste your MiniMax Token Plan API key",
	placeholder: "sk-...",
	allow_empty: false,
	empty_fallback: None,
	validation: Validation::Chat {
		provider: "MiniMax Token Plan (China)",
		base_url: "https://api.minimaxi.com/v1",
		model: "MiniMax-M3",
	},
};

const FIREPASS: ApiKeySpec = ApiKeySpec {
	label: "Fire Pass",
	auth_url: Some("https://app.fireworks.ai/settings/users/api-keys"),
	instructions: Some("Create a dedicated Fire Pass API key in the Fireworks dashboard"),
	prompt: "Paste your Fire Pass API key",
	placeholder: "fpk_...",
	allow_empty: false,
	empty_fallback: None,
	validation: Validation::Chat {
		provider: "Fire Pass",
		base_url: "https://api.fireworks.ai/inference/v1",
		model: "accounts/fireworks/routers/kimi-k2p6-turbo",
	},
};

const DEEPSEEK: ApiKeySpec = ApiKeySpec {
	label: "DeepSeek",
	auth_url: Some("https://platform.deepseek.com/api_keys"),
	instructions: Some("Create or copy your API key from the DeepSeek dashboard"),
	prompt: "Paste your DeepSeek API key",
	placeholder: "sk-...",
	allow_empty: false,
	empty_fallback: None,
	validation: Validation::Models {
		provider: "deepseek",
		url: ModelsUrl::Static("https://api.deepseek.com/v1/models"),
		project_header: false,
	},
};

const META: ApiKeySpec = ApiKeySpec {
	label: "Meta Model API",
	auth_url: Some("https://developer.meta.com/ai/"),
	instructions: Some("Create or copy your key from the Meta Model API dashboard"),
	prompt: "Paste your Meta Model API key",
	placeholder: "Model API key",
	allow_empty: false,
	empty_fallback: None,
	validation: Validation::Models {
		provider: "Meta Model API",
		url: ModelsUrl::Static("https://api.meta.ai/v1/models"),
		project_header: false,
	},
};

const MOONSHOT: ApiKeySpec = ApiKeySpec {
	label: "Moonshot",
	auth_url: Some("https://platform.moonshot.ai/console/api-keys"),
	instructions: Some("Copy your API key from the Moonshot dashboard"),
	prompt: "Paste your Moonshot API key",
	placeholder: "sk-...",
	allow_empty: false,
	empty_fallback: None,
	validation: Validation::Models { provider: "moonshot", url: ModelsUrl::Moonshot, project_header: false },
};

const CEREBRAS: ApiKeySpec = ApiKeySpec {
	label: "Cerebras",
	auth_url: Some("https://cloud.cerebras.ai/platform/"),
	instructions: Some("Copy your API key from the Cerebras dashboard"),
	prompt: "Paste your Cerebras API key",
	placeholder: "csk-...",
	allow_empty: false,
	empty_fallback: None,
	validation: Validation::Chat {
		provider: "Cerebras",
		base_url: "https://api.cerebras.ai/v1",
		model: "gpt-oss-120b",
	},
};

const BASETEN: ApiKeySpec = ApiKeySpec {
	label: "Baseten",
	auth_url: Some("https://app.baseten.co/settings/api_keys"),
	instructions: Some("Copy your API key from the Baseten dashboard"),
	prompt: "Paste your Baseten API key",
	placeholder: "bt_...",
	allow_empty: false,
	empty_fallback: None,
	validation: Validation::Models {
		provider: "Baseten",
		url: ModelsUrl::Static("https://inference.baseten.co/v1/models"),
		project_header: false,
	},
};

const FIREWORKS: ApiKeySpec = ApiKeySpec {
	label: "Fireworks",
	auth_url: Some("https://app.fireworks.ai/settings/users/api-keys"),
	instructions: Some("Create or copy your Fireworks API key"),
	prompt: "Paste your Fireworks API key",
	placeholder: "fw_...",
	allow_empty: false,
	empty_fallback: None,
	validation: Validation::Models {
		provider: "Fireworks",
		url: ModelsUrl::Static(
			"https://api.fireworks.ai/v1/accounts/fireworks/models?filter=supports_serverless%3Dtrue&pageSize=1",
		),
		project_header: false,
	},
};

const TOGETHER: ApiKeySpec = ApiKeySpec {
	label: "Together",
	auth_url: Some("https://api.together.xyz/settings/api-keys"),
	instructions: Some("Copy your API key from the Together dashboard"),
	prompt: "Paste your Together API key",
	placeholder: "sk-...",
	allow_empty: false,
	empty_fallback: None,
	validation: Validation::Models {
		provider: "together",
		url: ModelsUrl::Static("https://api.together.xyz/v1/models"),
		project_header: false,
	},
};

const NVIDIA: ApiKeySpec = ApiKeySpec {
	label: "NVIDIA",
	auth_url: Some("https://org.ngc.nvidia.com/setup/personal-keys"),
	instructions: Some("Copy your API key from NVIDIA NGC Personal Keys"),
	prompt: "Paste your NVIDIA API key",
	placeholder: "nvapi-...",
	allow_empty: false,
	empty_fallback: None,
	validation: Validation::Tolerant {
		provider: "nvidia",
		base_url: "https://integrate.api.nvidia.com/v1",
		model: "nvidia/llama-3.1-nemotron-70b-instruct",
	},
};

const NOVITA: ApiKeySpec = ApiKeySpec {
	label: "Novita",
	auth_url: Some("https://novita.ai/settings/key-management"),
	instructions: Some("Create or copy your API key from the Novita dashboard"),
	prompt: "Paste your Novita API key",
	placeholder: "sk_...",
	allow_empty: false,
	empty_fallback: None,
	validation: Validation::Chat {
		provider: "Novita",
		base_url: "https://api.novita.ai/openai/v1",
		model: "moonshotai/kimi-k2.7-code",
	},
};

const DEEPINFRA: ApiKeySpec = ApiKeySpec {
	label: "DeepInfra",
	auth_url: Some("https://deepinfra.com/dash/api_keys"),
	instructions: Some("Create or copy your API key from the DeepInfra dashboard"),
	prompt: "Paste your DeepInfra API key",
	placeholder: "...",
	allow_empty: false,
	empty_fallback: None,
	validation: Validation::Chat {
		provider: "DeepInfra",
		base_url: "https://api.deepinfra.com/v1/openai",
		model: "deepseek-ai/DeepSeek-V4-Flash-0731",
	},
};

const HUGGINGFACE: ApiKeySpec = ApiKeySpec {
	label: "Hugging Face",
	auth_url: Some(
		"https://huggingface.co/settings/tokens/new?ownUserPermissions=inference.serverless.write&tokenType=fineGrained",
	),
	instructions: Some(
		"Create/copy a token with Make calls to Inference Providers permission (usable as HUGGINGFACE_HUB_TOKEN or HF_TOKEN)",
	),
	prompt: "Paste your Hugging Face token (HUGGINGFACE_HUB_TOKEN / HF_TOKEN)",
	placeholder: "hf_...",
	allow_empty: false,
	empty_fallback: None,
	validation: Validation::Chat {
		provider: "Hugging Face",
		base_url: "https://router.huggingface.co/v1",
		model: "openai/gpt-oss-120b",
	},
};

const QIANFAN: ApiKeySpec = ApiKeySpec {
	label: "Qianfan",
	auth_url: Some("https://console.bce.baidu.com/qianfan/ais/console/apiKey"),
	instructions: Some("Copy your Qianfan API key from the console"),
	prompt: "Paste your Qianfan API key",
	placeholder: "bce-v3/ALTAK-...",
	allow_empty: false,
	empty_fallback: None,
	validation: Validation::Chat {
		provider: "qianfan",
		base_url: "https://qianfan.baidubce.com/v2",
		model: "deepseek-v3.2",
	},
};

const VENICE: ApiKeySpec = ApiKeySpec {
	label: "Venice",
	auth_url: Some("https://venice.ai/settings/api"),
	instructions: Some("Copy your API key from the Venice dashboard"),
	prompt: "Paste your Venice API key",
	placeholder: "vapi_...",
	allow_empty: false,
	empty_fallback: None,
	validation: Validation::Chat {
		provider: "Venice",
		base_url: "https://api.venice.ai/api/v1",
		model: "qwen3-4b",
	},
};

const SILICONFLOW: ApiKeySpec = ApiKeySpec {
	label: "SiliconFlow",
	auth_url: Some("https://cloud.siliconflow.com/account/ak"),
	instructions: Some("Create or copy your API key from the SiliconFlow console"),
	prompt: "Paste your SiliconFlow API key",
	placeholder: "sk-...",
	allow_empty: false,
	empty_fallback: None,
	validation: Validation::Models {
		provider: "siliconflow",
		url: ModelsUrl::Static("https://api.siliconflow.com/v1/models"),
		project_header: false,
	},
};

const SILICONFLOW_CN: ApiKeySpec = ApiKeySpec {
	label: "SiliconFlow (China)",
	auth_url: Some("https://cloud.siliconflow.cn/account/ak"),
	instructions: Some("Create or copy your API key from the SiliconFlow console"),
	prompt: "Paste your SiliconFlow API key",
	placeholder: "sk-...",
	allow_empty: false,
	empty_fallback: None,
	validation: Validation::Models {
		provider: "siliconflow-cn",
		url: ModelsUrl::Static("https://api.siliconflow.cn/v1/models"),
		project_header: false,
	},
};

const SYNTHETIC: ApiKeySpec = ApiKeySpec {
	label: "Synthetic",
	auth_url: Some("https://dev.synthetic.new/docs/api/overview"),
	instructions: Some("Copy your API key from the Synthetic dashboard"),
	prompt: "Paste your Synthetic API key",
	placeholder: "sk-...",
	allow_empty: false,
	empty_fallback: None,
	validation: Validation::Models {
		provider: "Synthetic",
		url: ModelsUrl::Static("https://api.synthetic.new/openai/v1/models"),
		project_header: false,
	},
};

const NANOGPT: ApiKeySpec = ApiKeySpec {
	label: "NanoGPT",
	auth_url: Some("https://nano-gpt.com/api"),
	instructions: Some("Create or copy your NanoGPT API key"),
	prompt: "Paste your NanoGPT API key",
	placeholder: "sk-...",
	allow_empty: false,
	empty_fallback: None,
	validation: Validation::Models {
		provider: "NanoGPT",
		url: ModelsUrl::Static("https://nano-gpt.com/api/v1/models"),
		project_header: false,
	},
};

const WAFER_SERVERLESS: ApiKeySpec = ApiKeySpec {
	label: "Wafer Serverless",
	auth_url: Some("https://app.wafer.ai/usage"),
	instructions: Some("Create or copy your Wafer Serverless API key from the Wafer dashboard"),
	prompt: "Paste your Wafer Serverless API key",
	placeholder: "wfr_...",
	allow_empty: false,
	empty_fallback: None,
	validation: Validation::Models {
		provider: "Wafer Serverless",
		url: ModelsUrl::Static("https://pass.wafer.ai/v1/models"),
		project_header: false,
	},
};

const COREWEAVE: ApiKeySpec = ApiKeySpec {
	label: "CoreWeave Serverless Inference",
	auth_url: Some("https://wandb.ai/settings"),
	instructions: Some(
		"Create or select a CoreWeave Serverless Inference project, add export COREWEAVE_PROJECT=<team>/<project> to your shell startup file (for example ~/.zshrc, ~/.bashrc, or your shell's profile/rc file) for the OpenAI-Project header, then copy your API key from account settings",
	),
	prompt: "Paste your CoreWeave Serverless Inference API key",
	placeholder: "api-key",
	allow_empty: false,
	empty_fallback: None,
	validation: Validation::Models {
		provider: "CoreWeave Serverless Inference",
		url: ModelsUrl::Static("https://api.inference.wandb.ai/v1/models"),
		project_header: true,
	},
};

const VERCEL_AI_GATEWAY: ApiKeySpec = ApiKeySpec {
	label: "Vercel AI Gateway",
	auth_url: Some(
		"https://vercel.com/d?to=%2F%5Bteam%5D%2F%7E%2Fai-gateway%2Fapi-keys&title=AI+Gateway+API+Keys",
	),
	instructions: Some("Copy your Vercel AI Gateway API key from the Vercel dashboard"),
	prompt: "Paste your Vercel AI Gateway API key",
	placeholder: "vck_...",
	allow_empty: false,
	empty_fallback: None,
	validation: Validation::None,
};

const LITELLM: ApiKeySpec = ApiKeySpec {
	label: "LiteLLM",
	auth_url: Some("https://docs.litellm.ai/docs/proxy/deploy"),
	instructions: Some(
		"Run LiteLLM proxy (default http://localhost:4000/v1; set LITELLM_BASE_URL to customize it), then copy your master key or virtual key",
	),
	prompt: "Paste your LiteLLM API key (master key or virtual key)",
	placeholder: "sk-...",
	allow_empty: false,
	empty_fallback: None,
	validation: Validation::None,
};

const ZENMUX: ApiKeySpec = ApiKeySpec {
	label: "ZenMux",
	auth_url: Some("https://zenmux.ai/settings/keys"),
	instructions: Some("Create or copy your ZenMux API key"),
	prompt: "Paste your ZenMux API key",
	placeholder: "sk-...",
	allow_empty: false,
	empty_fallback: None,
	validation: Validation::Models {
		provider: "ZenMux",
		url: ModelsUrl::Static("https://zenmux.ai/api/v1/models"),
		project_header: false,
	},
};

const OPENCODE_ZEN: ApiKeySpec = ApiKeySpec {
	label: "OpenCode Zen",
	auth_url: Some("https://opencode.ai/auth"),
	instructions: Some("Log in to the OpenCode Zen console and copy your OpenCode Zen API key"),
	prompt: "Paste your OpenCode Zen API key",
	placeholder: "sk-...",
	allow_empty: false,
	empty_fallback: None,
	validation: Validation::None,
};

const OPENCODE_GO: ApiKeySpec = ApiKeySpec {
	label: "OpenCode Go",
	auth_url: Some("https://opencode.ai/auth"),
	instructions: Some("Log in to the OpenCode Zen console and copy your OpenCode Go API key"),
	prompt: "Paste your OpenCode Go API key",
	placeholder: "sk-...",
	allow_empty: false,
	empty_fallback: None,
	validation: Validation::None,
};

const YOLO_AUTO: ApiKeySpec = ApiKeySpec {
	label: "Yolo-Auto",
	auth_url: Some("https://yolo-auto.com/app"),
	instructions: Some("Create or copy your Yolo-Auto API key (yolo_...)"),
	prompt: "Paste your Yolo-Auto API key",
	placeholder: "yolo_...",
	allow_empty: false,
	empty_fallback: None,
	validation: Validation::Models {
		provider: "Yolo-Auto",
		url: ModelsUrl::Static("https://yolo-auto.com/v1/models"),
		project_header: false,
	},
};

const TAVILY: ApiKeySpec = ApiKeySpec {
	label: "Tavily",
	auth_url: Some("https://app.tavily.com/home"),
	instructions: Some("Copy your Tavily API key from the API Keys page."),
	prompt: "Paste your Tavily API key",
	placeholder: "tvly-...",
	allow_empty: false,
	empty_fallback: None,
	validation: Validation::None,
};

const KAGI: ApiKeySpec = ApiKeySpec {
	label: "Kagi",
	auth_url: Some("https://kagi.com/settings/api"),
	instructions: Some(
		"Copy your Kagi Search API key from Kagi API settings. Search API access is beta-only; if unavailable, email support@kagi.com.",
	),
	prompt: "Paste your Kagi API key",
	placeholder: "KG_...",
	allow_empty: false,
	empty_fallback: None,
	validation: Validation::None,
};

const EXA: ApiKeySpec = ApiKeySpec {
	label: "Exa",
	auth_url: Some("https://dashboard.exa.ai/api-keys"),
	instructions: Some("Create or copy your API key from the Exa dashboard."),
	prompt: "Paste your Exa API key",
	placeholder: "API key",
	allow_empty: false,
	empty_fallback: None,
	validation: Validation::None,
};

const PARALLEL: ApiKeySpec = ApiKeySpec {
	label: "Parallel",
	auth_url: Some("https://platform.parallel.ai/settings?tab=api-keys"),
	instructions: Some("Copy your Parallel API key from the Parallel settings page."),
	prompt: "Paste your Parallel API key",
	placeholder: "sk_...",
	allow_empty: false,
	empty_fallback: None,
	validation: Validation::None,
};

const OLLAMA: ApiKeySpec = ApiKeySpec {
	label: "Ollama",
	auth_url: Some("https://github.com/ollama/ollama/blob/main/docs/api.md"),
	instructions: Some(
		"Optional: paste an Ollama API key/token for authenticated hosts. Leave empty for local no-auth mode.",
	),
	prompt: "Paste your Ollama API key/token (optional)",
	placeholder: "ollama-local",
	allow_empty: true,
	empty_fallback: Some(""),
	validation: Validation::None,
};

const OLLAMA_CLOUD: ApiKeySpec = ApiKeySpec {
	label: "Ollama Cloud",
	auth_url: Some("https://ollama.com/settings/keys"),
	instructions: Some("Create an Ollama Cloud API key, then paste it here."),
	prompt: "Paste your Ollama Cloud API key",
	placeholder: "ollama-cloud-api-key",
	allow_empty: false,
	empty_fallback: None,
	validation: Validation::None,
};

const LM_STUDIO: ApiKeySpec = ApiKeySpec {
	label: "lm-studio",
	auth_url: None,
	instructions: None,
	prompt: "Optional: Paste LM Studio API key (to customize endpoint URL, set LM_STUDIO_BASE_URL env var)",
	placeholder: "lm-studio-local",
	allow_empty: true,
	empty_fallback: Some("lm-studio-local"),
	validation: Validation::None,
};

const LLAMA_CPP: ApiKeySpec = ApiKeySpec {
	label: "llama.cpp",
	auth_url: Some("https://github.com/ggml-org/llama.cpp#quick-start"),
	instructions: Some(
		"Paste your llama.cpp API key if your server requires auth. Leave empty for local no-auth mode (default base URL: http://127.0.0.1:8080; set LLAMA_CPP_BASE_URL to customize).",
	),
	prompt: "Paste your llama.cpp API key (optional for local no-auth)",
	placeholder: "llama-cpp-local",
	allow_empty: true,
	empty_fallback: Some("llama-cpp-local"),
	validation: Validation::None,
};

const VLLM: ApiKeySpec = ApiKeySpec {
	label: "vllm",
	auth_url: Some("https://docs.vllm.ai/en/latest/serving/openai_compatible_server.html"),
	instructions: Some(
		"Paste your vLLM API key if your server requires auth. Leave empty for local no-auth mode (default base URL: http://127.0.0.1:8000/v1).",
	),
	prompt: "Paste your vLLM API key (optional for local no-auth)",
	placeholder: "vllm-local",
	allow_empty: true,
	empty_fallback: Some("vllm-local"),
	validation: Validation::None,
};

const ZAI: ApiKeySpec = ApiKeySpec {
	label: "Z.AI",
	auth_url: Some("https://z.ai/manage-apikey/apikey-list"),
	instructions: Some("Copy your API key from the dashboard"),
	prompt: "Paste your Z.AI API key",
	placeholder: "sk-...",
	allow_empty: false,
	empty_fallback: None,
	validation: Validation::Chat {
		provider: "Z.AI",
		base_url: "https://api.z.ai/api/coding/paas/v4",
		model: "glm-5.2",
	},
};

const GMI_CLOUD: ApiKeySpec = ApiKeySpec {
	label: "GMI Cloud",
	auth_url: Some("https://console.gmicloud.ai"),
	instructions: Some("Create or copy your GMI Cloud API key"),
	prompt: "Paste your GMI Cloud API key",
	placeholder: "eyJ...",
	allow_empty: false,
	empty_fallback: None,
	validation: Validation::Models {
		provider: "GMI Cloud",
		url: ModelsUrl::Static("https://api.gmi-serving.com/v1/models"),
		project_header: false,
	},
};

/// Providers whose `login` requires an interactive sequence rather than one
/// pasted value (`Paste`-style logins live in `ApiKeySpec`).
pub const PROVIDERS: &[ProviderEntry] = &[
	oauth(
		"openai-codex",
		"ChatGPT Plus/Pro (Codex Subscription)",
		Some(1455),
		true,
		None,
	),
	oauth("anthropic", "Anthropic (Claude Pro/Max)", Some(54545), true, None),
	key("zai", "Z.AI (GLM Coding Plan)", &ZAI),
	oauth("zai-coding-plan", "Z.AI (GLM Coding Plan · Sign in)", Some(54548), true, Some("zai")),
	device("kimi-code", "Kimi Code", None),
	oauth("openrouter", "OpenRouter", Some(54549), true, None),
	device("github-copilot", "GitHub Copilot", None),
	custom("cursor", "Cursor (Claude, GPT, etc.)"),
	oauth("devin", "Devin", Some(59653), true, None),
	oauth(
		"google-antigravity",
		"Antigravity (Gemini 3, Claude, GPT-OSS)",
		Some(51121),
		true,
		None,
	),
	oauth(
		"google-gemini-cli",
		"Google Cloud Code Assist (Gemini CLI)",
		Some(8085),
		true,
		None,
	),
	device("openai-codex-device", "ChatGPT Plus/Pro (Codex, headless/device)", Some("openai-codex")),
	key("xai", "xAI API", &XAI_API_KEY),
	device("xai-oauth", "xAI Grok OAuth (SuperGrok or X Premium+)", None),
	oauth("gitlab-duo", "GitLab Duo Non-Agentic", Some(8080), true, None),
	oauth("gitlab-duo-agent", "GitLab Duo Agent", Some(8080), true, None),
	custom("alibaba-coding-plan", "Alibaba Coding Plan"),
	custom("alibaba-token-plan", "QwenCloud Token Plan"),
	key("aiand", "ai&", &AIAND),
	key("zhipu-coding-plan", "Zhipu Coding Plan (智谱)", &ZHIPU_CODING_PLAN),
	key("umans", "Umans AI Coding Plan", &UMANS),
	custom("qwen-portal", "Qwen Portal"),
	key("sakana", "Sakana AI", &SAKANA),
	key("minimax-code", "MiniMax Token Plan (International)", &MINIMAX_CODE),
	key("minimax-code-cn", "MiniMax Token Plan (China)", &MINIMAX_CODE_CN),
	custom("xiaomi", "Xiaomi MiMo"),
	custom("xiaomi-token-plan-sgp", "Xiaomi Token Plan (Singapore)"),
	custom("xiaomi-token-plan-ams", "Xiaomi Token Plan (Europe)"),
	custom("xiaomi-token-plan-cn", "Xiaomi Token Plan (China)"),
	key("firepass", "Fire Pass (Fireworks Kimi K2.6 Turbo subscription)", &FIREPASS),
	key("deepseek", "DeepSeek", &DEEPSEEK),
	key("meta", "Meta Model API", &META),
	key("moonshot", "Moonshot (Kimi API)", &MOONSHOT),
	key("cerebras", "Cerebras", &CEREBRAS),
	key("baseten", "Baseten", &BASETEN),
	key("fireworks", "Fireworks", &FIREWORKS),
	key("together", "Together", &TOGETHER),
	key("nvidia", "NVIDIA", &NVIDIA),
	key("novita", "Novita", &NOVITA),
	key("deepinfra", "DeepInfra", &DEEPINFRA),
	key("huggingface", "Hugging Face Inference", &HUGGINGFACE),
	custom("perplexity", "Perplexity (Pro/Max)"),
	key("qianfan", "Qianfan", &QIANFAN),
	key("venice", "Venice", &VENICE),
	key("siliconflow", "SiliconFlow", &SILICONFLOW),
	key("siliconflow-cn", "SiliconFlow (China)", &SILICONFLOW_CN),
	key("synthetic", "Synthetic", &SYNTHETIC),
	key("nanogpt", "NanoGPT", &NANOGPT),
	key("wafer-serverless", "Wafer Serverless (pay-as-you-go)", &WAFER_SERVERLESS),
	key("coreweave", "CoreWeave Serverless Inference", &COREWEAVE),
	key("vercel-ai-gateway", "Vercel AI Gateway", &VERCEL_AI_GATEWAY),
	custom("cloudflare-ai-gateway", "Cloudflare AI Gateway"),
	key("litellm", "LiteLLM", &LITELLM),
	device("kilo", "Kilo Gateway", None),
	key("zenmux", "ZenMux", &ZENMUX),
	key("opencode-zen", "OpenCode Zen", &OPENCODE_ZEN),
	key("opencode-go", "OpenCode Go", &OPENCODE_GO),
	key("yolo-auto", "Yolo-Auto", &YOLO_AUTO),
	key("tavily", "Tavily", &TAVILY),
	key("kagi", "Kagi", &KAGI),
	key("exa", "Exa", &EXA),
	key("parallel", "Parallel", &PARALLEL),
	key("ollama", "Ollama (Local OpenAI-compatible)", &OLLAMA),
	key("ollama-cloud", "Ollama Cloud", &OLLAMA_CLOUD),
	key("lm-studio", "LM Studio (Local OpenAI-compatible)", &LM_STUDIO),
	key("llama.cpp", "llama.cpp (Local OpenAI-compatible)", &LLAMA_CPP),
	key("vllm", "vLLM (Local OpenAI-compatible)", &VLLM),
	key("gmi-cloud", "GMI Cloud", &GMI_CLOUD),
];

#[must_use]
pub fn find(id: &str) -> Option<&'static ProviderEntry> {
	PROVIDERS.iter().find(|provider| provider.id == id)
}

/// Provider whose stored credential backs `id` (`storeCredentialsAs`).
#[must_use]
pub fn storage_provider(entry: &ProviderEntry) -> &'static str {
	entry.store_as.unwrap_or(entry.id)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn registry_matches_upstream_provider_set() {
		// `PROVIDER_REGISTRY.filter(p => p.login && p.showInLoginList !== false)`.
		// 78 `ALL` entries minus 10 without `login` (azure, aimlapi, openai,
		// google, google-vertex, groq, mistral, minimax, amazon-bedrock,
		// bedrock-mantle); no provider sets `showInLoginList: false`.
		assert_eq!(PROVIDERS.len(), 68);
		assert_eq!(PROVIDERS[0].id, "openai-codex");
		assert_eq!(PROVIDERS[1].id, "anthropic");
		assert_eq!(PROVIDERS[PROVIDERS.len() - 1].id, "gmi-cloud");
		let mut ids: Vec<&str> = PROVIDERS.iter().map(|provider| provider.id).collect();
		ids.sort_unstable();
		let original = ids.len();
		ids.dedup();
		assert_eq!(ids.len(), original, "provider ids are unique");
	}

	#[test]
	fn ids_that_differ_from_their_module_are_preserved() {
		assert_eq!(find("gitlab-duo-agent").map(|entry| entry.name), Some("GitLab Duo Agent"));
		assert_eq!(find("zai-coding-plan").and_then(|entry| entry.store_as), Some("zai"));
		assert_eq!(find("openai-codex-device").and_then(|entry| entry.store_as), Some("openai-codex"));
		assert_eq!(find("llama.cpp").map(|entry| entry.kind), Some(LoginKind::ApiKey));
	}

	#[test]
	fn callback_ports_match_auth_broker_map() {
		let ports: Vec<(&str, Option<u16>)> =
			PROVIDERS.iter().filter(|entry| entry.callback_port.is_some()).map(|entry| (entry.id, entry.callback_port)).collect();
		assert_eq!(
			ports,
			vec![
				("openai-codex", Some(1455)),
				("anthropic", Some(54545)),
				("zai-coding-plan", Some(54548)),
				("openrouter", Some(54549)),
				("devin", Some(59653)),
				("google-antigravity", Some(51121)),
				("google-gemini-cli", Some(8085)),
				("gitlab-duo", Some(8080)),
				("gitlab-duo-agent", Some(8080)),
			]
		);
	}

	#[test]
	fn paste_code_flows_match_upstream_set() {
		let paste: Vec<&str> = PROVIDERS.iter().filter(|entry| entry.paste_code).map(|entry| entry.id).collect();
		assert_eq!(
			paste,
			vec![
				"openai-codex",
				"anthropic",
				"zai-coding-plan",
				"openrouter",
				"devin",
				"google-antigravity",
				"google-gemini-cli",
				"gitlab-duo",
				"gitlab-duo-agent",
			]
		);
	}

	#[test]
	fn unknown_provider_is_not_found() {
		assert!(find("definitely-not-a-provider").is_none());
	}
}
