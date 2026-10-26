//! Headless `omp` runtime: argument parsing, file-backed config, prompts,
//! skills, model catalog and the agent-runner seam consumed by `pi-agent`.
//!
//! Everything mutable lives in files (config.yml, models.json, prompts,
//! skills); this crate only reads them and hands a [`agent::RunRequest`] to a
//! registered [`agent::AgentRunner`].

pub mod agent;
pub mod approve;
pub mod args;
pub mod catalog;
pub mod config;
pub mod deadline;
pub mod dirs;
pub mod foreign_session;
pub mod output;
pub mod prompts;
pub mod repl;
pub mod runtime;
pub mod session;
pub mod skills;
pub mod yaml;

use std::fmt;

/// Crate-wide error: a human-readable message plus an optional source.
#[derive(Debug)]
pub struct Error {
	message: String,
	source: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl Error {
	#[must_use]
	pub fn new(message: impl Into<String>) -> Self {
		Self { message: message.into(), source: None }
	}

	#[must_use]
	pub fn with_source(
		message: impl Into<String>,
		source: impl std::error::Error + Send + Sync + 'static,
	) -> Self {
		Self { message: message.into(), source: Some(Box::new(source)) }
	}
}

impl fmt::Display for Error {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(&self.message)?;
		if let Some(source) = &self.source {
			write!(f, ": {source}")?;
		}
		Ok(())
	}
}

impl std::error::Error for Error {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		self
			.source
			.as_deref()
			.map(|source| source as &(dyn std::error::Error + 'static))
	}
}

impl From<std::io::Error> for Error {
	fn from(err: std::io::Error) -> Self {
		Self::new(err.to_string())
	}
}

impl From<serde_json::Error> for Error {
	fn from(err: serde_json::Error) -> Self {
		Self::new(err.to_string())
	}
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

// Exit codes, matching the TS CLI: success 0, runtime/config failure 1, and the
// conventional command-line usage error 2 (clap's default, `main.ts` exit(2)).
pub const EXIT_SUCCESS: u8 = 0;
pub const EXIT_FAILURE: u8 = 1;
pub const EXIT_USAGE: u8 = 2;

/// Serialises unit tests that mutate process-wide environment variables.
#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
