//! Persistent subprocess kernel: NDJSON framing, cancellation, shutdown.
//!
//! One kernel owns one interpreter process. Runners dup their original stdout
//! for the protocol channel and repoint fd 1/2 at capture pipes, so user output
//! cannot spoof a frame; unparseable lines on the protocol channel are surfaced
//! as cell output instead of being dropped.

use std::{
	collections::HashMap,
	path::{Path, PathBuf},
	process::Stdio,
	sync::{
		Arc, Mutex,
		atomic::{AtomicBool, Ordering},
	},
	time::Duration,
};

use tokio::{
	io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
	process::{Child, ChildStdin, Command},
	sync::{Mutex as AsyncMutex, mpsc},
	task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use super::{
	env::InterpreterRuntime,
	languages,
	output::render_display_bundle,
	types::{
		EvalError, EvalLanguage, KernelError, KernelExecuteOptions, KernelExecuteResult,
		KernelFrame, KernelStatus,
	},
	util,
};

/// Hard cap for one protocol line before it is discarded.
const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
/// Retained runner stderr for diagnostics.
const STDERR_TAIL_BYTES: usize = 4 * 1024;
#[cfg(unix)]
const TERM_SIGNAL: i32 = libc::SIGTERM;
#[cfg(unix)]
const KILL_SIGNAL: i32 = libc::SIGKILL;
#[cfg(not(unix))]
const TERM_SIGNAL: i32 = 15;
#[cfg(not(unix))]
const KILL_SIGNAL: i32 = 9;

enum KernelEvent {
	Frame(KernelFrame),
	/// Non-protocol line written to the protocol fd by a child. Surfaced as
	/// cell output instead of being silently dropped.
	Raw(String),
	Closed,
}

/// Spawn configuration shared by all languages.
pub struct KernelSpawn {
	pub language: EvalLanguage,
	pub runtime: InterpreterRuntime,
	pub cwd: PathBuf,
	/// Bridge/session env merged over the filtered runtime env.
	pub extra_env: HashMap<String, String>,
}

pub struct Kernel {
	pub language: EvalLanguage,
	pub id: String,
	stdin: AsyncMutex<ChildStdin>,
	child: AsyncMutex<Child>,
	frames: AsyncMutex<mpsc::UnboundedReceiver<KernelEvent>>,
	alive: AtomicBool,
	shutdown_confirmed: AtomicBool,
	/// Set once the runner emits its protocol `started` frame. Startup failure
	/// text uses it to say whether the interpreter reached runner code at all.
	started_seen: AtomicBool,
	execution: AsyncMutex<()>,
	reader: Mutex<Option<JoinHandle<()>>>,
	stderr_drain: Mutex<Option<JoinHandle<()>>>,
	stderr_tail: Arc<Mutex<String>>,
	pid: Option<u32>,
	interrupt_grace: Duration,
}

impl Kernel {
	pub fn is_alive(&self) -> bool {
		self.alive.load(Ordering::SeqCst)
	}

	pub fn pid(&self) -> Option<u32> {
		self.pid
	}

	fn stderr_text(&self) -> String {
		self.stderr_tail.lock().unwrap_or_else(|poison| poison.into_inner()).clone()
	}

	/// Spawn the interpreter, run the language init cell, then its prelude.
	pub async fn start(spawn: KernelSpawn, cancellation: CancellationToken) -> Result<Self, EvalError> {
		let script = languages::stage_runner(spawn.language)?;
		let argv = languages::argv(spawn.language, &spawn.runtime.program, &script);
		let mut env = spawn.runtime.env.clone();
		env.extend(spawn.extra_env.clone());
		match spawn.language {
			EvalLanguage::Py => {
				env.insert("PYTHONUNBUFFERED".to_string(), "1".to_string());
				env.insert("PYTHONIOENCODING".to_string(), "utf-8".to_string());
			},
			EvalLanguage::Jl => {
				env.entry("GKSwstype".to_string()).or_insert_with(|| "100".to_string());
			},
			_ => {},
		}
		let kernel = Self::spawn_process(spawn.language, &argv, &spawn.cwd, env)?;

		let timeout = languages::startup_timeout(spawn.language);
		let init = languages::init_script(spawn.language, &spawn.cwd);
		kernel.run_startup("init", &init, timeout, &cancellation).await?;
		let assets = languages::assets(spawn.language);
		if !assets.prelude.is_empty() {
			kernel
				.run_startup(
					&format!("{} prelude", spawn.language.display_name()),
					assets.prelude,
					timeout,
					&cancellation,
				)
				.await?;
		}
		Ok(kernel)
	}

	/// Wire a child process into a protocol kernel without running init/prelude.
	fn spawn_process(
		language: EvalLanguage,
		argv: &[String],
		cwd: &Path,
		env: HashMap<String, String>,
	) -> Result<Self, EvalError> {
		let Some(program) = argv.first() else {
			return Err(EvalError::Kernel("empty kernel argv".to_string()));
		};
		let mut command = Command::new(program);
		command
			.args(&argv[1..])
			.current_dir(cwd)
			.env_clear()
			.envs(&env)
			.stdin(Stdio::piped())
			.stdout(Stdio::piped())
			.stderr(Stdio::piped())
			.kill_on_drop(true);
		#[cfg(unix)]
		{
			// SAFETY: `setsid` in the child before exec; it cannot affect the host.
			unsafe {
				command.pre_exec(|| {
					libc::setsid();
					Ok(())
				});
			}
		}
		let mut child = command.spawn()?;
		let pid = child.id();
		let stdin = child
			.stdin
			.take()
			.ok_or_else(|| EvalError::Kernel("kernel stdin was not captured".to_string()))?;
		let stdout = child
			.stdout
			.take()
			.ok_or_else(|| EvalError::Kernel("kernel stdout was not captured".to_string()))?;
		let stderr = child
			.stderr
			.take()
			.ok_or_else(|| EvalError::Kernel("kernel stderr was not captured".to_string()))?;

		let (frames_tx, frames_rx) = mpsc::unbounded_channel();
		let reader = tokio::spawn(read_frames(stdout, frames_tx));
		let stderr_tail = Arc::new(Mutex::new(String::new()));
		let stderr_drain = tokio::spawn(drain_stderr(stderr, Arc::clone(&stderr_tail)));

		Ok(Self {
			language,
			id: util::random_token(),
			stdin: AsyncMutex::new(stdin),
			child: AsyncMutex::new(child),
			frames: AsyncMutex::new(frames_rx),
			alive: AtomicBool::new(true),
			shutdown_confirmed: AtomicBool::new(false),
			started_seen: AtomicBool::new(false),
			execution: AsyncMutex::new(()),
			reader: Mutex::new(Some(reader)),
			stderr_drain: Mutex::new(Some(stderr_drain)),
			stderr_tail,
			pid,
			interrupt_grace: languages::INTERRUPT_ESCALATION,
		})
	}

	async fn run_startup(
		&self,
		label: &str,
		code: &str,
		timeout: Duration,
		cancellation: &CancellationToken,
	) -> Result<(), EvalError> {
		let token = CancellationToken::new();
		let (combined, task) = combine(cancellation.clone(), token.clone());
		let _combined_task = AbortOnDrop(task);
		// Provenance for the failure below: the flag is stored before `cancel`,
		// so a failure observed after the deadline is attributable to the timer
		// instead of to the caller's cancellation.
		let deadline_fired = Arc::new(AtomicBool::new(false));
		let _timer = AbortOnDrop({
			let deadline_fired = Arc::clone(&deadline_fired);
			tokio::spawn(async move {
				tokio::time::sleep(timeout).await;
				deadline_fired.store(true, Ordering::SeqCst);
				token.cancel();
			})
		});
		let env = HashMap::new();
		let result = self
			.execute(
				code,
				KernelExecuteOptions {
					run_id: format!("{}-{label}", self.id),
					cwd: Path::new("."),
					env: &env,
					silent: true,
					store_history: false,
					cancellation: combined,
					on_chunk: None,
					on_display: None,
				},
			)
			.await;
		// Read once, right after the cell settles: a later timer tick cannot
		// relabel an already observed failure. Caller cancellation wins so an
		// external abort never reads as a deadline.
		let deadline = (deadline_fired.load(Ordering::SeqCst) && !cancellation.is_cancelled())
			.then_some(timeout);
		let runner_started = self.started_seen.load(Ordering::SeqCst);
		match result {
			Ok(result) if result.status == KernelStatus::Ok && !result.cancelled => Ok(()),
			Ok(result) => {
				let reason = result
					.error
					.map_or_else(|| "kernel startup was cancelled".to_string(), |error| error.value);
				let stderr = self.stderr_text();
				self.shutdown().await;
				Err(EvalError::Kernel(startup_failure(label, &reason, &stderr, deadline, runner_started)))
			},
			Err(error) => {
				self.shutdown().await;
				Err(startup_error(label, error, &self.stderr_text(), deadline, runner_started))
			},
		}
	}

	/// Execute one cell. Concurrent callers serialize; one cell at a time.
	pub async fn execute(
		&self,
		code: &str,
		mut options: KernelExecuteOptions<'_>,
	) -> Result<KernelExecuteResult, EvalError> {
		let _guard = self.execution.lock().await;
		if !self.is_alive() {
			return Err(EvalError::Kernel(format!(
				"{} kernel is not running{}",
				self.language.display_name(),
				self.stderr_suffix()
			)));
		}
		if options.cancellation.is_cancelled() {
			return Ok(cancelled_result(true));
		}

		let payload = self.build_payload(code, &options);
		if payload.len() > MAX_FRAME_BYTES {
			return Err(EvalError::InvalidArguments(format!(
				"cell payload exceeds {MAX_FRAME_BYTES} bytes"
			)));
		}
		{
			let mut stdin = self.stdin.lock().await;
			stdin.write_all(payload.as_bytes()).await?;
			stdin.write_all(b"\n").await?;
			stdin.flush().await?;
		}

		let done = Arc::new(AtomicBool::new(false));
		let mut interrupt_sent = false;
		let mut status = KernelStatus::Ok;
		let mut error = None;
		let mut execution_count = None;
		let mut cancelled = false;

		let mut frames = self.frames.lock().await;
		loop {
			if options.cancellation.is_cancelled() && !interrupt_sent {
				interrupt_sent = true;
				cancelled = true;
				self.interrupt();
				spawn_escalation(self.pid, Arc::clone(&done), self.interrupt_escalation());
			}
			let event = tokio::select! {
				event = frames.recv() => event,
				() = options.cancellation.cancelled(), if !interrupt_sent => {
					interrupt_sent = true;
					cancelled = true;
					self.interrupt();
					spawn_escalation(self.pid, Arc::clone(&done), self.interrupt_escalation());
					continue;
				},
			};
			match event {
				None | Some(KernelEvent::Closed) => {
					done.store(true, Ordering::SeqCst);
					self.alive.store(false, Ordering::SeqCst);
					return Ok(KernelExecuteResult {
						status: KernelStatus::Error,
						execution_count,
						error: Some(KernelError {
							name: "KernelExited".to_string(),
							value: format!(
								"{} kernel exited unexpectedly{}",
								self.language.display_name(),
								self.stderr_suffix()
							),
							traceback: Vec::new(),
						}),
						cancelled: true,
						timed_out: false,
						stdin_requested: false,
						kernel_killed: true,
					});
				},
				Some(KernelEvent::Raw(text)) => {
					if let Some(on_chunk) = options.on_chunk.as_mut()
						&& !text.is_empty()
					{
						on_chunk(&text);
					}
				},
				Some(KernelEvent::Frame(frame)) => {
					if frame.id.as_deref() != Some(options.run_id.as_str()) {
						continue;
					}
					match frame.kind.as_str() {
						"started" => {
							self.started_seen.store(true, Ordering::SeqCst);
						},
						"stdout" | "stderr" => {
							if let (Some(chunk), Some(on_chunk)) =
								(frame.data.as_deref(), options.on_chunk.as_mut())
								&& !chunk.is_empty()
							{
								on_chunk(chunk);
							}
						},
						"display" | "result" => {
							let Some(bundle) = frame.bundle.as_ref() else { continue };
							let rendered = render_display_bundle(bundle);
							if !rendered.text.is_empty()
								&& let Some(on_chunk) = options.on_chunk.as_mut()
							{
								on_chunk(&rendered.text);
							}
							if let Some(on_display) = options.on_display.as_mut() {
								for output in rendered.outputs {
									on_display(output);
								}
							}
						},
						"error" => {
							status = KernelStatus::Error;
							let traceback = frame.traceback.unwrap_or_default();
							let name = frame.ename.unwrap_or_else(|| "Error".to_string());
							let value = frame.evalue.unwrap_or_default();
							let message = if traceback.is_empty() {
								format!("{name}: {value}\n")
							} else {
								format!("{}\n", traceback.join("\n"))
							};
							if let Some(on_chunk) = options.on_chunk.as_mut() {
								on_chunk(&message);
							}
							error = Some(KernelError { name, value, traceback });
						},
						"done" => {
							done.store(true, Ordering::SeqCst);
							if frame.status.as_deref() == Some("error") && status == KernelStatus::Ok {
								status = KernelStatus::Error;
							}
							if frame.cancelled.unwrap_or(false) {
								cancelled = true;
							}
							execution_count = frame.execution_count;
							return Ok(KernelExecuteResult {
								status,
								execution_count,
								error,
								cancelled,
								timed_out: false,
								stdin_requested: false,
								kernel_killed: false,
							});
						},
						_ => {},
					}
				},
			}
		}
	}

	fn interrupt_escalation(&self) -> Duration {
		self.interrupt_grace
	}

	fn stderr_suffix(&self) -> String {
		let stderr = self.stderr_text();
		let text = stderr.trim();
		if text.is_empty() {
			String::new()
		} else {
			format!(": {text}")
		}
	}

	fn build_payload(&self, code: &str, options: &KernelExecuteOptions<'_>) -> String {
		let cwd = options.cwd.display().to_string();
		match self.language {
			EvalLanguage::Jl => {
				let env_pairs = options
					.env
					.iter()
					.map(|(key, value)| {
						format!(
							"{}:{}",
							util::base64_encode(key.as_bytes()),
							util::base64_encode(value.as_bytes())
						)
					})
					.collect::<Vec<_>>()
					.join(" ");
				format!(
					"run\t{}\t{}\t{}\t{}\t{}\t{}",
					options.run_id,
					util::base64_encode(cwd.as_bytes()),
					if options.silent { "1" } else { "0" },
					if options.store_history && !options.silent { "1" } else { "0" },
					env_pairs,
					util::base64_encode(code.as_bytes())
				)
			},
			_ => serde_json::json!({
				"id": options.run_id,
				"code": code,
				"cwd": cwd,
				"env": options.env,
				"silent": options.silent,
				"storeHistory": options.store_history && !options.silent,
			})
			.to_string(),
		}
	}

	/// Deliver SIGINT to the runner so user code sees `KeyboardInterrupt`.
	pub fn interrupt(&self) {
		#[cfg(unix)]
		if let Some(pid) = self.pid.filter(|pid| *pid > 1) {
			// SAFETY: signal a child we spawned.
			unsafe {
				libc::kill(pid as i32, libc::SIGINT);
			}
		}
	}

	/// Graceful exit with SIGTERM/SIGKILL escalation and a group sweep.
	pub async fn shutdown(&self) {
		if self.shutdown_confirmed.swap(true, Ordering::SeqCst) {
			return;
		}
		self.alive.store(false, Ordering::SeqCst);
		let grace = languages::SHUTDOWN_GRACE;
		// Graceful stage, bounded as one unit: the stdin lock can be held by a
		// write stuck on a kernel that stopped draining its pipe. Expiring it
		// only skips the courtesy exit payload — the escalation below always
		// runs, so a stalled kernel is still killed and reaped.
		let graceful = async {
			let mut stdin = self.stdin.lock().await;
			let _ = stdin.write_all(languages::exit_payload(self.language).as_bytes()).await;
			let _ = stdin.write_all(b"\n").await;
			let _ = stdin.flush().await;
			let _ = stdin.shutdown().await;
		};
		let _ = tokio::time::timeout(grace, graceful).await;
		let mut child = self.child.lock().await;
		if tokio::time::timeout(grace, child.wait()).await.is_ok() {
			return;
		}
		signal_group(self.pid, TERM_SIGNAL);
		if tokio::time::timeout(grace, child.wait()).await.is_ok() {
			signal_group(self.pid, KILL_SIGNAL);
			return;
		}
		let _ = child.start_kill();
		signal_group(self.pid, KILL_SIGNAL);
		let _ = tokio::time::timeout(grace, child.wait()).await;
	}

	/// Total owned time for diagnostics/tests.
	pub fn stderr_tail(&self) -> String {
		self.stderr_text()
	}
}

impl Drop for Kernel {
	fn drop(&mut self) {
		if let Some(reader) = self.reader.lock().unwrap_or_else(|poison| poison.into_inner()).take() {
			reader.abort();
		}
		if let Some(drain) = self.stderr_drain.lock().unwrap_or_else(|poison| poison.into_inner()).take() {
			drain.abort();
		}
		// Best-effort group sweep: Drop cannot await a graceful exit.
		signal_group(self.pid, KILL_SIGNAL);
	}
}

fn cancelled_result(timed_out: bool) -> KernelExecuteResult {
	KernelExecuteResult {
		status: KernelStatus::Error,
		execution_count: None,
		error: None,
		cancelled: true,
		timed_out,
		stdin_requested: false,
		kernel_killed: false,
	}
}

/// Startup failure text: keeps the underlying reason, names an expired deadline
/// explicitly with whether the runner had started, and appends the bounded
/// runner stderr tail exactly once.
fn startup_failure(
	label: &str,
	reason: &str,
	stderr: &str,
	deadline: Option<Duration>,
	runner_started: bool,
) -> String {
	let mut message = match deadline {
		Some(timeout) => format!(
			"{label} timed out after {timeout:?} ({}): {reason}",
			if runner_started { "runner started" } else { "no runner frame" }
		),
		None => format!("{label} failed: {reason}"),
	};
	let stderr = stderr.trim();
	if !stderr.is_empty() && !message.contains(stderr) {
		message.push('\n');
		message.push_str(stderr);
	}
	message
}

/// Same context for errors that never reached a result frame, preserving the
/// error kind. Only runner stderr is added; argv and environment stay out.
fn startup_error(
	label: &str,
	error: EvalError,
	stderr: &str,
	deadline: Option<Duration>,
	runner_started: bool,
) -> EvalError {
	match error {
		EvalError::Kernel(reason) => EvalError::Kernel(startup_failure(
			label,
			&reason,
			stderr,
			deadline,
			runner_started,
		)),
		EvalError::Io(error) => {
			let kind = error.kind();
			let message = startup_failure(label, &error.to_string(), stderr, deadline, runner_started);
			EvalError::Io(std::io::Error::new(kind, message))
		},
		other => other,
	}
}

/// Aborts its task when dropped, including when the surrounding startup future
/// is dropped mid-flight: a bounded helper never outlives its owner.
struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
	fn drop(&mut self) {
		self.0.abort();
	}
}

fn combine(external: CancellationToken, idle: CancellationToken) -> (CancellationToken, JoinHandle<()>) {
	let combined = CancellationToken::new();
	let target = combined.clone();
	let task = tokio::spawn(async move {
		tokio::select! {
			() = external.cancelled() => {},
			() = idle.cancelled() => {},
		}
		target.cancel();
	});
	(combined, task)
}

fn spawn_escalation(pid: Option<u32>, done: Arc<AtomicBool>, after: Duration) {
	tokio::spawn(async move {
		tokio::time::sleep(after).await;
		if done.load(Ordering::SeqCst) {
			return;
		}
		signal_group(pid, KILL_SIGNAL);
	});
}

fn signal_group(pid: Option<u32>, signal: i32) {
	#[cfg(unix)]
	if let Some(pid) = pid.filter(|pid| *pid > 1) {
		// SAFETY: negative pid targets the child session we created; 0/1 are
		// rejected because they would signal our own group or everything.
		unsafe {
			libc::kill(-(pid as i32), signal);
		}
	}
}

async fn read_frames(stdout: tokio::process::ChildStdout, sender: mpsc::UnboundedSender<KernelEvent>) {
	let mut reader = BufReader::new(stdout);
	let mut line = Vec::new();
	loop {
		line.clear();
		match reader.read_until(b'\n', &mut line).await {
			Ok(0) | Err(_) => break,
			Ok(_) => {},
		}
		if line.len() > MAX_FRAME_BYTES {
			continue;
		}
		let text = String::from_utf8_lossy(&line);
		if text.trim().is_empty() {
			continue;
		}
		match serde_json::from_str::<KernelFrame>(text.trim()) {
			Ok(frame) => {
				if sender.send(KernelEvent::Frame(frame)).is_err() {
					break;
				}
			},
			Err(_) => {
				if sender.send(KernelEvent::Raw(text.into_owned())).is_err() {
					break;
				}
			},
		}
	}
	let _ = sender.send(KernelEvent::Closed);
}

async fn drain_stderr(stderr: tokio::process::ChildStderr, tail: Arc<Mutex<String>>) {
	use tokio::io::AsyncReadExt as _;
	let mut reader = BufReader::new(stderr);
	let mut buffer = [0u8; 1024];
	loop {
		match reader.read(&mut buffer).await {
			Ok(0) | Err(_) => break,
			Ok(read) => {
				let text = String::from_utf8_lossy(&buffer[..read]);
				let mut tail = tail.lock().unwrap_or_else(|poison| poison.into_inner());
				tail.push_str(&text);
				if tail.len() > STDERR_TAIL_BYTES {
					let keep = tail.len() - STDERR_TAIL_BYTES;
					let cut = tail
						.char_indices()
						.find(|(index, _)| *index >= keep)
						.map_or(tail.len(), |(index, _)| index);
					tail.drain(..cut);
				}
			},
		}
	}
}

/// Staged runner path for diagnostics/tests.
pub fn staged_runner_path(language: EvalLanguage) -> std::io::Result<PathBuf> {
	languages::stage_runner(language)
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::time::Instant;

	const FAKE_WIRE: &str = r#"trap '' INT
while IFS= read -r line; do
  case "$line" in
    *'"id":"'*)
      id=$(printf '%s' "$line" | sed -E 's/.*"id":"([^"]+)".*/\1/')
      printf '{"type":"stdout","id":"%s","data":"ready"}\n' "$id"
      ;;
  esac
done"#;

	const FAKE_WIRE_RAW: &str = r#"trap '' INT
while IFS= read -r line; do
  case "$line" in
    *'"id":"'*)
      id=$(printf '%s' "$line" | sed -E 's/.*"id":"([^"]+)".*/\1/')
      printf 'garbage-line\n'
      printf '{"type":"done","id":"%s","status":"ok"}\n' "$id"
      ;;
  esac
done"#;

	#[cfg(unix)]
	#[tokio::test]
	async fn non_protocol_lines_surface_as_cell_output() {
		let env = std::env::vars().collect::<HashMap<_, _>>();
		let mut kernel = Kernel::spawn_process(
			EvalLanguage::Py,
			&["sh".to_string(), "-c".to_string(), FAKE_WIRE_RAW.to_string()],
			Path::new("."),
			env,
		)
		.expect("fake kernel spawn");
		let mut sink = String::new();
		let env = HashMap::new();
		let result = {
			let on_chunk = |chunk: &str| sink.push_str(chunk);
			kernel
				.execute(
					"ignored",
					KernelExecuteOptions {
						run_id: "run-raw".to_string(),
						cwd: Path::new("."),
						env: &env,
						silent: false,
						store_history: false,
						cancellation: CancellationToken::new(),
						on_chunk: Some(Box::new(on_chunk)),
						on_display: None,
					},
				)
				.await
				.expect("execute")
		};
		assert_eq!(result.status, KernelStatus::Ok, "done frame was recognized: {result:?}");
		assert!(sink.contains("garbage-line"), "raw line was dropped: {sink:?}");
		kernel.shutdown().await;
	}

	#[cfg(unix)]
	#[tokio::test]
	async fn fake_wire_cancel_escalates_and_reports_kernel_killed() {
		let env = std::env::vars().collect::<HashMap<_, _>>();
		let mut kernel = Kernel::spawn_process(
			EvalLanguage::Py,
			&["sh".to_string(), "-c".to_string(), FAKE_WIRE.to_string()],
			Path::new("."),
			env,
		)
		.expect("fake kernel spawn");
		kernel.interrupt_grace = Duration::from_millis(200);

		let cancellation = CancellationToken::new();
		let trigger = cancellation.clone();
		tokio::spawn(async move {
			tokio::time::sleep(Duration::from_millis(150)).await;
			trigger.cancel();
		});
		let started = Instant::now();
		let mut sink = String::new();
		let env = HashMap::new();
		let result = {
			let on_chunk = |chunk: &str| sink.push_str(chunk);
			kernel
				.execute(
					"ignored",
					KernelExecuteOptions {
						run_id: "run-1".to_string(),
						cwd: Path::new("."),
						env: &env,
						silent: false,
						store_history: false,
						cancellation: cancellation.clone(),
						on_chunk: Some(Box::new(on_chunk)),
						on_display: None,
					},
				)
				.await
				.expect("execute")
		};
		assert!(sink.contains("ready"), "wire output was captured: {sink:?}");
		assert!(result.cancelled || result.kernel_killed);
		assert!(started.elapsed() < Duration::from_secs(2), "escalation was bounded");
		assert!(!kernel.is_alive() || result.kernel_killed);
	}

	/// A kernel that stopped draining its stdin must not stall exit teardown:
	/// the graceful write stage is bounded, and the SIGTERM/SIGKILL escalation
	/// still runs, so the process is killed and reaped instead of leaked.
	#[cfg(unix)]
	#[tokio::test]
	async fn shutdown_bounds_a_stalled_stdin_stage_and_still_kills() {
		let env = std::env::vars().collect::<HashMap<_, _>>();
		let kernel = Arc::new(
			Kernel::spawn_process(
				EvalLanguage::Py,
				&["sh".to_string(), "-c".to_string(), "exec sleep 30".to_string()],
				Path::new("."),
				env,
			)
			.expect("stalled kernel spawn"),
		);
		let pid = kernel.pid().expect("kernel pid");
		// Wedge the write path the way a non-draining pipe does: the stdin
		// writer stays locked for longer than the graceful bound. The holder
		// reports readiness itself, so nothing depends on task scheduling.
		let (locked, lock_held) = tokio::sync::oneshot::channel();
		let hold = {
			let kernel = Arc::clone(&kernel);
			tokio::spawn(async move {
				let _guard = kernel.stdin.lock().await;
				let _ = locked.send(());
				tokio::time::sleep(Duration::from_secs(30)).await;
			})
		};
		lock_held.await.expect("stdin lock was acquired");
		let started = Instant::now();
		tokio::time::timeout(Duration::from_secs(5), kernel.shutdown())
			.await
			.expect("shutdown waited on the stalled write");
		assert!(started.elapsed() < Duration::from_secs(5), "graceful stage was bounded");
		assert!(!kernel.is_alive(), "kernel was retired");
		assert!(!pid_alive(pid), "stalled kernel survived teardown");
		hold.abort();
	}

	/// Blocks until killed and ignores SIGINT, so only the startup deadline's
	/// escalation can end it.
	const FAKE_SILENT: &str = r#"trap '' INT
while :; do sleep 0.05; done"#;

	/// Consumes one payload, then exits cleanly: EOF with no deadline involved.
	const FAKE_EXIT_AFTER_REQUEST: &str = r#"read line
exit 0"#;

	/// Emits the protocol `started` frame, then hangs until killed.
	const FAKE_STARTED_THEN_SILENT: &str = r#"trap '' INT
while IFS= read -r line; do
  case "$line" in
    *'"id":"'*)
      id=$(printf '%s' "$line" | sed -E 's/.*"id":"([^"]+)".*/\1/')
      printf '{"type":"started","id":"%s"}\n' "$id"
      ;;
  esac
done"#;

	#[cfg(unix)]
	fn fake_kernel(script: &str) -> Kernel {
		let env = std::env::vars().collect::<HashMap<_, _>>();
		Kernel::spawn_process(
			EvalLanguage::Py,
			&["sh".to_string(), "-c".to_string(), script.to_string()],
			Path::new("."),
			env,
		)
		.expect("fake kernel spawn")
	}

	#[cfg(unix)]
	#[tokio::test]
	async fn startup_deadline_names_timeout_and_keeps_reason() {
		let mut kernel = fake_kernel(FAKE_SILENT);
		kernel.interrupt_grace = Duration::from_millis(200);
		let started = Instant::now();
		let error = kernel
			.run_startup("init", "ignored", Duration::from_millis(300), &CancellationToken::new())
			.await
			.expect_err("deadline expiry fails startup");
		let message = error.to_string();
		assert!(message.contains("init timed out after 300ms"), "deadline provenance missing: {message}");
		assert!(message.contains("no runner frame"), "started-frame provenance missing: {message}");
		assert!(message.contains("exited unexpectedly"), "underlying EOF reason was dropped: {message}");
		assert!(started.elapsed() < Duration::from_secs(3), "deadline path was unbounded");
		assert!(!kernel.is_alive(), "timed-out kernel survived");
	}

	#[cfg(unix)]
	#[tokio::test]
	async fn external_cancel_is_never_reported_as_deadline() {
		let mut kernel = fake_kernel(FAKE_SILENT);
		kernel.interrupt_grace = Duration::from_millis(200);
		let cancellation = CancellationToken::new();
		let trigger = cancellation.clone();
		tokio::spawn(async move {
			tokio::time::sleep(Duration::from_millis(100)).await;
			trigger.cancel();
		});
		let started = Instant::now();
		let error = kernel
			.run_startup("init", "ignored", Duration::from_secs(60), &cancellation)
			.await
			.expect_err("caller cancellation fails startup");
		let message = error.to_string();
		assert!(!message.contains("timed out"), "external cancel was blamed on the deadline: {message}");
		assert!(message.contains("init failed"), "startup label missing: {message}");
		assert!(started.elapsed() < Duration::from_secs(5), "cancel path was slow");
	}

	#[cfg(unix)]
	#[tokio::test]
	async fn early_exit_without_deadline_keeps_unexpected_eof_wording() {
		let kernel = fake_kernel(FAKE_EXIT_AFTER_REQUEST);
		let error = kernel
			.run_startup("init", "ignored", Duration::from_secs(60), &CancellationToken::new())
			.await
			.expect_err("early exit fails startup");
		let message = error.to_string();
		assert!(!message.contains("timed out"), "early exit was blamed on the deadline: {message}");
		assert!(message.contains("init failed"), "startup label missing: {message}");
		assert!(message.contains("exited unexpectedly"), "EOF wording changed: {message}");
	}

	#[cfg(unix)]
	#[tokio::test]
	async fn deadline_reports_that_the_runner_had_started() {
		let mut kernel = fake_kernel(FAKE_STARTED_THEN_SILENT);
		kernel.interrupt_grace = Duration::from_millis(200);
		let error = kernel
			.run_startup("init", "ignored", Duration::from_secs(1), &CancellationToken::new())
			.await
			.expect_err("deadline expiry fails startup");
		let message = error.to_string();
		assert!(message.contains("timed out after 1s"), "{message}");
		assert!(message.contains("runner started"), "started frame was not observed: {message}");
		assert!(!message.contains("no runner frame"), "{message}");
	}

	#[test]
	fn startup_failure_appends_stderr_once_and_keeps_io_kind() {
		let message =
			startup_failure("init", "Python kernel exited unexpectedly: boom", "boom\n", None, false);
		assert_eq!(message.matches("boom").count(), 1, "stderr was duplicated: {message}");
		let message =
			startup_failure("init", "write failed", "trace\n", Some(Duration::from_millis(300)), false);
		assert!(
			message.starts_with("init timed out after 300ms (no runner frame): write failed"),
			"{message}"
		);
		assert!(message.ends_with("\ntrace"), "{message}");
		let error = startup_error(
			"init",
			EvalError::Io(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "boom")),
			"trace\n",
			None,
			false,
		);
		assert!(matches!(&error, EvalError::Io(io) if io.kind() == std::io::ErrorKind::BrokenPipe));
		let text = error.to_string();
		assert!(text.contains("init failed: boom") && text.contains("trace"), "{text}");
	}

	/// Signal-0 existence probe: after the reap the pid is gone.
	#[cfg(unix)]
	fn pid_alive(pid: u32) -> bool {
		// SAFETY: signal 0 delivers nothing; it only asks whether the pid exists.
		unsafe { libc::kill(pid as i32, 0) == 0 }
	}
}
