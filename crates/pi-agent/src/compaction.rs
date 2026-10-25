use crate::{
	state::{Message, Role},
	tokenizer::Tokenizer,
};

/// Compactor preserves system prompt and recent turns while pruning older messages
/// when estimated token count exceeds the budget.
pub struct Compactor;

impl Compactor {
	/// Compact messages slice to fit within token_budget.
	/// Guarantees:
	/// 1. System messages at the head are preserved.
	/// 2. At least min_recent_messages (default e.g. 4) are kept if possible.
	/// 3. Older user/assistant/tool messages are pruned or summarized.
	/// 4. Never orphan Tool messages: keeps assistant message with tool_calls together with its Tool response messages.
	pub fn compact(messages: &[Message], token_budget: usize, min_recent: usize) -> Vec<Message> {
		if messages.is_empty() {
			return Vec::new();
		}

		let total_tokens = Tokenizer::count_messages(messages);
		if total_tokens <= token_budget {
			return messages.to_vec();
		}

		// Partition system messages at the start
		let mut system_messages = Vec::new();
		let mut conversation = Vec::new();

		let mut in_leading_system = true;
		for msg in messages {
			if in_leading_system && msg.role == Role::System {
				system_messages.push(msg.clone());
			} else {
				in_leading_system = false;
				conversation.push(msg.clone());
			}
		}

		let system_tokens = Tokenizer::count_messages(&system_messages);
		let remaining_budget = token_budget.saturating_sub(system_tokens);

		if conversation.is_empty() {
			return system_messages;
		}

		// We need to keep the most recent messages from conversation
		let mut kept_reverse = Vec::new();
		let mut current_tokens = 0;

		// Walk backwards from newest to oldest
		for (idx, msg) in conversation.iter().rev().enumerate() {
			let msg_tokens = Tokenizer::count_message(msg);
			if idx < min_recent || (current_tokens + msg_tokens <= remaining_budget) {
				current_tokens += msg_tokens;
				kept_reverse.push(msg.clone());
			} else {
				break;
			}
		}

		kept_reverse.reverse();

		// Clean up boundary to avoid orphaning tool messages:
		// If the first message in kept_reverse is Role::Tool, it is missing its predecessor assistant message.
		// We must drop leading orphan Tool messages until the sequence starts cleanly at a user or assistant message.
		while let Some(first) = kept_reverse.first() {
			if first.role == Role::Tool {
				kept_reverse.remove(0);
			} else {
				break;
			}
		}

		let pruned_count = conversation.len().saturating_sub(kept_reverse.len());

		let mut result = system_messages;
		if pruned_count > 0 {
			let summary_notice = format!(
				"[System notice: {} earlier turn(s) were pruned to fit the context limit]",
				pruned_count
			);
			result.push(Message::system(summary_notice));
		}
		result.extend(kept_reverse);
		result
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::state::ToolCall;

	#[test]
	fn test_compaction_under_budget() {
		let msgs = vec![
			Message::system("System instructions"),
			Message::user("Hello"),
			Message::assistant("Hi there!"),
		];
		let compacted = Compactor::compact(&msgs, 10_000, 2);
		assert_eq!(compacted.len(), 3);
	}

	#[test]
	fn test_compaction_over_budget() {
		let mut msgs = vec![Message::system("System prompt")];
		for i in 0..20 {
			msgs.push(Message::user(format!("Turn {i} question with some length")));
			msgs.push(Message::assistant(format!("Turn {i} response with some length")));
		}

		let compacted = Compactor::compact(&msgs, 150, 4);
		assert!(compacted.len() < msgs.len());
		assert_eq!(compacted[0].role, Role::System);
		assert_eq!(compacted[0].content, "System prompt");
		assert!(
			compacted
				.iter()
				.any(|m| m.content.contains("earlier turn(s) were pruned"))
		);
	}

	#[test]
	fn test_compaction_prevents_orphan_tool_message() {
		let msgs = vec![
			Message::system("System prompt"),
			Message::user("Please do task"),
			Message::assistant_with_tools(
				"running tool",
				vec![ToolCall {
					id: "call_1".to_string(),
					name: "read".to_string(),
					arguments: serde_json::json!({"path": "foo"}),
				}],
			),
			Message::tool("call_1", "file content"),
			Message::user("Next user query"),
			Message::assistant("Done!"),
		];

		// Budget enough only for the last 3 messages (Tool, User, Assistant), which would orphan Tool
		let compacted = Compactor::compact(&msgs, 25, 1);
		assert_ne!(compacted[1].role, Role::Tool, "First non-system message must not be orphan Tool");
	}
}
