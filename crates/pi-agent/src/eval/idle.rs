//! Eval cell watchdog with reference-counted pause/resume.
//!
//! A cell's `timeout` bounds only time while the kernel has control. Host-side
//! `agent()` and `completion()` bridges pause the watchdog; returning control
//! starts a fresh window.

use std::{
	sync::{
		Arc, Mutex,
		atomic::{AtomicBool, Ordering},
	},
	time::{Duration, Instant},
};

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

struct State {
	depth: usize,
	deadline: Instant,
	timer: Option<JoinHandle<()>>,
	settled: bool,
}

struct Inner {
	idle: Duration,
	token: CancellationToken,
	timed_out: AtomicBool,
	state: Mutex<State>,
}

impl Inner {
	fn arm(self: &Arc<Self>) {
		let deadline = {
			let mut state = self.state.lock().unwrap_or_else(|poison| poison.into_inner());
			if state.settled || state.depth > 0 {
				return;
			}
			if let Some(timer) = state.timer.take() {
				timer.abort();
			}
			state.deadline
		};
		let inner = Arc::clone(self);
		let timer = tokio::spawn(async move {
			loop {
				let now = Instant::now();
				if now >= deadline {
					break;
				}
				tokio::time::sleep(deadline.saturating_duration_since(now)).await;
				// Re-read the deadline: a resume may have moved it forward.
				let current = {
					let state = inner.state.lock().unwrap_or_else(|poison| poison.into_inner());
					state.deadline
				};
				if current <= now {
					break;
				}
			}
			let fired = {
				let mut state = inner.state.lock().unwrap_or_else(|poison| poison.into_inner());
				if state.settled || state.depth > 0 {
					false
				} else {
					state.settled = true;
					state.timer = None;
					true
				}
			};
			if fired {
				inner.timed_out.store(true, Ordering::SeqCst);
				inner.token.cancel();
			}
		});
		let mut state = self.state.lock().unwrap_or_else(|poison| poison.into_inner());
		if !state.settled {
			state.timer = Some(timer);
		}
	}
}

/// Watchdog handle.
#[derive(Clone)]
pub struct IdleTimeout {
	inner: Arc<Inner>,
}

impl IdleTimeout {
	pub fn new(idle: Duration) -> Self {
		let idle = idle.max(Duration::from_millis(1));
		let inner = Arc::new(Inner {
			idle,
			token: CancellationToken::new(),
			timed_out: AtomicBool::new(false),
			state: Mutex::new(State {
				depth: 0,
				deadline: Instant::now() + idle,
				timer: None,
				settled: false,
			}),
		});
		inner.arm();
		Self { inner }
	}

	pub fn idle(&self) -> Duration {
		self.inner.idle
	}

	/// Cancelled with the `TimeoutError` analogue when the active window expires.
	pub fn token(&self) -> CancellationToken {
		self.inner.token.clone()
	}

	pub fn timed_out(&self) -> bool {
		self.inner.timed_out.load(Ordering::SeqCst)
	}

	/// Suspend accounting while host-side work runs.
	pub fn pause(&self) {
		let mut state = self.inner.state.lock().unwrap_or_else(|poison| poison.into_inner());
		if state.settled {
			return;
		}
		state.depth += 1;
		if state.depth == 1
			&& let Some(timer) = state.timer.take()
		{
			timer.abort();
		}
	}

	/// Resume accounting with a fresh window.
	pub fn resume(&self) {
		let arm = {
			let mut state = self.inner.state.lock().unwrap_or_else(|poison| poison.into_inner());
			if state.settled || state.depth == 0 {
				return;
			}
			state.depth -= 1;
			if state.depth > 0 {
				return;
			}
			state.deadline = Instant::now() + self.inner.idle;
			true
		};
		if arm {
			self.inner.arm();
		}
	}

	pub fn dispose(&self) {
		let mut state = self.inner.state.lock().unwrap_or_else(|poison| poison.into_inner());
		if state.settled {
			return;
		}
		state.settled = true;
		if let Some(timer) = state.timer.take() {
			timer.abort();
		}
	}
}

impl Drop for IdleTimeout {
	fn drop(&mut self) {
		// Each clone drops; the anyway-idempotent dispose runs once the last
		// handle disappears only when state is still unsettled.
		if Arc::strong_count(&self.inner) == 1 {
			self.dispose();
		}
	}
}

struct ControlState {
	shield_depth: usize,
	external_aborted: bool,
}

struct ControlInner {
	idle: Option<IdleTimeout>,
	kernel: CancellationToken,
	state: Mutex<ControlState>,
	watcher: Mutex<Option<JoinHandle<()>>>,
}

/// Kernel cancellation plus bridge-side watchdog control for one cell.
///
/// The kernel token cancels on watchdog expiry or on turn cancellation, except
/// while a bridge call marked `defer_external_abort` is in flight. Deferral
/// protects the runtime only: the host still receives the caller's real
/// cancellation token through [`crate::state::AgentContext`].
#[derive(Clone)]
pub struct CellControl {
	inner: Arc<ControlInner>,
}

impl CellControl {
	/// Derive the kernel token from `external` and the optional watchdog.
	pub fn new(external: CancellationToken, idle: Option<IdleTimeout>) -> Self {
		let inner = Arc::new(ControlInner {
			idle,
			kernel: CancellationToken::new(),
			state: Mutex::new(ControlState { shield_depth: 0, external_aborted: false }),
			watcher: Mutex::new(None),
		});
		let kernel = inner.kernel.clone();
		if external.is_cancelled() {
			let mut state = inner
				.state
				.lock()
				.unwrap_or_else(|poison| poison.into_inner());
			state.external_aborted = true;
			drop(state);
			kernel.cancel();
			return Self { inner };
		}
		let idle_token = inner.idle.as_ref().map(IdleTimeout::token);
		let weak = Arc::downgrade(&inner);
		let watcher = tokio::spawn(async move {
			tokio::select! {
				() = external.cancelled() => {
					if let Some(inner) = weak.upgrade() {
						let mut state = inner
							.state
							.lock()
							.unwrap_or_else(|poison| poison.into_inner());
						state.external_aborted = true;
						if state.shield_depth == 0 {
							inner.kernel.cancel();
						}
					}
				},
				() = async move {
					match idle_token {
						Some(token) => token.cancelled().await,
						None => std::future::pending().await,
					}
				} => {
					if let Some(inner) = weak.upgrade() {
						inner.kernel.cancel();
					}
				},
			}
		});
		*inner.watcher.lock().unwrap_or_else(|poison| poison.into_inner()) = Some(watcher);
		Self { inner }
	}

	/// Token passed to the kernel; a deferred external abort is withheld here.
	pub fn kernel_token(&self) -> CancellationToken {
		self.inner.kernel.clone()
	}

	/// Suspend the cell watchdog while host-side work owns control.
	pub fn pause(&self) {
		if let Some(idle) = &self.inner.idle {
			idle.pause();
		}
	}

	/// Resume the watchdog with a fresh window.
	pub fn resume(&self) {
		if let Some(idle) = &self.inner.idle {
			idle.resume();
		}
	}

	/// Pause and hold an external abort away from the kernel until the matching
	/// [`Self::resume_deferring_abort`].
	///
	/// The shield is raised before the watchdog is paused: a turn cancel that
	/// lands between the two steps is then observed as deferred instead of
	/// cancelling the kernel while host work is starting.
	pub fn pause_deferring_abort(&self) {
		let mut state = self
			.inner
			.state
			.lock()
			.unwrap_or_else(|poison| poison.into_inner());
		state.shield_depth += 1;
		drop(state);
		self.pause();
	}

	pub fn resume_deferring_abort(&self) {
		let cancel = {
			let mut state = self
				.inner
				.state
				.lock()
				.unwrap_or_else(|poison| poison.into_inner());
			if state.shield_depth == 0 {
				false
			} else {
				state.shield_depth -= 1;
				state.shield_depth == 0 && state.external_aborted
			}
		};
		if cancel {
			self.inner.kernel.cancel();
		}
		self.resume();
	}

	pub fn timed_out(&self) -> bool {
		self.inner.idle.as_ref().is_some_and(IdleTimeout::timed_out)
	}

	pub fn dispose(&self) {
		if let Some(watcher) = self
			.inner
			.watcher
			.lock()
			.unwrap_or_else(|poison| poison.into_inner())
			.take()
		{
			watcher.abort();
		}
		if let Some(idle) = &self.inner.idle {
			idle.dispose();
		}
	}
}

impl Drop for CellControl {
	fn drop(&mut self) {
		if Arc::strong_count(&self.inner) == 1 {
			self.dispose();
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[tokio::test]
	async fn external_abort_is_held_while_deferred_then_applied() {
		let external = CancellationToken::new();
		let control = CellControl::new(external.clone(), None);
		control.pause_deferring_abort();
		external.cancel();
		tokio::time::sleep(Duration::from_millis(20)).await;
		assert!(!control.kernel_token().is_cancelled(), "shield must hold the kernel");
		control.resume_deferring_abort();
		let applied = tokio::time::timeout(Duration::from_secs(1), async {
			while !control.kernel_token().is_cancelled() {
				tokio::time::sleep(Duration::from_millis(5)).await;
			}
		})
		.await;
		assert!(applied.is_ok(), "abort must resume after the shielded phase");
		control.dispose();
	}

	#[tokio::test]
	async fn plain_pause_does_not_defer_external_abort() {
		let external = CancellationToken::new();
		let control = CellControl::new(external.clone(), None);
		control.pause();
		external.cancel();
		let applied = tokio::time::timeout(Duration::from_secs(1), async {
			while !control.kernel_token().is_cancelled() {
				tokio::time::sleep(Duration::from_millis(5)).await;
			}
		})
		.await;
		assert!(applied.is_ok(), "plain pause must not defer the abort");
		control.dispose();
	}

	#[tokio::test]
	async fn pause_ignores_expiry_and_resume_restarts_window() {
		let idle = IdleTimeout::new(Duration::from_millis(100));
		idle.pause();
		tokio::time::sleep(Duration::from_millis(250)).await;
		assert!(!idle.token().is_cancelled());
		idle.resume();
		tokio::time::sleep(Duration::from_millis(60)).await;
		assert!(!idle.token().is_cancelled());
		tokio::time::sleep(Duration::from_millis(80)).await;
		assert!(idle.token().is_cancelled());
		assert!(idle.timed_out());
	}
}
