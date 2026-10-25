//! `omp repl` line grammar.
//!
//! A line is a turn unless it starts with `/`: `/exit`, `/quit`, `/model`,
//! `/cwd`.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplCommand {
	Exit,
	/// `/model <id>` or bare `/model` (show current).
	Model(Option<String>),
	/// `/cwd <path>` or bare `/cwd` (show current).
	Cwd(Option<String>),
	Prompt(String),
}

#[must_use]
pub fn parse(line: &str) -> Option<ReplCommand> {
	let trimmed = line.trim();
	if trimmed.is_empty() {
		return None;
	}
	if !trimmed.starts_with('/') {
		return Some(ReplCommand::Prompt(trimmed.to_string()));
	}
	let (command, argument) = match trimmed.split_once(char::is_whitespace) {
		Some((command, argument)) => (command, argument.trim()),
		None => (trimmed, ""),
	};
	let argument = (!argument.is_empty()).then(|| argument.to_string());
	match command {
		"/exit" | "/quit" => Some(ReplCommand::Exit),
		"/model" => Some(ReplCommand::Model(argument)),
		"/cwd" => Some(ReplCommand::Cwd(argument)),
		_ => Some(ReplCommand::Prompt(trimmed.to_string())),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn classifies_commands_and_prompts() {
		assert_eq!(parse("  "), None);
		assert_eq!(parse("/exit"), Some(ReplCommand::Exit));
		assert_eq!(parse("/quit"), Some(ReplCommand::Exit));
		assert_eq!(parse("/model gpt-5"), Some(ReplCommand::Model(Some("gpt-5".into()))));
		assert_eq!(parse("/model"), Some(ReplCommand::Model(None)));
		assert_eq!(parse("/cwd /tmp"), Some(ReplCommand::Cwd(Some("/tmp".into()))));
		assert_eq!(parse("fix the tests"), Some(ReplCommand::Prompt("fix the tests".into())));
		assert_eq!(
			parse("/unknown stays a prompt"),
			Some(ReplCommand::Prompt("/unknown stays a prompt".into()))
		);
	}
}
