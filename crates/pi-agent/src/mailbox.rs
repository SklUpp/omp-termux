//! Agent-to-agent mailbox.
//!
//! Mirrors the TypeScript `IrcBus` mailbox rules for the subset this build can
//! actually honour: a message handed to a live recipient (waiter or steering
//! channel) is never buffered, because buffering it too would deliver it twice
//! through a later `wait`/`inbox`; only a failed live hand-off is buffered.
//! There is no park/revive lifecycle here, so receipts never claim `woken` or
//! `revived`.

use std::{
	collections::{HashMap, HashSet, VecDeque},
	sync::{
		Arc, Mutex,
		atomic::{AtomicBool, AtomicU64, Ordering},
	},
	time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use tokio::sync::{oneshot, watch};
use tokio_util::sync::CancellationToken;

/// Root agent address. The TypeScript build calls it `Main`; this build uses
/// the same reserved id so a child can address the parent it knows about.
pub const MAIN_AGENT_ID: &str = "Main";
/// Per-agent mailbox cap; oldest messages are dropped past it.
pub const MAILBOX_CAP: usize = 100;
/// Default reply wait used by `send await:true` when no timeout is supplied.
pub const DEFAULT_IRC_TIMEOUT_MS: u64 = 120_000;

static MESSAGE_SEQ: AtomicU64 = AtomicU64::new(1);

fn now_ms() -> u64 {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map(|duration| duration.as_millis() as u64)
		.unwrap_or_default()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IrcMessage {
	pub id:      String,
	pub from:    String,
	pub to:      String,
	pub body:    String,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub reply_to: Option<String>,
	pub ts:      u64,
}

impl IrcMessage {
	pub fn new(from: impl Into<String>, to: impl Into<String>, body: impl Into<String>) -> Self {
		Self {
			id: format!("msg-{}", MESSAGE_SEQ.fetch_add(1, Ordering::Relaxed)),
			from: from.into(),
			to: to.into(),
			body: body.into(),
			reply_to: None,
			ts: now_ms(),
		}
	}

	pub fn with_reply_to(mut self, reply_to: Option<&str>) -> Self {
		self.reply_to = reply_to.map(str::to_string);
		self
	}
}

/// Model-facing message line, identical in shape to the TypeScript
/// `formatIncoming`.
pub fn format_incoming(message: &IrcMessage) -> String {
	let reply_tag = message
		.reply_to
		.as_ref()
		.map(|reply| format!(" (reply to {reply})"))
		.unwrap_or_default();
	format!("[{}] {}{}: {}", message.id, message.from, reply_tag, message.body)
}

struct Waiter {
	id:   u64,
	from: Option<String>,
	tx:   oneshot::Sender<IrcMessage>,
	/// Set when the hand-off succeeds, so the waiter's drop knows it must
	/// release one reserved in-flight slot.
	delivered: Arc<AtomicBool>,
}

#[derive(Default)]
struct MailboxState {
	mailboxes: HashMap<String, VecDeque<IrcMessage>>,
	waiters:   HashMap<String, Vec<Waiter>>,
	/// Messages handed to a waiter but not yet observed by its receiver. They
	/// count against [`MAILBOX_CAP`] together with buffered mail, so recovery
	/// can always fit without evicting older messages.
	inflight:  HashMap<String, usize>,
	next_waiter: u64,
	/// Agents with a running loop or hub wait. Root (`Main`) appears while a
	/// root turn with a peer inbox is executing; children register themselves.
	active:    HashSet<String>,
}

/// Shared mailbox store. Cheap to clone: all clones share one map.
#[derive(Clone)]
pub struct Mailbox {
	state: Arc<Mutex<MailboxState>>,
	/// Monotonic presence version; every active-set change bumps it so a peer
	/// wait can wake when the last eligible peer exits instead of blocking until
	/// its timeout.
	presence: watch::Sender<u64>,
}

impl Default for Mailbox {
	fn default() -> Self {
		let (presence, _) = watch::channel(0);
		Self { state: Arc::new(Mutex::new(MailboxState::default())), presence }
	}
}

impl Mailbox {
	pub fn new() -> Self {
		Self::default()
	}

	/// Buffer a message whose live hand-off failed.
	pub fn buffer(&self, message: IrcMessage) {
		let mut state = self.state.lock().unwrap_or_else(|poison| poison.into_inner());
		buffer_locked(&mut state, message);
	}

	/// Hand a message to the oldest matching parked waiter. Returns `false`
	/// when no waiter accepted it (none registered, stale, or capacity fully
	/// reserved); the message is then **not consumed** and the caller retains
	/// ownership, so it must try live steering or buffer it.
	pub fn deliver_to_waiter(&self, message: &IrcMessage) -> bool {
		let mut state = self.state.lock().unwrap_or_else(|poison| poison.into_inner());
		let buffered = state.mailboxes.get(&message.to).map_or(0, VecDeque::len);
		let inflight = state.inflight.get(&message.to).copied().unwrap_or(0);
		if buffered + inflight >= MAILBOX_CAP {
			return false;
		}
		let mut delivered = false;
		{
			let Some(waiters) = state.waiters.get_mut(&message.to) else {
				return false;
			};
			while let Some(index) = waiters
				.iter()
				.position(|waiter| waiter.from.as_deref().is_none_or(|from| from == message.from))
			{
				let waiter = waiters.remove(index);
				if waiter.tx.send(message.clone()).is_ok() {
					waiter.delivered.store(true, Ordering::Release);
					delivered = true;
					break;
				}
			}
		}
		if delivered {
			*state.inflight.entry(message.to.clone()).or_default() += 1;
			return true;
		}
		false
	}

	/// Register a future-message waiter. The guard removes itself on drop, so a
	/// timeout or cancellation never leaves a stale waiter behind. Prefer
	/// [`Self::take_or_register`] in waits; this alone can race buffered mail.
	pub fn register(&self, agent: &str, from: Option<&str>) -> MailboxWaiter {
		let mut state = self.state.lock().unwrap_or_else(|poison| poison.into_inner());
		let (id, rx, delivered) = push_waiter(&mut state, agent, from);
		MailboxWaiter {
			mailbox: self.clone(),
			agent: agent.to_string(),
			id,
			rx,
			delivered,
		}
	}

	/// Consume the oldest buffered message for `agent`, optionally restricted
	/// to one sender. Atomic with respect to concurrent waiters.
	pub fn take(&self, agent: &str, from: Option<&str>) -> Option<IrcMessage> {
		let mut state = self.state.lock().unwrap_or_else(|poison| poison.into_inner());
		take_from(&mut state, agent, from)
	}

	/// Atomically take a buffered message or register a waiter, never both.
	///
	/// Registering before looking at the buffer races a concurrent sender: it
	/// can hand a second message to the waiter, which the caller then drops
	/// after returning the buffered one. Senders that see no waiter buffer the
	/// message instead, so the message is still reachable through `inbox`.
	pub fn take_or_register(
		&self,
		agent: &str,
		from: Option<&str>,
	) -> (Option<IrcMessage>, Option<MailboxWaiter>) {
		let mut state = self.state.lock().unwrap_or_else(|poison| poison.into_inner());
		if let Some(message) = take_from(&mut state, agent, from) {
			return (Some(message), None);
		}
		let (id, rx, delivered) = push_waiter(&mut state, agent, from);
		(
			None,
			Some(MailboxWaiter {
				mailbox: self.clone(),
				agent: agent.to_string(),
				id,
				rx,
				delivered,
			}),
		)
	}

	/// Mark an agent as running (root turn with a peer inbox, child loop, hub
	/// waiter) or stopped. Presence changes wake peer waits through [`Self::presence`].
	pub fn set_agent_active(&self, agent: &str, active: bool) {
		let changed = {
			let mut state = self.state.lock().unwrap_or_else(|poison| poison.into_inner());
			if active {
				state.active.insert(agent.to_string())
			} else {
				state.active.remove(agent)
			}
		};
		if changed {
			self.presence.send_modify(|version| *version = version.wrapping_add(1));
		}
	}

	pub fn is_agent_active(&self, agent: &str) -> bool {
		self
			.state
			.lock()
			.unwrap_or_else(|poison| poison.into_inner())
			.active
			.contains(agent)
	}

	pub fn active_agents(&self) -> Vec<String> {
		self
			.state
			.lock()
			.unwrap_or_else(|poison| poison.into_inner())
			.active
			.iter()
			.cloned()
			.collect()
	}

	/// Subscribe to presence-version changes for peer-liveness waits.
	pub fn presence(&self) -> watch::Receiver<u64> {
		self.presence.subscribe()
	}

	pub fn inbox(&self, agent: &str, peek: bool) -> Vec<IrcMessage> {
		let mut state = self.state.lock().unwrap_or_else(|poison| poison.into_inner());
		if peek {
			return state
				.mailboxes
				.get(agent)
				.map(|mailbox| mailbox.iter().cloned().collect())
				.unwrap_or_default();
		}
		state
			.mailboxes
			.remove(agent)
			.map(|mailbox| mailbox.into_iter().collect())
			.unwrap_or_default()
	}

	pub fn unread_count(&self, agent: &str) -> usize {
		self
			.state
			.lock()
			.unwrap_or_else(|poison| poison.into_inner())
			.mailboxes
			.get(agent)
			.map(VecDeque::len)
			.unwrap_or_default()
	}

	/// Convenience wait: consume buffered mail first, then park a waiter.
	/// `timeout_ms == 0` waits until a message or cancellation.
	pub async fn wait(
		&self,
		agent: &str,
		from: Option<&str>,
		timeout_ms: u64,
		cancel: &CancellationToken,
	) -> Option<IrcMessage> {
		let (buffered, waiter) = self.take_or_register(agent, from);
		if let Some(message) = buffered {
			return Some(message);
		}
		let Some(mut waiter) = waiter else { return None };
		let deadline = (timeout_ms > 0)
			.then(|| tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms));
		loop {
			let receive = waiter.recv();
			tokio::pin!(receive);
			match deadline {
				Some(deadline) => {
					tokio::select! {
						_ = cancel.cancelled() => return None,
						message = &mut receive => return message,
						_ = tokio::time::sleep_until(deadline) => return None,
					}
				},
				None => {
					tokio::select! {
						_ = cancel.cancelled() => return None,
						message = &mut receive => return message,
					}
				},
			}
		}
	}
}

/// RAII waiter slot; dropping it removes the slot so a timed-out wait cannot
/// steal a later message.
pub struct MailboxWaiter {
	mailbox:   Mailbox,
	agent:     String,
	id:        u64,
	rx:        oneshot::Receiver<IrcMessage>,
	delivered: Arc<AtomicBool>,
}

impl MailboxWaiter {
	pub async fn recv(&mut self) -> Option<IrcMessage> {
		(&mut self.rx).await.ok()
	}
}

impl Drop for MailboxWaiter {
	fn drop(&mut self) {
		// A successful oneshot hand-off is invisible until polled. If the wait
		// returns through another branch (job snapshot, peer exit, timeout,
		// cancellation), close the receiver and recover any delivered message
		// into the bounded mailbox instead of silently dropping it. `close`
		// happens first, so a concurrent sender either wins with a value we
		// recover here or fails and buffers it itself.
		self.rx.close();
		let recovered = self.rx.try_recv().ok();
		let mut state = match self.mailbox.state.lock() {
			Ok(state) => state,
			Err(poison) => poison.into_inner(),
		};
		let empty = state.waiters.get_mut(&self.agent).map(|waiters| {
			waiters.retain(|waiter| waiter.id != self.id);
			waiters.is_empty()
		});
		if empty == Some(true) {
			state.waiters.remove(&self.agent);
		}
		if self.delivered.swap(false, Ordering::AcqRel) {
			let release = state.inflight.get_mut(&self.agent).map(|count| {
				*count = count.saturating_sub(1);
				*count
			});
			if release == Some(0) {
				state.inflight.remove(&self.agent);
			}
		}
		if let Some(message) = recovered {
			buffer_locked(&mut state, message);
		}
	}
}

/// Bounded buffer insertion shared by [`Mailbox::buffer`] and waiter-drop
/// recovery. Oldest mail is evicted at the cap, matching the existing policy.
fn buffer_locked(state: &mut MailboxState, message: IrcMessage) {
	let inflight = state.inflight.get(&message.to).copied().unwrap_or(0);
	let cap = MAILBOX_CAP.saturating_sub(inflight);
	let mailbox = state.mailboxes.entry(message.to.clone()).or_default();
	mailbox.push_back(message);
	while mailbox.len() > cap {
		mailbox.pop_front();
	}
}

fn push_waiter(
	state: &mut MailboxState,
	agent: &str,
	from: Option<&str>,
) -> (u64, oneshot::Receiver<IrcMessage>, Arc<AtomicBool>) {
	let id = state.next_waiter;
	state.next_waiter += 1;
	let (tx, rx) = oneshot::channel();
	let delivered = Arc::new(AtomicBool::new(false));
	state
		.waiters
		.entry(agent.to_string())
		.or_default()
		.push(Waiter {
			id,
			from: from.map(str::to_string),
			tx,
			delivered: Arc::clone(&delivered),
		});
	(id, rx, delivered)
}

fn take_from(state: &mut MailboxState, agent: &str, from: Option<&str>) -> Option<IrcMessage> {
	let mailbox = state.mailboxes.get_mut(agent)?;
	let index = mailbox
		.iter()
		.position(|message| from.is_none_or(|from| message.from == from))?;
	let message = mailbox.remove(index);
	if mailbox.is_empty() {
		state.mailboxes.remove(agent);
	}
	message
}

#[cfg(test)]
mod tests {
	use super::*;

	#[tokio::test]
	async fn buffered_message_wins_then_waiter_receives_once() {
		let mailbox = Mailbox::new();
		mailbox.buffer(IrcMessage::new("a", "b", "one"));
		assert_eq!(mailbox.take("b", None).unwrap().body, "one");
		assert!(mailbox.take("b", None).is_none());

		let mut waiter = mailbox.register("b", Some("a"));
		assert!(!mailbox.deliver_to_waiter(&IrcMessage::new("c", "b", "wrong")));
		assert!(mailbox.deliver_to_waiter(&IrcMessage::new("a", "b", "two")));
		assert_eq!(waiter.recv().await.unwrap().body, "two");
		assert_eq!(mailbox.unread_count("b"), 0);
	}

	#[tokio::test]
	async fn waiter_drop_does_not_swallow_later_mail() {
		let mailbox = Mailbox::new();
		let waiter = mailbox.register("b", None);
		drop(waiter);
		let later = IrcMessage::new("a", "b", "later");
		// No waiter accepted the hand-off: `false` means the sender still owns
		// the message. A real sender falls back to live steering or buffers it;
		// buffer here to prove the dropped waiter did not swallow it.
		assert!(!mailbox.deliver_to_waiter(&later));
		mailbox.buffer(later);
		assert_eq!(mailbox.unread_count("b"), 1);
		assert_eq!(mailbox.take("b", None).expect("reachable mail").body, "later");
	}

	#[test]
	fn take_or_register_is_atomic_and_never_hands_off_into_a_dropped_waiter() {
		let mailbox = Mailbox::new();
		mailbox.buffer(IrcMessage::new("a", "b", "one"));
		let (message, waiter) = mailbox.take_or_register("b", None);
		assert_eq!(message.expect("buffered message").body, "one");
		assert!(waiter.is_none(), "buffered take must not also register a waiter");
		// With no waiter, a racing sender buffers instead of handing off to a
		// guard this call already dropped; the message stays reachable.
		assert!(!mailbox.deliver_to_waiter(&IrcMessage::new("a", "b", "two")));
		mailbox.buffer(IrcMessage::new("a", "b", "two"));
		assert_eq!(mailbox.take("b", None).expect("late mail").body, "two");
		assert!(mailbox.take("b", None).is_none());
	}

	#[test]
	fn dropping_a_waiter_with_an_unpolled_delivery_requeues_it_once() {
		let mailbox = Mailbox::new();
		let waiter = mailbox.register("b", None);
		// Sender wins the hand-off before the wait polls its receiver.
		assert!(mailbox.deliver_to_waiter(&IrcMessage::new("a", "b", "handoff")));
		drop(waiter);
		// The delivered body must not vanish with the dropped wait.
		assert_eq!(mailbox.take("b", None).expect("recovered hand-off").body, "handoff");
		assert!(mailbox.take("b", None).is_none(), "recovery must be once-only");
	}

	#[test]
	fn recovered_handoffs_fit_reserved_capacity_without_evicting_mail() {
		let mailbox = Mailbox::new();
		for index in 0..MAILBOX_CAP - 1 {
			mailbox.buffer(IrcMessage::new("a", "b", format!("buffered{index}")));
		}
		let waiter = mailbox.register("b", None);
		assert!(mailbox.deliver_to_waiter(&IrcMessage::new("a", "b", "handoff")));
		drop(waiter);
		let all = mailbox.inbox("b", true);
		assert_eq!(all.len(), MAILBOX_CAP);
		assert_eq!(all[0].body, "buffered0", "oldest buffered mail must survive");
		assert_eq!(all.last().unwrap().body, "handoff");
	}

	#[test]
	fn handoff_is_refused_when_capacity_including_inflight_is_full() {
		let mailbox = Mailbox::new();
		let mut waiters: Vec<_> = (0..MAILBOX_CAP).map(|_| mailbox.register("b", None)).collect();
		for index in 0..MAILBOX_CAP {
			assert!(
				mailbox.deliver_to_waiter(&IrcMessage::new("a", "b", format!("handoff{index}"))),
				"in-flight handoffs must fit the cap"
			);
		}
		assert!(
			!mailbox.deliver_to_waiter(&IrcMessage::new("a", "b", "overflow")),
			"a hand-off beyond capacity must fail instead of being reported injected"
		);
		drop(waiters.drain(..));
		let all = mailbox.inbox("b", true);
		assert_eq!(all.len(), MAILBOX_CAP);
		assert_eq!(all[0].body, "handoff0");
		assert_eq!(all.last().unwrap().body, format!("handoff{}", MAILBOX_CAP - 1));
	}

	#[test]
	fn handoff_is_refused_when_buffered_mail_already_fills_the_cap() {
		let mailbox = Mailbox::new();
		for index in 0..MAILBOX_CAP {
			mailbox.buffer(IrcMessage::new("a", "b", format!("buffered{index}")));
		}
		let _waiter = mailbox.register("b", None);
		assert!(!mailbox.deliver_to_waiter(&IrcMessage::new("a", "b", "late")));
		assert_eq!(mailbox.inbox("b", true).len(), MAILBOX_CAP);
	}

	#[tokio::test]
	async fn presence_changes_are_observable_by_peer_waits() {
		let mailbox = Mailbox::new();
		let mut presence = mailbox.presence();
		mailbox.set_agent_active(MAIN_AGENT_ID, true);
		presence.changed().await.expect("presence bump");
		assert!(mailbox.is_agent_active(MAIN_AGENT_ID));
		assert_eq!(mailbox.active_agents(), vec![MAIN_AGENT_ID.to_string()]);
		mailbox.set_agent_active(MAIN_AGENT_ID, false);
		presence.changed().await.expect("presence bump");
		assert!(!mailbox.is_agent_active(MAIN_AGENT_ID));
	}

	#[test]
	fn cap_drops_oldest() {
		let mailbox = Mailbox::new();
		for index in 0..(MAILBOX_CAP + 2) {
			mailbox.buffer(IrcMessage::new("a", "b", index.to_string()));
		}
		let inbox = mailbox.inbox("b", false);
		assert_eq!(inbox.len(), MAILBOX_CAP);
		assert_eq!(inbox.first().unwrap().body, "2");
	}
}
