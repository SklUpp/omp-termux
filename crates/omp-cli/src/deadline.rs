//! Turn deadline (`--max-time`).
//!
//! Enforcement happens twice: [`drive`] stops consuming the runner stream once
//! the budget passes (so already-produced events and partial text survive), and
//! the `pi-agent` runtime wraps its dialog in a tokio timeout so a stalled
//! provider or tool is actually interrupted. `elapsed` is injected, which keeps
//! the tests free of wall-clock sleeps.

use std::time::Duration;

use crate::agent::Event;
use crate::{Error, Result};

/// stderr message for an expired `--max-time` budget (TS prints
/// "Deadline exceeded" from the agent loop and exits 1).
pub const TIMEOUT_MESSAGE: &str = "max time exceeded";

#[derive(Debug, Clone, Copy)]
pub struct Deadline {
	budget: Duration,
}

impl Deadline {
	#[must_use]
	pub fn new(budget: Option<Duration>) -> Option<Self> {
		budget.map(|budget| Self { budget })
	}

	#[must_use]
	pub fn exceeded(&self, elapsed: Duration) -> bool {
		elapsed >= self.budget
	}

	#[must_use]
	pub fn remaining(&self, elapsed: Duration) -> Duration {
		self.budget.saturating_sub(elapsed)
	}
}

/// Consume a runner stream until it ends, the deadline passes, or an event
/// fails. Events observed before expiry have already been reported.
pub fn drive(
	events: impl Iterator<Item = Result<Event>>,
	deadline: Option<Deadline>,
	mut elapsed: impl FnMut() -> Duration,
	mut on_event: impl FnMut(&Event) -> Result<()>,
) -> Result<()> {
	for event in events {
		if deadline.is_some_and(|deadline| deadline.exceeded(elapsed())) {
			return Err(Error::new(TIMEOUT_MESSAGE));
		}
		let event = event?;
		on_event(&event)?;
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use std::collections::VecDeque;

	use super::*;

	fn event(text: &str) -> Result<Event> {
		Ok(Event::MessageEnd { text: text.to_string() })
	}

	#[test]
	fn budget_aborts_between_events_without_sleeping() {
		let deadline = Deadline::new(Some(Duration::from_secs(5)));
		let mut clock = VecDeque::from([
			Duration::ZERO,
			Duration::from_secs(1),
			Duration::from_secs(6),
			Duration::from_secs(7),
		]);
		let stream = vec![event("one"), event("two"), event("three")].into_iter();
		let mut observed = Vec::new();
		let result = drive(
			stream,
			deadline,
			|| clock.pop_front().unwrap_or(Duration::from_secs(60)),
			|event| {
				if let Event::MessageEnd { text } = event {
					observed.push(text.clone());
				}
				Ok(())
			},
		);
		let error = result.expect_err("deadline must expire");
		assert_eq!(error.to_string(), TIMEOUT_MESSAGE);
		assert_eq!(observed, ["one", "two"]);
	}

	#[test]
	fn no_budget_consumes_every_event() {
		let stream = vec![event("one"), event("two")].into_iter();
		let mut count = 0;
		drive(
			stream,
			None,
			|| Duration::from_secs(1_000),
			|_| {
				count += 1;
				Ok(())
			},
		)
		.expect("no deadline");
		assert_eq!(count, 2);
	}

	#[test]
	fn event_errors_propagate() {
		let stream = vec![Err(Error::new("boom"))].into_iter();
		let error =
			drive(stream, Deadline::new(Some(Duration::from_secs(5))), || Duration::ZERO, |_| Ok(()))
				.expect_err("runner error");
		assert_eq!(error.to_string(), "boom");
	}

	#[test]
	fn deadline_helpers_are_monotonic() {
		let deadline = Deadline::new(Some(Duration::from_secs(10))).expect("deadline");
		assert!(!deadline.exceeded(Duration::from_secs(9)));
		assert!(deadline.exceeded(Duration::from_secs(10)));
		assert_eq!(deadline.remaining(Duration::from_secs(4)), Duration::from_secs(6));
		assert_eq!(deadline.remaining(Duration::from_secs(12)), Duration::ZERO);
		assert!(Deadline::new(None).is_none());
	}
}
