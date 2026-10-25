use crate::state::Message;

/// Fast heuristic tokenizer: calculates token count approximations.
/// Roughly 3.5 - 4.0 chars per token for English/code, plus overhead per message/tool call.
pub struct Tokenizer;

impl Tokenizer {
	/// Heuristic token count for a text string.
	#[must_use]
	pub fn count_text(text: &str) -> usize {
		if text.is_empty() {
			return 0;
		}
		// 1 token ~= 4 utf-8 bytes (minimum 1 token if non-empty)
		let bytes = text.len();
		(bytes.div_ceil(4)).max(1)
	}

	/// Estimate tokens for a single message including message envelope framing.
	#[must_use]
	pub fn count_message(msg: &Message) -> usize {
		let mut tokens = 4; // Envelope overhead (<|im_start|>role\n ... <|im_end|>)
		tokens += Self::count_text(&msg.content);

		if let Some(name) = &msg.name {
			tokens += 1 + Self::count_text(name);
		}

		if let Some(id) = &msg.tool_call_id {
			tokens += 2 + Self::count_text(id);
		}

		for call in &msg.tool_calls {
			tokens += 6; // Tool call overhead
			tokens += Self::count_text(&call.id);
			tokens += Self::count_text(&call.name);
			tokens += Self::count_text(&call.arguments.to_string());
		}

		tokens
	}

	/// Estimate total tokens across messages.
	#[must_use]
	pub fn count_messages(messages: &[Message]) -> usize {
		if messages.is_empty() {
			return 0;
		}
		let mut sum = 3; // Prime conversation tokens
		for msg in messages {
			sum += Self::count_message(msg);
		}
		sum
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn test_token_counting() {
		assert_eq!(Tokenizer::count_text(""), 0);
		assert!(Tokenizer::count_text("hello world") >= 2);

		let msg = Message::user("test prompt");
		assert!(Tokenizer::count_message(&msg) > 4);

		let msgs = vec![Message::system("sys"), Message::user("usr")];
		assert!(Tokenizer::count_messages(&msgs) > 10);
	}
}
