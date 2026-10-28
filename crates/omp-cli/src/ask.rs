//! Terminal `ask` surface.
//!
//! The CLI backend for `pi_agent::tools::ask::AskChannel`: numbered line input
//! on the terminal, recommended defaults, multi-select, custom "Other" answers,
//! optional notes, chat redirect, timeout auto-selection and cancellation.
//!
//! Input is polled in short slices from a single blocking task, so a timeout or
//! turn cancellation returns within one slice and never leaves a detached stdin
//! reader behind to steal the next REPL prompt. It is only installed for an
//! interactive REPL; headless runs and piped prompts never reach it.

use std::{
	collections::BTreeSet,
	io::Write,
	sync::Arc,
	time::{Duration, Instant},
};

use async_trait::async_trait;
use pi_agent::tools::ask::{
	AskAnswer, AskChannel, AskOutcome, AskQuestion, CHAT_ABOUT_THIS_OPTION, OTHER_OPTION,
	auto_selection, display_label,
};
use tokio_util::sync::CancellationToken;

const POLL_SLICE_MS: i32 = 50;
const KEYWORDS_BACK: [&str; 2] = ["b", "back"];
const KEYWORDS_CANCEL: [&str; 5] = ["q", "quit", "cancel", "esc", "escape"];
const KEYWORDS_CHAT: [&str; 2] = ["c", "chat"];
const KEYWORDS_OTHER: [&str; 2] = ["o", "other"];
const KEYWORDS_DONE: [&str; 2] = ["d", "done"];

/// Parsed single-select input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SingleChoice {
	/// Empty line: keep the current selection, else the recommended/first option.
	Default,
	Index(usize),
	Other,
	Note(Option<String>),
	Chat,
	Back,
	Cancel,
	Invalid,
}

/// Parsed multi-select input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MultiChoice {
	/// Empty line or `d`/`done`.
	Done,
	/// Zero-based option indices to toggle.
	Toggle(Vec<usize>),
	Other,
	Note(Option<String>),
	Chat,
	Back,
	Cancel,
	Invalid,
}

fn keyword(line: &str) -> (String, Option<String>) {
	let trimmed = line.trim();
	match trimmed.split_once(char::is_whitespace) {
		Some((word, rest)) => (word.to_ascii_lowercase(), Some(rest.trim().to_string())),
		None => (trimmed.to_ascii_lowercase(), None),
	}
}

fn parse_note(line: &str) -> Option<Option<String>> {
	let (word, rest) = keyword(line);
	if word != "n" && word != "note" {
		return None;
	}
	Some(rest.filter(|text| !text.is_empty()))
}

fn parse_indices(line: &str, option_count: usize) -> Option<Vec<usize>> {
	let mut indices = Vec::new();
	for token in line.split([',', ' ', '\t']).filter(|token| !token.is_empty()) {
		let parsed: usize = token.parse().ok()?;
		if parsed == 0 || parsed > option_count {
			return None;
		}
		let index = parsed - 1;
		if !indices.contains(&index) {
			indices.push(index);
		}
	}
	(!indices.is_empty()).then_some(indices)
}

/// Parse one single-select answer line. Numbers are 1-based as displayed.
#[must_use]
pub fn parse_single_choice(line: &str, option_count: usize) -> SingleChoice {
	let trimmed = line.trim();
	if trimmed.is_empty() {
		return SingleChoice::Default;
	}
	if let Some(note) = parse_note(trimmed) {
		return SingleChoice::Note(note);
	}
	let (word, _) = keyword(trimmed);
	if KEYWORDS_CANCEL.contains(&word.as_str()) {
		return SingleChoice::Cancel;
	}
	if KEYWORDS_BACK.contains(&word.as_str()) {
		return SingleChoice::Back;
	}
	if KEYWORDS_CHAT.contains(&word.as_str()) {
		return SingleChoice::Chat;
	}
	if KEYWORDS_OTHER.contains(&word.as_str()) {
		return SingleChoice::Other;
	}
	match trimmed.parse::<usize>() {
		Ok(value) if value >= 1 && value <= option_count => SingleChoice::Index(value - 1),
		_ => SingleChoice::Invalid,
	}
}

/// Parse one multi-select answer line.
#[must_use]
pub fn parse_multi_choice(line: &str, option_count: usize) -> MultiChoice {
	let trimmed = line.trim();
	if trimmed.is_empty() {
		return MultiChoice::Done;
	}
	if let Some(note) = parse_note(trimmed) {
		return MultiChoice::Note(note);
	}
	let (word, _) = keyword(trimmed);
	if KEYWORDS_CANCEL.contains(&word.as_str()) {
		return MultiChoice::Cancel;
	}
	if KEYWORDS_BACK.contains(&word.as_str()) {
		return MultiChoice::Back;
	}
	if KEYWORDS_CHAT.contains(&word.as_str()) {
		return MultiChoice::Chat;
	}
	if KEYWORDS_OTHER.contains(&word.as_str()) {
		return MultiChoice::Other;
	}
	if KEYWORDS_DONE.contains(&word.as_str()) {
		return MultiChoice::Done;
	}
	match parse_indices(trimmed, option_count) {
		Some(indices) => MultiChoice::Toggle(indices),
		None => MultiChoice::Invalid,
	}
}

fn flatten(text: &str) -> String {
	text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn effective_single_index(
	question: &AskQuestion,
	selected: &BTreeSet<usize>,
) -> Option<usize> {
	if let Some(index) = selected.iter().next() {
		return Some(*index);
	}
	if question.options.is_empty() {
		return None;
	}
	Some(
		question
			.recommended
			.filter(|value| *value >= 0 && (*value as usize) < question.options.len())
			.map_or(0, |value| value as usize),
	)
}

/// Render the question block. Pure so option/description/preview formatting is
/// testable without a terminal.
#[must_use]
pub fn render_question(
	question: &AskQuestion,
	position: usize,
	total: usize,
	selected: &BTreeSet<usize>,
	custom_input: Option<&str>,
	note: Option<&str>,
) -> Vec<String> {
	let mut lines = Vec::new();
	let progress = if total > 1 {
		format!(" ({}/{})", position + 1, total)
	} else {
		String::new()
	};
	let title = format!("{}{progress}", question.question);
	match &question.header {
		Some(header) => lines.push(format!("[{}] {title}", header.trim())),
		None => lines.push(title),
	}
	let cursor = effective_single_index(question, selected);
	for (index, option) in question.options.iter().enumerate() {
		let label = display_label(option, index, question.recommended);
		let marker = if question.multi {
			if selected.contains(&index) { "[x]" } else { "[ ]" }
		} else if cursor == Some(index) {
			">"
		} else {
			" "
		};
		lines.push(format!("  {marker} {}) {label}", index + 1));
		if let Some(description) = &option.description {
			let flat = flatten(description);
			if !flat.is_empty() {
				lines.push(format!("        {flat}"));
			}
		}
		if let Some(preview) = &option.preview {
			for line in preview.lines() {
				lines.push(format!("        {line}"));
			}
		}
	}
	lines.push(format!("  o) {OTHER_OPTION}"));
	lines.push(format!("  c) {CHAT_ABOUT_THIS_OPTION}"));
	if question.multi {
		lines.push("  Enter numbers to toggle, d to finish, o custom, n note, c chat, b back, q cancel.".to_string());
	} else {
		lines.push("  Enter a number, o custom, n note, c chat, b back, q cancel.".to_string());
	}
	if let Some(custom) = custom_input {
		lines.push(format!("  custom: {custom}"));
	}
	if let Some(note) = note {
		lines.push(format!("  note: {note}"));
	}
	lines
}

/// Terminal implementation of [`AskChannel`].
#[derive(Debug, Clone)]
pub struct TerminalAskChannel {
	timeout: Option<Duration>,
}

impl TerminalAskChannel {
	#[must_use]
	pub fn new(timeout: Option<Duration>) -> Self {
		Self { timeout }
	}

	async fn ask_question(
		&self,
		question: &AskQuestion,
		position: usize,
		total: usize,
		previous: Option<AskAnswer>,
		cancel: &CancellationToken,
	) -> QuestionFlow {
		let mut selected: BTreeSet<usize> = BTreeSet::new();
		if let Some(previous) = &previous {
			for label in &previous.selected_options {
				if let Some(index) = question.options.iter().position(|option| &option.label == label) {
					selected.insert(index);
				}
			}
		}
		let mut custom_input = previous.as_ref().and_then(|answer| answer.custom_input.clone());
		let mut note = previous.as_ref().and_then(|answer| answer.note.clone());
		let allow_back = position > 0;

		loop {
			for line in render_question(
				question,
				position,
				total,
				&selected,
				custom_input.as_deref(),
				note.as_deref(),
			) {
				eprintln!("{line}");
			}
			let event = read_input(cancel, self.timeout).await;
			let line = match event {
				InputEvent::Line(line) => line,
				InputEvent::Timeout => {
					eprintln!();
					return QuestionFlow::Answer(timeout_answer(
						question,
						&selected,
						custom_input,
						note,
					));
				},
				InputEvent::Cancelled => return QuestionFlow::Cancelled,
				InputEvent::Aborted => return QuestionFlow::Aborted,
			};

			if question.multi {
				match parse_multi_choice(&line, question.options.len()) {
					MultiChoice::Done => {
						return QuestionFlow::Answer(build_answer(
							question,
							&selected,
							custom_input,
							note,
							false,
						));
					},
					MultiChoice::Toggle(indices) => {
						for index in indices {
							if !selected.remove(&index) {
								selected.insert(index);
							}
						}
					},
					MultiChoice::Other => {
						match self.read_custom(cancel).await {
							CustomFlow::Input(input) => {
								custom_input = input;
								return QuestionFlow::Answer(build_answer(
									question,
									&selected,
									custom_input,
									note,
									false,
								));
							},
							CustomFlow::Cancelled => return QuestionFlow::Cancelled,
							CustomFlow::Aborted => return QuestionFlow::Aborted,
						}
					},
					MultiChoice::Note(value) => note = value,
					MultiChoice::Chat => return QuestionFlow::Chat,
					MultiChoice::Back if allow_back => return QuestionFlow::Back,
					MultiChoice::Back => eprintln!("Already at the first question."),
					MultiChoice::Cancel => return QuestionFlow::Cancelled,
					MultiChoice::Invalid => invalid_choice(question.options.len()),
				}
				continue;
			}

			match parse_single_choice(&line, question.options.len()) {
				SingleChoice::Default => {
					if let Some(previous) = previous.clone()
						&& selected.is_empty()
						&& custom_input.is_none()
					{
						return QuestionFlow::Answer(previous);
					}
					match effective_single_index(question, &selected) {
						Some(index) => {
							selected.clear();
							selected.insert(index);
							return QuestionFlow::Answer(build_answer(
								question,
								&selected,
								custom_input,
								note,
								false,
							));
						},
						None => invalid_choice(question.options.len()),
					}
				},
				SingleChoice::Index(index) => {
					selected.clear();
					selected.insert(index);
					custom_input = None;
					return QuestionFlow::Answer(build_answer(
						question,
						&selected,
						custom_input,
						note,
						false,
					));
				},
				SingleChoice::Other => match self.read_custom(cancel).await {
					CustomFlow::Input(input) => {
						selected.clear();
						custom_input = input;
						return QuestionFlow::Answer(build_answer(
							question,
							&selected,
							custom_input,
							note,
							false,
						));
					},
					CustomFlow::Cancelled => return QuestionFlow::Cancelled,
					CustomFlow::Aborted => return QuestionFlow::Aborted,
				},
				SingleChoice::Note(value) => note = value,
				SingleChoice::Chat => return QuestionFlow::Chat,
				SingleChoice::Back if allow_back => return QuestionFlow::Back,
				SingleChoice::Back => eprintln!("Already at the first question."),
				SingleChoice::Cancel => return QuestionFlow::Cancelled,
				SingleChoice::Invalid => invalid_choice(question.options.len()),
			}
		}
	}

	async fn read_custom(&self, cancel: &CancellationToken) -> CustomFlow {
		eprint!("  Enter your response: ");
		let _ = std::io::stderr().flush();
		match read_input(cancel, None).await {
			InputEvent::Line(line) => CustomFlow::Input(Some(line)),
			InputEvent::Timeout | InputEvent::Cancelled => CustomFlow::Cancelled,
			InputEvent::Aborted => CustomFlow::Aborted,
		}
	}
}

#[async_trait]
impl AskChannel for TerminalAskChannel {
	async fn ask(&self, questions: &[AskQuestion], cancel: CancellationToken) -> AskOutcome {
		let mut answers: Vec<Option<AskAnswer>> = vec![None; questions.len()];
		let mut index = 0;
		while index < questions.len() {
			let previous = answers[index].clone();
			let flow = self
				.ask_question(&questions[index], index, questions.len(), previous, &cancel)
				.await;
			match flow {
				QuestionFlow::Answer(answer) => {
					answers[index] = Some(answer);
					index += 1;
				},
				QuestionFlow::Back => index = index.saturating_sub(1),
				QuestionFlow::Chat => return AskOutcome::ChatRedirect,
				QuestionFlow::Cancelled => return AskOutcome::Cancelled,
				QuestionFlow::Aborted => return AskOutcome::Aborted,
			}
		}
		AskOutcome::Answered(answers.into_iter().flatten().collect())
	}
}

enum QuestionFlow {
	Answer(AskAnswer),
	Back,
	Chat,
	Cancelled,
	Aborted,
}

enum CustomFlow {
	Input(Option<String>),
	Cancelled,
	Aborted,
}

fn invalid_choice(count: usize) {
	if count == 0 {
		eprintln!("No options. Enter o for custom input or q to cancel.");
	} else if count == 1 {
		eprintln!("Invalid choice. Enter 1, o, c or q.");
	} else {
		eprintln!("Invalid choice. Enter 1-{count}, o, c or q.");
	}
}

fn build_answer(
	question: &AskQuestion,
	selected: &BTreeSet<usize>,
	custom_input: Option<String>,
	note: Option<String>,
	timed_out: bool,
) -> AskAnswer {
	let keep_selection = custom_input.is_none() || question.multi;
	let selected_options = if keep_selection {
		selected
			.iter()
			.filter_map(|index| question.options.get(*index))
			.map(|option| option.label.clone())
			.collect()
	} else {
		Vec::new()
	};
	AskAnswer {
		id: question.id.clone(),
		selected_options,
		custom_input,
		note,
		timed_out,
	}
}

fn timeout_answer(
	question: &AskQuestion,
	selected: &BTreeSet<usize>,
	custom_input: Option<String>,
	note: Option<String>,
) -> AskAnswer {
	let mut answer = build_answer(question, selected, custom_input, note, true);
	if answer.selected_options.is_empty() && answer.custom_input.is_none() {
		answer.selected_options = auto_selection(&question.options, question.recommended);
	}
	answer
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum InputEvent {
	Line(String),
	Timeout,
	Cancelled,
	Aborted,
}

async fn read_input(cancel: &CancellationToken, timeout: Option<Duration>) -> InputEvent {
	let cancel = cancel.clone();
	match tokio::task::spawn_blocking(move || read_input_blocking(cancel, timeout)).await {
		Ok(event) => event,
		Err(_) => InputEvent::Aborted,
	}
}

/// Poll stdin in bounded slices. Never issues a read that outlives the timeout
/// or cancellation check. The deadline applies only before the first byte: an
/// explicit submission is finished even if it crosses the deadline, so a
/// complete line and its UTF-8 codepoints are never split. A timeout or
/// cancellation discards any unread terminal input, so an abandoned partial
/// answer cannot become the next ask/approval/REPL input.
#[cfg(unix)]
fn read_input_blocking(cancel: CancellationToken, timeout: Option<Duration>) -> InputEvent {
	let deadline = timeout.map(|duration| Instant::now() + duration);
	let mut bytes = Vec::new();
	loop {
		if cancel.is_cancelled() {
			crate::runtime::flush_tty_stdin();
			return InputEvent::Aborted;
		}
		let wait = if bytes.is_empty() {
			match deadline {
				Some(deadline) => {
					let now = Instant::now();
					if now >= deadline {
						crate::runtime::flush_tty_stdin();
						return InputEvent::Timeout;
					}
					(deadline - now).as_millis().clamp(1, POLL_SLICE_MS as u128) as i32
				},
				None => POLL_SLICE_MS,
			}
		} else {
			POLL_SLICE_MS
		};
		if !crate::runtime::poll_stdin_readable(wait) {
			continue;
		}
		match crate::runtime::read_stdin_byte() {
			None => {
				crate::runtime::flush_tty_stdin();
				return InputEvent::Cancelled;
			},
			Some(b'\n') => return InputEvent::Line(String::from_utf8_lossy(&bytes).into_owned()),
			Some(b'\r') => {},
			Some(0x03) => {
				crate::runtime::flush_tty_stdin();
				return InputEvent::Cancelled;
			},
			Some(byte) => bytes.push(byte),
		}
	}
}

#[cfg(not(unix))]
fn read_input_blocking(cancel: CancellationToken, _timeout: Option<Duration>) -> InputEvent {
	use std::io::BufRead;
	if cancel.is_cancelled() {
		return InputEvent::Aborted;
	}
	let mut line = String::new();
	match std::io::stdin().lock().read_line(&mut line) {
		Ok(0) => InputEvent::Cancelled,
		Ok(_) => InputEvent::Line(line.trim_end_matches(['\r', '\n']).to_string()),
		Err(_) => InputEvent::Cancelled,
	}
}

/// Install the terminal channel for an interactive REPL.
#[must_use]
pub fn terminal_channel(timeout: Option<Duration>) -> Arc<dyn AskChannel> {
	Arc::new(TerminalAskChannel::new(timeout))
}

#[cfg(test)]
mod tests {
	use std::collections::BTreeSet;

	use pi_agent::tools::ask::{AskOption, RECOMMENDED_SUFFIX};

	use super::*;

	fn option(label: &str) -> AskOption {
		AskOption { label: label.to_string(), ..AskOption::default() }
	}

	fn question() -> AskQuestion {
		AskQuestion {
			id: "q".into(),
			question: "Pick".into(),
			header: None,
			options: vec![option("A"), option("B")],
			multi: false,
			recommended: Some(1),
		}
	}

	#[test]
	fn single_choice_boundaries() {
		assert_eq!(parse_single_choice("", 2), SingleChoice::Default);
		assert_eq!(parse_single_choice("  1 ", 2), SingleChoice::Index(0));
		assert_eq!(parse_single_choice("2", 2), SingleChoice::Index(1));
		assert_eq!(parse_single_choice("0", 2), SingleChoice::Invalid);
		assert_eq!(parse_single_choice("3", 2), SingleChoice::Invalid);
		assert_eq!(parse_single_choice("-1", 2), SingleChoice::Invalid);
		assert_eq!(parse_single_choice("1,2", 2), SingleChoice::Invalid);
		assert_eq!(parse_single_choice("o", 2), SingleChoice::Other);
		assert_eq!(parse_single_choice("c", 2), SingleChoice::Chat);
		assert_eq!(parse_single_choice("q", 2), SingleChoice::Cancel);
		assert_eq!(parse_single_choice("esc", 2), SingleChoice::Cancel);
		assert_eq!(parse_single_choice("b", 2), SingleChoice::Back);
		assert_eq!(parse_single_choice("n why", 2), SingleChoice::Note(Some("why".into())));
		assert_eq!(parse_single_choice("note", 2), SingleChoice::Note(None));
	}

	#[test]
	fn multi_choice_boundaries() {
		assert_eq!(parse_multi_choice("", 3), MultiChoice::Done);
		assert_eq!(parse_multi_choice("d", 3), MultiChoice::Done);
		assert_eq!(parse_multi_choice("done", 3), MultiChoice::Done);
		assert_eq!(parse_multi_choice("1,3", 3), MultiChoice::Toggle(vec![0, 2]));
		assert_eq!(parse_multi_choice("1 1 2", 3), MultiChoice::Toggle(vec![0, 1]));
		assert_eq!(parse_multi_choice("4", 3), MultiChoice::Invalid);
		assert_eq!(parse_multi_choice("0", 3), MultiChoice::Invalid);
		assert_eq!(parse_multi_choice("1,x", 3), MultiChoice::Invalid);
		assert_eq!(parse_multi_choice("o", 3), MultiChoice::Other);
		assert_eq!(parse_multi_choice("n note text", 3), MultiChoice::Note(Some("note text".into())));
		assert_eq!(parse_multi_choice("c", 3), MultiChoice::Chat);
		assert_eq!(parse_multi_choice("b", 3), MultiChoice::Back);
		assert_eq!(parse_multi_choice("q", 3), MultiChoice::Cancel);
	}

	#[test]
	fn render_includes_descriptions_previews_and_default_marker() {
		let mut question = question();
		question.header = Some(" chip ".into());
		question.options[0].description = Some(" first\n option ".into());
		question.options[1].preview = Some("preview line".into());
		let lines = render_question(&question, 0, 2, &BTreeSet::new(), None, None);
		let text = lines.join("\n");
		assert!(text.contains("[chip] Pick (1/2)"), "{text}");
		assert!(text.contains("1) A"), "{text}");
		assert!(text.contains("first option"), "{text}");
		assert!(text.contains("2) B (Recommended)"), "{text}");
		assert!(text.contains("preview line"), "{text}");
		assert!(text.contains("> 2)"), "{text}");
		assert!(text.contains(OTHER_OPTION), "{text}");
		assert!(text.contains(CHAT_ABOUT_THIS_OPTION), "{text}");

		question.recommended = Some(9);
		let lines = render_question(&question, 0, 1, &BTreeSet::from([0]), None, None);
		let text = lines.join("\n");
		assert!(!text.contains(RECOMMENDED_SUFFIX), "{text}");
		assert!(text.contains("> 1) A"), "{text}");
	}

	#[test]
	fn render_shows_checkboxes_and_notes() {
		let mut question = question();
		question.multi = true;
		let lines = render_question(
			&question,
			1,
			3,
			&BTreeSet::from([0]),
			Some("free"),
			Some("note"),
		);
		let text = lines.join("\n");
		assert!(text.contains("[x] 1) A"), "{text}");
		assert!(text.contains("[ ] 2) B (Recommended)"), "{text}");
		assert!(text.contains("custom: free"), "{text}");
		assert!(text.contains("note: note"), "{text}");
		assert!(text.contains("(2/3)"), "{text}");
	}

	#[test]
	fn timeout_answer_uses_recommended_then_first_and_keeps_prior_selection() {
		let question = question();
		let answer = timeout_answer(&question, &BTreeSet::new(), None, None);
		assert_eq!(answer.selected_options, vec!["B".to_string()]);
		assert!(answer.timed_out);

		let mut invalid = question.clone();
		invalid.recommended = Some(9);
		let answer = timeout_answer(&invalid, &BTreeSet::new(), None, None);
		assert_eq!(answer.selected_options, vec!["A".to_string()]);

		let answer = timeout_answer(&question, &BTreeSet::from([0]), None, None);
		assert_eq!(answer.selected_options, vec!["A".to_string()]);

		let custom = timeout_answer(&question, &BTreeSet::new(), Some("free".into()), None);
		assert!(custom.selected_options.is_empty());
		assert_eq!(custom.custom_input.as_deref(), Some("free"));
	}

	#[test]
	fn custom_input_replaces_single_selection_but_not_multi() {
		let question = question();
		let single = build_answer(&question, &BTreeSet::from([0]), Some("x".into()), None, false);
		assert!(single.selected_options.is_empty());
		assert_eq!(single.custom_input.as_deref(), Some("x"));

		let mut multi = question;
		multi.multi = true;
		let answer = build_answer(&multi, &BTreeSet::from([0]), Some("x".into()), None, false);
		assert_eq!(answer.selected_options, vec!["A".to_string()]);
		assert_eq!(answer.custom_input.as_deref(), Some("x"));
	}

	#[test]
	fn channel_is_constructible_without_touching_stdin() {
		let channel: Arc<dyn AskChannel> = terminal_channel(Some(Duration::from_secs(1)));
		assert!(Arc::strong_count(&channel) >= 1);
	}
}
