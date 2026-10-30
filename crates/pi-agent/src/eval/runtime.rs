//! Eval settings and the process-wide runtime (bridge server + kernel registry).
//!
//! Settings are always explicit: the module never reads or mutates process
//! environment variables. The parent resolves `eval.py`/`PI_PY`/interpreter
//! settings and passes them in.

use std::{
	collections::HashMap,
	path::{Path, PathBuf},
	time::Instant,
};

use tokio_util::sync::CancellationToken;

use super::{
	bridge::Bridge,
	env::{self, InterpreterRuntime},
	kernel::{Kernel, KernelSpawn},
	languages,
	output::DEFAULT_MAX_BYTES,
	session::{KernelKey, SessionRegistry, normalize_cwd},
	types::{EvalError, EvalLanguage},
	util,
};

/// Tool-level timeout defaults (upstream `TOOL_TIMEOUTS.eval`).
pub const EVAL_DEFAULT_TIMEOUT_SECS: u64 = 30;
pub const EVAL_MIN_TIMEOUT_SECS: u64 = 1;
pub const EVAL_MAX_TIMEOUT_SECS: u64 = 3600;
/// Upstream `eval.autoBackground.thresholdMs` default.
pub const DEFAULT_AUTO_BACKGROUND_THRESHOLD_MS: u64 = 60_000;

/// Enabled backends. Defaults match settings-schema: py/js on, rb/jl off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EvalAllowance {
	pub python: bool,
	pub js: bool,
	pub ruby: bool,
	pub julia: bool,
}

impl Default for EvalAllowance {
	fn default() -> Self {
		Self { python: true, js: true, ruby: false, julia: false }
	}
}

impl EvalAllowance {
	pub const ALL: Self = Self { python: true, js: true, ruby: true, julia: true };

	pub fn enabled(self, language: EvalLanguage) -> bool {
		match language {
			EvalLanguage::Py => self.python,
			EvalLanguage::Js => self.js,
			EvalLanguage::Rb => self.ruby,
			EvalLanguage::Jl => self.julia,
		}
	}

	pub fn set(&mut self, language: EvalLanguage, enabled: bool) {
		match language {
			EvalLanguage::Py => self.python = enabled,
			EvalLanguage::Js => self.js = enabled,
			EvalLanguage::Rb => self.ruby = enabled,
			EvalLanguage::Jl => self.julia = enabled,
		}
	}

	/// Enabled languages in stable display order.
	pub fn languages(self) -> Vec<EvalLanguage> {
		EvalLanguage::ALL.into_iter().filter(|language| self.enabled(*language)).collect()
	}
}

/// Explicit interpreter overrides (`python.interpreter` etc.).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Interpreters {
	pub python: Option<String>,
	pub ruby: Option<String>,
	pub julia: Option<String>,
}

impl Interpreters {
	pub fn new(python: Option<String>, ruby: Option<String>, julia: Option<String>) -> Self {
		Self { python, ruby, julia }
	}

	pub fn for_language(&self, language: EvalLanguage) -> Option<&str> {
		match language {
			EvalLanguage::Py => self.python.as_deref(),
			EvalLanguage::Rb => self.ruby.as_deref(),
			EvalLanguage::Jl => self.julia.as_deref(),
			// Bun is discovered from PATH; there is no JS override.
			EvalLanguage::Js => None,
		}
	}
}

/// Environment flag equivalents (`PI_PY` …), supplied by the parent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EvalFlagOverrides {
	pub python: Option<bool>,
	pub js: Option<bool>,
	pub ruby: Option<bool>,
	pub julia: Option<bool>,
}

impl EvalFlagOverrides {
	pub fn from_values(python: Option<bool>, js: Option<bool>, ruby: Option<bool>, julia: Option<bool>) -> Self {
		Self { python, js, ruby, julia }
	}

	fn resolve(self, language: EvalLanguage, fallback: bool) -> bool {
		match language {
			EvalLanguage::Py => self.python.unwrap_or(fallback),
			EvalLanguage::Js => self.js.unwrap_or(fallback),
			EvalLanguage::Rb => self.ruby.unwrap_or(fallback),
			EvalLanguage::Jl => self.julia.unwrap_or(fallback),
		}
	}
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum KernelMode {
	/// One retained kernel per (language, owner, session, cwd, interpreter).
	#[default]
	Session,
	/// Fresh kernel per call (upstream `per-call`).
	PerCall,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutoBackground {
	pub threshold_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvalSettings {
	allowance: EvalAllowance,
	interpreters: Interpreters,
	default_timeout_secs: u64,
	max_timeout_secs: u64,
	kernel_mode: KernelMode,
	probe_timeout_ms: u64,
	auto_background: Option<AutoBackground>,
	spill_bytes: usize,
	keep_artifacts: bool,
}

impl Default for EvalSettings {
	fn default() -> Self {
		Self {
			allowance: EvalAllowance::default(),
			interpreters: Interpreters::default(),
			default_timeout_secs: EVAL_DEFAULT_TIMEOUT_SECS,
			// `tools.maxTimeout` default: no global cap beyond the tool ceiling.
			max_timeout_secs: 0,
			kernel_mode: KernelMode::Session,
			probe_timeout_ms: 10_000,
			auto_background: None,
			spill_bytes: DEFAULT_MAX_BYTES,
			keep_artifacts: true,
		}
	}
}

impl EvalSettings {
	/// Explicit constructor. `allowance` and `interpreters` are the only
	/// required inputs; every remaining knob has an explicit builder.
	pub fn new(allowance: EvalAllowance, interpreters: Interpreters) -> Self {
		Self { allowance, interpreters, ..Self::default() }
	}

	pub fn allowance(&self) -> EvalAllowance {
		self.allowance
	}

	pub fn interpreters(&self) -> &Interpreters {
		&self.interpreters
	}

	pub fn default_timeout_secs(&self) -> u64 {
		self.default_timeout_secs
	}

	pub fn max_timeout_secs(&self) -> u64 {
		self.max_timeout_secs
	}

	pub fn kernel_mode(&self) -> KernelMode {
		self.kernel_mode
	}

	pub fn probe_timeout_ms(&self) -> u64 {
		self.probe_timeout_ms
	}

	pub fn auto_background(&self) -> Option<AutoBackground> {
		self.auto_background
	}

	pub fn spill_bytes(&self) -> usize {
		self.spill_bytes
	}

	pub fn keep_artifacts(&self) -> bool {
		self.keep_artifacts
	}

	/// Fold `PI_PY`/`PI_JS`/`PI_RB`/`PI_JL` style overrides over settings.
	pub fn with_flag_overrides(mut self, overrides: EvalFlagOverrides) -> Self {
		for language in EvalLanguage::ALL {
			let fallback = self.allowance.enabled(language);
			self.allowance.set(language, overrides.resolve(language, fallback));
		}
		self
	}

	pub fn with_interpreter(mut self, language: EvalLanguage, interpreter: Option<String>) -> Self {
		let interpreter = interpreter.filter(|value| !value.trim().is_empty());
		match language {
			EvalLanguage::Py => self.interpreters.python = interpreter,
			EvalLanguage::Rb => self.interpreters.ruby = interpreter,
			EvalLanguage::Jl => self.interpreters.julia = interpreter,
			EvalLanguage::Js => {},
		}
		self
	}

	pub fn with_timeouts(mut self, default_secs: u64, max_secs: u64) -> Self {
		self.default_timeout_secs = default_secs.max(EVAL_MIN_TIMEOUT_SECS);
		self.max_timeout_secs = max_secs;
		self
	}

	pub fn with_kernel_mode(mut self, mode: KernelMode) -> Self {
		self.kernel_mode = mode;
		self
	}

	pub fn with_probe_timeout_ms(mut self, timeout_ms: u64) -> Self {
		self.probe_timeout_ms = timeout_ms;
		self
	}

	pub fn with_auto_background(mut self, config: Option<AutoBackground>) -> Self {
		self.auto_background = config;
		self
	}

	pub fn with_spill_bytes(mut self, spill_bytes: usize) -> Self {
		self.spill_bytes = spill_bytes.max(1);
		self
	}

	pub fn with_keep_artifacts(mut self, keep: bool) -> Self {
		self.keep_artifacts = keep;
		self
	}

	/// Resolve the per-call timeout, mirroring `clampTimeout("eval", …)`.
	pub fn resolve_timeout_secs(&self, requested: Option<u64>) -> u64 {
		let timeout = requested.unwrap_or(self.default_timeout_secs);
		let capped = if self.max_timeout_secs > 0 {
			timeout.min(self.max_timeout_secs)
		} else {
			timeout
		};
		capped.clamp(EVAL_MIN_TIMEOUT_SECS, EVAL_MAX_TIMEOUT_SECS)
	}

	/// Notice for a model that asked for a disabled backend.
	pub fn disabled_message(language: EvalLanguage) -> String {
		match language {
			EvalLanguage::Py => "Python backend is disabled (PI_PY=0 or eval.py = false).".to_string(),
			EvalLanguage::Js => "JavaScript backend is disabled (PI_JS=0 or eval.js = false).".to_string(),
			EvalLanguage::Rb => "Ruby backend is disabled (PI_RB=0 or eval.rb = false).".to_string(),
			EvalLanguage::Jl => "Julia backend is disabled (PI_JL=0 or eval.jl = false).".to_string(),
		}
	}
}

/// Shared eval runtime held by the tool.
pub struct EvalRuntime {
	settings: EvalSettings,
	bridge: Bridge,
	registry: SessionRegistry,
	bridge_session_id: String,
}

impl EvalRuntime {
	pub async fn start(settings: EvalSettings) -> Result<Self, EvalError> {
		let bridge = Bridge::start().await?;
		Ok(Self {
			settings,
			bridge,
			registry: SessionRegistry::new(),
			bridge_session_id: format!("eval-{}", util::random_token()),
		})
	}

	pub fn settings(&self) -> &EvalSettings {
		&self.settings
	}

	pub fn bridge(&self) -> &Bridge {
		&self.bridge
	}

	pub fn bridge_session_id(&self) -> &str {
		&self.bridge_session_id
	}

	/// Resolve the interpreter (cached) and acquire/replace the kernel for one
	/// owner/session key.
	///
	/// In `per-call` mode a fresh unregistered kernel is started for this
	/// invocation (upstream `executePerCall`); otherwise the retained registry
	/// slot is reused/replaced.
	///
	/// Discovery/startup run under `cancellation` (the caller's real token).
	/// `probe_timeout_ms` bounds a throwaway probe. `cell_deadline` is the one
	/// deadline for the whole cell, anchored at the caller's start time: Julia
	/// readiness is the kernel startup itself, so it draws on that deadline (and
	/// on the remainder for retries), never on the probe budget, and the cell then
	/// executes inside whatever is left.
	pub async fn kernel_for_session(
		&self,
		language: EvalLanguage,
		owner: Option<&str>,
		session_id: &str,
		cwd: &Path,
		ambient_env: &HashMap<String, String>,
		reset: bool,
		probe_timeout_ms: Option<u64>,
		cell_deadline: Option<Instant>,
		cancellation: CancellationToken,
	) -> Result<std::sync::Arc<Kernel>, EvalError> {
		let interpreter = self.settings.interpreters.for_language(language);
		let cwd = normalize_cwd(cwd);
		let endpoint = self.bridge.endpoint();
		let extra_env = HashMap::from([
			("PI_TOOL_BRIDGE_URL".to_string(), endpoint.url.clone()),
			("PI_TOOL_BRIDGE_TOKEN".to_string(), endpoint.token.clone()),
			("PI_TOOL_BRIDGE_SESSION".to_string(), self.bridge_session_id.clone()),
		]);
		let per_call = matches!(self.settings.kernel_mode, KernelMode::PerCall);
		let resolution = languages::resolve_runtime(
			language,
			&cwd,
			ambient_env,
			interpreter,
			&cancellation,
			probe_timeout_ms,
		)
		.await?;
		let runtime = match resolution {
			languages::RuntimeResolution::Probed(runtime) => runtime,
			// Julia: the interpreter boot is the only readiness signal, so the
			// first real kernel doubles as discovery and is then reused.
			languages::RuntimeResolution::Handshake(candidates) => {
				return self
					.julia_kernel(
						candidates,
						&cwd,
						interpreter,
						owner,
						session_id,
						reset,
						per_call,
						cell_deadline,
						extra_env,
						cancellation,
					)
					.await;
			},
		};
		// Upstream `executePerCall` never touches the session registry: each call
		// owns a fresh kernel and shuts it down in `finally`. Sharing a registry
		// slot made concurrent cells reset/dispose each other's kernels.
		if per_call {
			let spawn = KernelSpawn { language, runtime, cwd, extra_env };
			return match Kernel::start(spawn, cancellation.clone()).await {
				Ok(kernel) => Ok(std::sync::Arc::new(kernel)),
				Err(EvalError::Kernel(_)) if cancellation.is_cancelled() => Err(EvalError::Aborted(format!(
					"{} kernel startup was cancelled",
					language.display_name()
				))),
				Err(error) => Err(error),
			};
		}
		let key = KernelKey {
			language,
			owner: owner.map(str::to_string),
			session_id: session_id.to_string(),
			cwd,
			interpreter: Some(runtime.program.display().to_string()),
		};
		match self
			.registry
			.acquire(key, runtime, extra_env, reset, cancellation.clone())
			.await
		{
			Ok(kernel) => Ok(kernel),
			Err(EvalError::Kernel(_)) if cancellation.is_cancelled() => Err(EvalError::Aborted(format!(
				"{} kernel startup was cancelled",
				language.display_name()
			))),
			Err(error) => Err(error),
		}
	}

	/// Start the real Julia kernel as the readiness check.
	///
	/// Julia pays a full sysimage boot for any argv, so a separate probe would
	/// boot it twice. One interpreter startup per candidate is attempted under
	/// the single overall cell deadline (a retry keeps the remainder); the first
	/// successful handshake is cached and, in session mode, retained, so later
	/// cells reuse the live process and a dead-kernel retry reuses the proven
	/// interpreter instead of re-scanning `PATH`.
	#[allow(clippy::too_many_arguments)]
	async fn julia_kernel(
		&self,
		candidates: Vec<InterpreterRuntime>,
		cwd: &Path,
		interpreter: Option<&str>,
		owner: Option<&str>,
		session_id: &str,
		reset: bool,
		per_call: bool,
		cell_deadline: Option<Instant>,
		extra_env: HashMap<String, String>,
		cancellation: CancellationToken,
	) -> Result<std::sync::Arc<Kernel>, EvalError> {
		let cached = languages::cached_runtime(EvalLanguage::Jl, cwd, interpreter);
		// Later cells reuse the retained kernel: readiness already happened, so no
		// boot happens and no budget is spent. Reset and per-call never reuse.
		if !per_call
			&& !reset
			&& let Some(runtime) = cached.as_ref()
			&& let Some(kernel) = self.registry.live(&julia_key(runtime, owner, session_id, cwd)).await
		{
			return Ok(kernel);
		}
		// Bounded cell => readiness draws on the caller's cell deadline (a retry
		// keeps the remaining time, never a fresh budget); unbounded cell => the
		// existing kernel startup safety deadline, never a hidden probe cap.
		let deadline = cell_deadline
			.unwrap_or_else(|| Instant::now() + languages::startup_timeout(EvalLanguage::Jl));
		if reset && !per_call {
			// Reset retires the old process before the replacement boots, exactly
			// like the probing path: two kernels never coexist for one slot.
			self.registry.evict(EvalLanguage::Jl, owner, session_id, cwd).await;
		}
		let mut candidates = candidates;
		if let Some(cached) = cached {
			// Prefer the interpreter already proven for this session over a PATH
			// re-scan that could pick a different binary mid-session.
			candidates.retain(|runtime| runtime.program != cached.program);
			candidates.insert(0, cached);
		}
		let mut failures = Vec::new();
		for runtime in candidates {
			if cancellation.is_cancelled() {
				return Err(EvalError::Aborted("Julia kernel readiness was cancelled".to_string()));
			}
			let now = Instant::now();
			if now >= deadline {
				failures.push(format!("{} (cell budget exhausted)", runtime.label));
				break;
			}
			if !env::is_executable(&runtime.program) {
				failures.push(format!("{} (not executable)", runtime.label));
				continue;
			}
			let spawn = KernelSpawn {
				language: EvalLanguage::Jl,
				runtime: runtime.clone(),
				cwd: cwd.to_path_buf(),
				extra_env: extra_env.clone(),
			};
			// The kernel owns its own process group: a failed, timed-out or
			// cancelled attempt is swept by `Kernel`'s shutdown/drop path.
			let attempt_started = Instant::now();
			let started = tokio::select! {
				() = cancellation.cancelled() => {
					return Err(EvalError::Aborted("Julia kernel readiness was cancelled".to_string()));
				},
				started = {
					let remaining = deadline.saturating_duration_since(now);
					tokio::time::timeout(
						remaining,
						Kernel::start_with_timeout(spawn, cancellation.clone(), remaining),
					)
				} => started,
			};
			let kernel = match started {
				Ok(Ok(kernel)) => kernel,
				Ok(Err(error)) => {
					if cancellation.is_cancelled() {
						return Err(EvalError::Aborted("Julia kernel readiness was cancelled".to_string()));
					}
					failures.push(format!(
						"{} ({}) in {}ms",
						runtime.label,
						first_line(&error.to_string()),
						attempt_started.elapsed().as_millis()
					));
					continue;
				},
				Err(_) => {
					failures.push(format!(
						"{} (readiness timed out after {}ms)",
						runtime.label,
						attempt_started.elapsed().as_millis()
					));
					break;
				},
			};
			languages::remember_runtime(EvalLanguage::Jl, cwd, interpreter, &runtime);
			if per_call {
				return Ok(std::sync::Arc::new(kernel));
			}
			let key = julia_key(&runtime, owner, session_id, cwd);
			return Ok(self.registry.adopt(key, std::sync::Arc::new(kernel)).await);
		}
		Err(EvalError::Unavailable(format!(
			"No working {} interpreter found. Tried: {}",
			EvalLanguage::Jl.display_name(),
			failures.join("; ")
		)))
	}

	pub async fn dispose_owner(&self, owner: Option<&str>) {
		self.registry.dispose_owner(owner).await;
	}

	pub async fn dispose_all(&self) {
		self.registry.dispose_all().await;
	}

	pub async fn shutdown(&self) {
		self.registry.dispose_all().await;
		self.bridge.shutdown().await;
	}

	/// Diagnostics: retained slot count for an owner.
	pub fn retained_count(&self, owner: Option<&str>) -> usize {
		self.registry.retained_count(owner)
	}
}

/// Retained-kernel key for one Julia interpreter.
fn julia_key(runtime: &InterpreterRuntime, owner: Option<&str>, session_id: &str, cwd: &Path) -> KernelKey {
	KernelKey {
		language: EvalLanguage::Jl,
		owner: owner.map(str::to_string),
		session_id: session_id.to_string(),
		cwd: cwd.to_path_buf(),
		interpreter: Some(runtime.program.display().to_string()),
	}
}

/// First non-empty line of a multi-line error, for one-line failure lists.
fn first_line(text: &str) -> String {
	text.lines().map(str::trim).find(|line| !line.is_empty()).unwrap_or_default().to_string()
}

/// Resolve a kernel cwd against the ambient environment (no implicit HOME write).
pub fn session_cwd(ctx_cwd: &Path, env: &HashMap<String, String>) -> PathBuf {
	if ctx_cwd.as_os_str().is_empty() {
		env.get("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from)
	} else {
		ctx_cwd.to_path_buf()
	}
}

/// Delay used by the auto-background wait race.
///
/// Mirrors `resolveAutoBackgroundWaitMs`: the cell is backgrounded one second
/// before its own timeout so a deadline expiry resolves inline, never just
/// after the watchdog killed the cell. `0` means background immediately.
pub fn background_wait(threshold_ms: u64, cell_timeout_ms: Option<u64>) -> u64 {
	const TIMEOUT_BUFFER_MS: u64 = 1_000;
	if threshold_ms == 0 {
		return 0;
	}
	match cell_timeout_ms {
		Some(0) | None => threshold_ms,
		Some(timeout) => threshold_ms.min(timeout.saturating_sub(TIMEOUT_BUFFER_MS)),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn defaults_match_upstream_allowance() {
		let settings = EvalSettings::default();
		assert!(settings.allowance().python);
		assert!(settings.allowance().js);
		assert!(!settings.allowance().ruby);
		assert!(!settings.allowance().julia);
		assert_eq!(settings.allowance().languages(), vec![EvalLanguage::Py, EvalLanguage::Js]);
	}

	#[test]
	fn flag_overrides_do_not_touch_process_env() {
		let settings = EvalSettings::default().with_flag_overrides(EvalFlagOverrides::from_values(
			Some(false),
			Some(true),
			Some(true),
			None,
		));
		assert!(!settings.allowance().python);
		assert!(settings.allowance().ruby);
		assert!(!settings.allowance().julia);
	}

	#[test]
	fn timeout_clamps_like_upstream() {
		let settings = EvalSettings::new(EvalAllowance::default(), Interpreters::default());
		assert_eq!(settings.resolve_timeout_secs(None), 30);
		assert_eq!(settings.resolve_timeout_secs(Some(0)), 1);
		assert_eq!(settings.resolve_timeout_secs(Some(9_999)), 3600);
		let capped = settings.with_timeouts(30, 5);
		assert_eq!(capped.resolve_timeout_secs(None), 5);
	}

	#[test]
	fn background_wait_keeps_one_second_of_cell_budget() {
		assert_eq!(background_wait(60_000, Some(10_000)), 9_000);
		assert_eq!(background_wait(60_000, Some(30_000)), 29_000);
		assert_eq!(background_wait(60_000, None), 60_000);
		assert_eq!(background_wait(60_000, Some(0)), 60_000);
		assert_eq!(background_wait(60_000, Some(500)), 0);
		assert_eq!(background_wait(0, None), 0);
	}
}
