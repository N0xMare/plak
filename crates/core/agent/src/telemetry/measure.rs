//! Content-free tool, model, and compaction measurements.
//!
//! A tool call is classified only by its canonical capability name and the
//! typed facts its executor produced: an [`ExecutionValue`] and a
//! [`WaitFact`]. Paths and queries leave this module only as a salted digest;
//! a process start leaves only its program label.

use std::time::Duration;

use plakous_tools::{
    ExecutionValue, LIST_ENTRIES_CAPABILITY_NAME, PROCESS_SESSION_CAPABILITY_NAME,
    PROCESS_SESSION_POLL_NAME, PROCESS_SESSION_WRITE_NAME, READ_FILE_CAPABILITY_NAME,
    SEARCH_REPOSITORY_CAPABILITY_NAME, ToolOutcome, WaitFact, WaitWake,
};

use super::TelemetryTerminalStatus;

const PROGRAM_BYTES: usize = 32;
/// Prefixes of common issued tokens and access keys.
const SECRET_PREFIXES: [&str; 12] = [
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "github_pat_",
    "glpat-",
    "sk-",
    "xox",
    "AKIA",
    "ASIA",
    "AIza",
];

/// A process start's program: a basename in `[A-Za-z0-9._+-]`, at most 32
/// bytes, not all digits, not an option and not shaped like a credential, or
/// `other`.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct ProgramBasename {
    len: u8,
    bytes: [u8; PROGRAM_BYTES],
}

impl ProgramBasename {
    /// A program basename, or `None` for anything outside the closed set.
    #[must_use]
    pub fn from_basename(name: &str) -> Option<Self> {
        let valid = !name.is_empty()
            && name.len() <= PROGRAM_BYTES
            && name.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'+' | b'-')
            })
            && !name.bytes().all(|byte| byte.is_ascii_digit())
            && !name.starts_with('-')
            && !secret_shaped(name);
        if !valid {
            return None;
        }
        let mut bytes = [0; PROGRAM_BYTES];
        bytes[..name.len()].copy_from_slice(name.as_bytes());
        Some(Self {
            len: u8::try_from(name.len()).ok()?,
            bytes,
        })
    }

    fn other() -> Self {
        let mut bytes = [0; PROGRAM_BYTES];
        bytes[..5].copy_from_slice(b"other");
        Self { len: 5, bytes }
    }

    /// The label.
    #[must_use]
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.bytes[..usize::from(self.len)]).unwrap_or("other")
    }
}

/// A known token prefix, or a run of 16 or more bytes mixing letters and digits.
fn secret_shaped(name: &str) -> bool {
    SECRET_PREFIXES
        .iter()
        .any(|prefix| name.starts_with(prefix))
        || name
            .split(|character: char| !character.is_ascii_alphanumeric())
            .any(|run| {
                run.len() >= 16
                    && run.bytes().any(|byte| byte.is_ascii_digit())
                    && run.bytes().any(|byte| byte.is_ascii_alphabetic())
            })
}

impl std::fmt::Debug for ProgramBasename {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.as_str().fmt(formatter)
    }
}

/// What a wait watched. Closed, never an identifier.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum WaitTargetKind {
    /// At least one child agent.
    Agent,
    /// Processes only.
    Process,
}

impl WaitTargetKind {
    /// Stable analytics label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Process => "process",
        }
    }
}

/// How a wait ended. Closed, never a message body.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum WaitOutcome {
    /// Every waited-on target reached a terminal state.
    Completed,
    /// The deadline passed with a target still running.
    Timeout,
    /// Mail for the waiter ended the wait.
    Mail,
    /// Steering or a notice cut the wait short.
    Interrupted,
    /// The call was cancelled.
    Cancelled,
}

impl WaitOutcome {
    /// Stable analytics label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Timeout => "timeout",
            Self::Mail => "mail",
            Self::Interrupted => "interrupted",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Facts one finished tool call contributes. Absent fields are not that class.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct ToolMeasurement {
    /// Per-driver salted digest of the normalized path or query. Never the text.
    pub access_digest: Option<[u8; 32]>,
    /// First line of the page a read returned.
    pub range_first_line: Option<u64>,
    /// Lines in that page.
    pub range_lines: Option<u64>,
    /// The result is part of more: a read short of the end of the file or with
    /// lines cut, or a search or listing that cut results (in its text or its
    /// typed value) or stopped its walk.
    pub page_partial: Option<bool>,
    /// A process start's program.
    pub command_class: Option<ProgramBasename>,
    /// Targets a wait watched.
    pub wait_count: Option<u32>,
    /// What a wait watched.
    pub wait_kind: Option<WaitTargetKind>,
    /// How a wait ended.
    pub wait_outcome: Option<WaitOutcome>,
}

impl ToolMeasurement {
    /// No measurement: a tool outside the read, search, list, process and wait classes.
    #[must_use]
    pub const fn absent() -> Self {
        Self {
            access_digest: None,
            range_first_line: None,
            range_lines: None,
            page_partial: None,
            command_class: None,
            wait_count: None,
            wait_kind: None,
            wait_outcome: None,
        }
    }

    /// A read (with its page's first line and line count), search or listing.
    #[must_use]
    pub const fn access(
        access_digest: Option<[u8; 32]>,
        range: Option<(u64, u64)>,
        page_partial: bool,
    ) -> Self {
        let (range_first_line, range_lines) = match range {
            Some((first, lines)) => (Some(first), Some(lines)),
            None => (None, None),
        };
        Self {
            access_digest,
            range_first_line,
            range_lines,
            page_partial: Some(page_partial),
            ..Self::absent()
        }
    }

    /// A process start.
    #[must_use]
    pub const fn command(class: ProgramBasename) -> Self {
        Self {
            command_class: Some(class),
            ..Self::absent()
        }
    }

    /// A wait.
    #[must_use]
    pub const fn wait(count: u32, kind: WaitTargetKind, outcome: WaitOutcome) -> Self {
        Self {
            wait_count: Some(count),
            wait_kind: Some(kind),
            wait_outcome: Some(outcome),
            ..Self::absent()
        }
    }
}

/// Route window, prepared input, first-attempt footprint, and idle gap of one
/// model attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct ModelMeasurement {
    /// The route descriptor's context window, in tokens.
    pub context_window_tokens: Option<u64>,
    /// Prepared-input upper bound of this attempt.
    pub prepared_input_tokens: Option<u64>,
    /// UTF-8 bytes of the stable instruction text. Only the turn's first attempt.
    pub instruction_bytes: Option<u64>,
    /// Serialized configured-tool input schema bytes. Only the turn's first attempt.
    pub tool_schema_bytes: Option<u64>,
    /// Time since this driver's previous provider request, taken once before
    /// any retry. Absent for the driver's first request.
    pub idle_gap: Option<Duration>,
}

impl ModelMeasurement {
    /// No measurement.
    #[must_use]
    pub const fn absent() -> Self {
        Self::new(None, None, None, None)
    }

    /// One attempt's measurement. `footprint` is the instruction and tool
    /// schema bytes, present only on a turn's first attempt.
    #[must_use]
    pub const fn new(
        context_window_tokens: Option<u64>,
        prepared_input_tokens: Option<u64>,
        footprint: Option<(u64, u64)>,
        idle_gap: Option<Duration>,
    ) -> Self {
        let (instruction_bytes, tool_schema_bytes) = match footprint {
            Some((instructions, tools)) => (Some(instructions), Some(tools)),
            None => (None, None),
        };
        Self {
            context_window_tokens,
            prepared_input_tokens,
            instruction_bytes,
            tool_schema_bytes,
            idle_gap,
        }
    }
}

/// Window, trigger threshold, occupancy, and idle gap of one compaction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct CompactionMeasurement {
    /// The route descriptor's context window, in tokens.
    pub context_window_tokens: Option<u64>,
    /// Compaction trigger of the context status that decided this compaction.
    pub threshold_tokens: Option<u64>,
    /// Occupancy of that status.
    pub occupancy_tokens: Option<u64>,
    /// Time since this driver's previous provider request when the paid call
    /// dispatched, taken once before any retry. Absent without a paid call or
    /// for the driver's first request.
    pub idle_gap: Option<Duration>,
}

impl CompactionMeasurement {
    /// No measurement.
    #[must_use]
    pub const fn absent() -> Self {
        Self::new(None, None, None, None)
    }

    /// One compaction's measurement.
    #[must_use]
    pub const fn new(
        context_window_tokens: Option<u64>,
        threshold_tokens: Option<u64>,
        occupancy_tokens: Option<u64>,
        idle_gap: Option<Duration>,
    ) -> Self {
        Self {
            context_window_tokens,
            threshold_tokens,
            occupancy_tokens,
            idle_gap,
        }
    }
}

/// Whether a telemetry-enabled driver asks this canonical capability's
/// executor for its typed value.
pub(crate) fn measures_capability(canonical: &str) -> bool {
    matches!(
        canonical,
        READ_FILE_CAPABILITY_NAME
            | SEARCH_REPOSITORY_CAPABILITY_NAME
            | LIST_ENTRIES_CAPABILITY_NAME
            | PROCESS_SESSION_CAPABILITY_NAME
            | PROCESS_SESSION_POLL_NAME
            | PROCESS_SESSION_WRITE_NAME
    )
}

/// Classifies one finished call from its typed facts.
pub(crate) fn measure_tool_call(
    salt: Option<&[u8; 32]>,
    canonical: &str,
    status: TelemetryTerminalStatus,
    value: Option<&ExecutionValue>,
    outcome: Option<&ToolOutcome>,
) -> ToolMeasurement {
    if let Some(fact) = outcome.and_then(ToolOutcome::wait_fact) {
        return wait_measurement(fact, status);
    }
    let process_wait = matches!(
        canonical,
        PROCESS_SESSION_POLL_NAME | PROCESS_SESSION_WRITE_NAME
    );
    if process_wait && status == TelemetryTerminalStatus::Cancelled {
        return ToolMeasurement::wait(1, WaitTargetKind::Process, WaitOutcome::Cancelled);
    }
    let digest = |text: &str| salt.map(|salt| access_digest(salt, &normalize_identity(text)));
    match value {
        Some(ExecutionValue::FileRead(read)) if canonical == READ_FILE_CAPABILITY_NAME => {
            ToolMeasurement::access(
                digest(&read.path),
                Some((read.first_line, read.lines)),
                !read.eof || read.lines_cut > 0 || read.clipped_lines > 0,
            )
        }
        Some(ExecutionValue::SearchHits(search)) => ToolMeasurement::access(
            digest(&search.query),
            None,
            search.hits_cut > 0 || search.incomplete,
        ),
        Some(ExecutionValue::Listing(listing)) => ToolMeasurement::access(
            digest(&listing.path),
            None,
            listing.entries_cut > 0 || listing.incomplete,
        ),
        Some(ExecutionValue::Process(process)) if canonical == PROCESS_SESSION_CAPABILITY_NAME => {
            ToolMeasurement::command(
                process
                    .program
                    .as_deref()
                    .and_then(ProgramBasename::from_basename)
                    .unwrap_or_else(ProgramBasename::other),
            )
        }
        // A poll or write that returns while its process still runs, or
        // leaves processes behind, ended on its deadline; anything else
        // settled, including a process its own timer killed.
        Some(ExecutionValue::Process(process)) if process_wait => {
            let outcome = if matches!(process.status.as_str(), "running" | "leftover") {
                WaitOutcome::Timeout
            } else {
                WaitOutcome::Completed
            };
            ToolMeasurement::wait(1, WaitTargetKind::Process, outcome)
        }
        _ => ToolMeasurement::absent(),
    }
}

fn wait_measurement(fact: WaitFact, status: TelemetryTerminalStatus) -> ToolMeasurement {
    let kind = if fact.agents > 0 {
        WaitTargetKind::Agent
    } else {
        WaitTargetKind::Process
    };
    let outcome = if status == TelemetryTerminalStatus::Cancelled {
        WaitOutcome::Cancelled
    } else {
        match fact.wake {
            WaitWake::Settled => WaitOutcome::Completed,
            WaitWake::Mail => WaitOutcome::Mail,
            WaitWake::Timeout => WaitOutcome::Timeout,
            WaitWake::Interrupted => WaitOutcome::Interrupted,
            // A wake this build does not know is not guessed at.
            _ => return ToolMeasurement::absent(),
        }
    };
    ToolMeasurement::wait(fact.agents.saturating_add(fact.processes), kind, outcome)
}

/// Lexical identity: backslashes become slashes, and empty and `.` components
/// drop, so `./a/b/`, `a//b` and `a\b` are all `a/b`.
fn normalize_identity(text: &str) -> String {
    let text = text.trim().replace('\\', "/");
    let joined = text
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect::<Vec<_>>()
        .join("/");
    if text.starts_with('/') {
        format!("/{joined}")
    } else if joined.is_empty() {
        ".".to_owned()
    } else {
        joined
    }
}

fn access_digest(salt: &[u8; 32], text: &str) -> [u8; 32] {
    plakous_model::sha256_bytes(&[salt, &[0], text.as_bytes()])
}

#[cfg(test)]
mod tests;
