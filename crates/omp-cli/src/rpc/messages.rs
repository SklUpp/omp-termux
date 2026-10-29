//! Stable-snapshot cursor pagination for `get_messages_page`.
//!
//! Exact port of `packages/coding-agent/src/modes/rpc/rpc-messages.ts`: the
//! cursor is unpadded base64url of `{version:1, sessionId, leafId,
//! messageCount, offset}`, re-encoded canonically before use, and any change to
//! the session/leaf/message count makes it stale.

use serde_json::{Map, Value, json};

use super::frame::{base64_encode, decode_base64_strict};

/// Messages per page when the caller omits `limit`.
pub const DEFAULT_RPC_MESSAGE_PAGE_LIMIT: usize = 100;
/// Hard page ceiling.
pub const MAX_RPC_MESSAGE_PAGE_LIMIT: usize = 256;
/// Soft payload budget; measured in UTF-8 bytes of the JSON encoding.
pub const MAX_RPC_MESSAGE_PAGE_BYTES: usize = 768 * 1024;
/// Longest accepted cursor, in characters.
pub const MAX_RPC_MESSAGE_CURSOR_CHARS: usize = 2048;

pub const RPC_MESSAGES_PAGE_BUSY_ERROR: &str = "Cannot page messages while the session is changing";
pub const RPC_MESSAGES_PAGE_STALE_ERROR: &str = "RPC message cursor is stale";

/// Machine-readable paging failures carried as the response `code`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageErrorCode {
	SessionBusy,
	StaleCursor,
}

impl PageErrorCode {
	#[must_use]
	pub fn as_str(self) -> &'static str {
		match self {
			Self::SessionBusy => "session_busy",
			Self::StaleCursor => "stale_cursor",
		}
	}
}

/// Paging failure with an optional wire code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageError {
	pub message: String,
	pub code:    Option<PageErrorCode>,
}

impl PageError {
	fn plain(message: impl Into<String>) -> Self {
		Self { message: message.into(), code: None }
	}

	fn coded(message: impl Into<String>, code: PageErrorCode) -> Self {
		Self { message: message.into(), code: Some(code) }
	}
}

impl std::fmt::Display for PageError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str(&self.message)
	}
}

/// Identity of the message list a cursor was issued against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageSnapshot {
	pub session_id:    String,
	pub leaf_id:       Option<String>,
	pub message_count: usize,
}

/// One page plus the cursor for the next one.
#[derive(Debug, Clone, PartialEq)]
pub struct MessagePage {
	pub messages:      Vec<Value>,
	pub next_cursor:   Option<String>,
	pub total_messages: usize,
}

impl MessagePage {
	#[must_use]
	pub fn to_wire(&self) -> Value {
		let mut value = Map::new();
		value.insert("messages".to_string(), Value::Array(self.messages.clone()));
		if let Some(cursor) = &self.next_cursor {
			value.insert("nextCursor".to_string(), json!(cursor));
		}
		value.insert("totalMessages".to_string(), json!(self.total_messages));
		Value::Object(value)
	}
}

struct CursorPayload {
	session_id:    String,
	leaf_id:       Option<String>,
	message_count: usize,
	offset:        usize,
}

fn encode_base64url(bytes: &[u8]) -> String {
	let standard = base64_encode(bytes);
	let mut out = String::with_capacity(standard.len());
	for byte in standard.bytes() {
		match byte {
			b'+' => out.push('-'),
			b'/' => out.push('_'),
			b'=' => {},
			other => out.push(char::from(other)),
		}
	}
	out
}

fn decode_base64url_exact(cursor: &str) -> Option<Vec<u8>> {
	if cursor.is_empty() || cursor.chars().count() > MAX_RPC_MESSAGE_CURSOR_CHARS {
		return None;
	}
	if !cursor
		.bytes()
		.all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
	{
		return None;
	}
	let mut standard = String::with_capacity(cursor.len() + 3);
	for byte in cursor.bytes() {
		match byte {
			b'-' => standard.push('+'),
			b'_' => standard.push('/'),
			other => standard.push(char::from(other)),
		}
	}
	while standard.len() % 4 != 0 {
		standard.push('=');
	}
	let bytes = decode_base64_strict(&standard).ok()?;
	// Canonical form only: a padded or non-canonical encoding is rejected.
	if encode_base64url(&bytes) != cursor {
		return None;
	}
	Some(bytes)
}

fn encode_cursor(snapshot: &MessageSnapshot, offset: usize) -> String {
	let payload = json!({
		"version": 1,
		"sessionId": snapshot.session_id,
		"leafId": snapshot.leaf_id,
		"messageCount": snapshot.message_count,
		"offset": offset,
	});
	encode_base64url(payload.to_string().as_bytes())
}

fn decode_cursor(cursor: &str) -> Result<CursorPayload, PageError> {
	let invalid = || PageError::plain("Invalid RPC message cursor");
	let bytes = decode_base64url_exact(cursor).ok_or_else(invalid)?;
	let text = String::from_utf8(bytes).map_err(|_| invalid())?;
	let value: Value = serde_json::from_str(&text).map_err(|_| invalid())?;
	let object = value.as_object().ok_or_else(invalid)?;
	let version = object.get("version").and_then(Value::as_u64);
	if version != Some(1) {
		return Err(invalid());
	}
	let session_id = object.get("sessionId").and_then(Value::as_str).ok_or_else(invalid)?;
	if session_id.is_empty() || session_id.chars().count() > 256 {
		return Err(invalid());
	}
	let leaf_id = match object.get("leafId") {
		Some(Value::Null) => None,
		Some(Value::String(leaf)) if !leaf.is_empty() && leaf.chars().count() <= 256 => {
			Some(leaf.clone())
		},
		_ => return Err(invalid()),
	};
	let message_count = safe_offset(object.get("messageCount")).ok_or_else(invalid)?;
	let offset = safe_offset(object.get("offset")).ok_or_else(invalid)?;
	if offset > message_count {
		return Err(invalid());
	}
	Ok(CursorPayload {
		session_id: session_id.to_string(),
		leaf_id,
		message_count,
		offset,
	})
}

fn safe_offset(value: Option<&Value>) -> Option<usize> {
	usize::try_from(value?.as_u64()?).ok()
}

/// Page `messages` against `snapshot`.
pub fn page_messages(
	messages: &[Value],
	snapshot: &MessageSnapshot,
	options: (Option<&str>, Option<i64>),
) -> Result<MessagePage, PageError> {
	let (cursor, limit) = options;
	if snapshot.message_count != messages.len() {
		return Err(PageError::plain("RPC message snapshot does not match current messages"));
	}
	let limit = match limit {
		None => DEFAULT_RPC_MESSAGE_PAGE_LIMIT,
		Some(limit) => {
			if limit < 1 || limit > MAX_RPC_MESSAGE_PAGE_LIMIT as i64 {
				return Err(PageError::plain(format!(
					"RPC message page limit must be between 1 and {MAX_RPC_MESSAGE_PAGE_LIMIT}"
				)));
			}
			limit as usize
		},
	};
	let mut offset = 0;
	if let Some(cursor) = cursor {
		let decoded = decode_cursor(cursor)?;
		if decoded.session_id != snapshot.session_id
			|| decoded.leaf_id != snapshot.leaf_id
			|| decoded.message_count != snapshot.message_count
		{
			return Err(PageError::coded(
				RPC_MESSAGES_PAGE_STALE_ERROR,
				PageErrorCode::StaleCursor,
			));
		}
		offset = decoded.offset;
	}

	let mut page = Vec::new();
	let mut page_bytes = 2usize;
	while offset + page.len() < messages.len() && page.len() < limit {
		let message = &messages[offset + page.len()];
		let message_bytes = message.to_string().len() + usize::from(!page.is_empty());
		if !page.is_empty() && page_bytes + message_bytes > MAX_RPC_MESSAGE_PAGE_BYTES {
			break;
		}
		page.push(message.clone());
		page_bytes += message_bytes;
	}

	let next_offset = offset + page.len();
	Ok(MessagePage {
		messages: page,
		next_cursor: (next_offset < messages.len())
			.then(|| encode_cursor(snapshot, next_offset)),
		total_messages: messages.len(),
	})
}

#[cfg(test)]
mod tests {
	use super::*;

	fn message(role: &str, timestamp: u64) -> Value {
		json!({ "role": role, "content": format!("{role}-{timestamp}"), "timestamp": timestamp })
	}

	fn transcript(count: usize) -> Vec<Value> {
		(0..count)
			.map(|index| message("assistant", index as u64 + 1))
			.collect()
	}

	fn snapshot(count: usize) -> MessageSnapshot {
		MessageSnapshot {
			session_id:    "session-1".to_string(),
			leaf_id:       Some("leaf-1".to_string()),
			message_count: count,
		}
	}

	#[test]
	fn pages_walk_without_duplicates_and_report_total() {
		let messages = transcript(5);
		let snapshot = snapshot(messages.len());
		let first = page_messages(&messages, &snapshot, (None, Some(2))).expect("first");
		assert_eq!(first.messages, messages[..2]);
		assert_eq!(first.total_messages, 5);
		let second = page_messages(
			&messages,
			&snapshot,
			(first.next_cursor.as_deref(), Some(2)),
		)
		.expect("second");
		assert_eq!(second.messages, messages[2..4]);
		let third = page_messages(
			&messages,
			&snapshot,
			(second.next_cursor.as_deref(), Some(2)),
		)
		.expect("third");
		assert_eq!(third.messages, messages[4..]);
		assert!(third.next_cursor.is_none());
	}

	#[test]
	fn snapshot_mismatch_and_stale_cursor_are_distinct_errors() {
		let messages = transcript(3);
		let mismatch = page_messages(&messages, &snapshot(4), (None, None)).expect_err("mismatch");
		assert_eq!(mismatch.message, "RPC message snapshot does not match current messages");
		assert_eq!(mismatch.code, None);

		let current_snapshot = snapshot(messages.len());
		let first = page_messages(&messages, &current_snapshot, (None, Some(1))).expect("first");
		let cursor = first.next_cursor.expect("cursor");
		let mut grown = messages.clone();
		grown.push(message("user", 9));
		let stale = page_messages(&grown, &snapshot(4), (Some(&cursor), None)).expect_err("stale");
		assert_eq!(stale.message, RPC_MESSAGES_PAGE_STALE_ERROR);
		assert_eq!(stale.code, Some(PageErrorCode::StaleCursor));
		assert!(stale.to_string().contains("stale"));
	}

	#[test]
	fn cursor_is_canonical_base64url_json() {
		let messages = transcript(2);
		let snapshot = snapshot(messages.len());
		let page = page_messages(&messages, &snapshot, (None, Some(1))).expect("page");
		let cursor = page.next_cursor.expect("cursor");
		assert!(!cursor.contains('='));
		assert!(!cursor.contains('+'));
		assert!(!cursor.contains('/'));
		let raw = decode_base64url_exact(&cursor).expect("decode");
		let parsed: Value = serde_json::from_slice(&raw).expect("json");
		assert_eq!(parsed["version"], json!(1));
		assert_eq!(parsed["sessionId"], json!("session-1"));
		assert_eq!(parsed["leafId"], json!("leaf-1"));
		assert_eq!(parsed["messageCount"], json!(2));
		assert_eq!(parsed["offset"], json!(1));
	}

	#[test]
	fn invalid_cursors_are_rejected() {
		let messages = transcript(2);
		let snapshot = snapshot(messages.len());
		for cursor in [
			"".to_string(),
			"!!!".to_string(),
			"a".repeat(MAX_RPC_MESSAGE_CURSOR_CHARS + 1),
			encode_base64url(br#"{"version":2,"sessionId":"s","leafId":null,"messageCount":2,"offset":0}"#),
			encode_base64url(br#"{"version":1,"sessionId":"","leafId":null,"messageCount":2,"offset":0}"#),
			encode_base64url(br#"{"version":1,"sessionId":"s","leafId":null,"messageCount":2,"offset":3}"#),
			encode_base64url(b"not json"),
		] {
			let error = page_messages(&messages, &snapshot, (Some(&cursor), None)).expect_err("invalid");
			assert_eq!(error.message, "Invalid RPC message cursor", "cursor {cursor}");
		}
	}

	#[test]
	fn padded_cursor_is_rejected_because_reencoding_differs() {
		let messages = transcript(2);
		let snapshot = snapshot(messages.len());
		let page = page_messages(&messages, &snapshot, (None, Some(1))).expect("page");
		let cursor = page.next_cursor.expect("cursor");
		let padded = format!("{cursor}=");
		let error = page_messages(&messages, &snapshot, (Some(&padded), None)).expect_err("padded");
		assert_eq!(error.message, "Invalid RPC message cursor");
	}

	#[test]
	fn limit_bounds_and_defaults() {
		let messages = transcript(300);
		let snapshot = snapshot(messages.len());
		assert_eq!(
			page_messages(&messages, &snapshot, (None, None))
				.expect("default")
				.messages
				.len(),
			DEFAULT_RPC_MESSAGE_PAGE_LIMIT
		);
		let error = page_messages(&messages, &snapshot, (None, Some(0))).expect_err("zero");
		assert_eq!(error.message, "RPC message page limit must be between 1 and 256");
		let error = page_messages(&messages, &snapshot, (None, Some(257))).expect_err("too big");
		assert_eq!(error.message, "RPC message page limit must be between 1 and 256");
		assert_eq!(
			page_messages(&messages, &snapshot, (None, Some(256)))
				.expect("max")
				.messages
				.len(),
			256
		);
	}

	#[test]
	fn byte_budget_always_returns_one_message() {
		let huge = "x".repeat(MAX_RPC_MESSAGE_PAGE_BYTES + 10);
		let messages = vec![
			json!({ "role": "assistant", "content": huge, "timestamp": 1 }),
			message("assistant", 2),
		];
		let snapshot = snapshot(messages.len());
		let page = page_messages(&messages, &snapshot, (None, None)).expect("page");
		assert_eq!(page.messages.len(), 1);
		let next = page_messages(
			&messages,
			&snapshot,
			(page.next_cursor.as_deref(), None),
		)
		.expect("next");
		assert_eq!(next.messages, vec![messages[1].clone()]);
	}

	#[test]
	fn empty_transcript_returns_empty_page() {
		let messages: Vec<Value> = Vec::new();
		let page = page_messages(&messages, &snapshot(0), (None, None)).expect("page");
		assert!(page.messages.is_empty());
		assert!(page.next_cursor.is_none());
		assert_eq!(page.total_messages, 0);
		assert_eq!(page.to_wire(), json!({ "messages": [], "totalMessages": 0 }));
	}
}
