//! Streaming search over one text file a caller already admitted.

use std::path::Path;
use std::sync::Arc;

use super::super::workspace_types::compile_query_regex;
use super::super::{RepositoryMatchCase, RepositoryQueryLimits, RepositoryQueryMatch};
use super::{
    ExecutionBackendError, OperationContext, PathGlob, RepositoryLiteralMatcher,
    RepositoryQueryResult, RepositoryQueryStopReason, WorkspaceContentIdentity,
    repository_row_bytes, utf8_prefix_len,
};

/// One already-admitted text file and the matcher to run over it.
///
/// Admission is the caller's. This value does not open, resolve, or authorize
/// `path`; the path is the match row and the glob subject.
#[derive(Clone, Copy)]
pub struct AdmittedTextQuery<'a> {
    /// Path recorded on each match and tested against `glob`.
    pub path: &'a Path,
    /// `None` lists the path. Otherwise the literal, or the regex when `regex`.
    pub pattern: Option<&'a str>,
    /// Interpret `pattern` as a regular expression (one hit per line).
    pub regex: bool,
    /// Literal or regex case mode. Unused when `pattern` is `None`.
    pub match_case: RepositoryMatchCase,
    /// When set, a glob that misses `path` returns no files.
    pub glob: Option<&'a str>,
    /// Match, result and scanned-byte bounds. Tree, file-count and per-file
    /// bounds are not applied: the caller bounds what it streams.
    pub limits: RepositoryQueryLimits,
}

/// What an admitted search needs next.
pub enum AdmittedTextSearch<'a> {
    /// Answered without content: a glob listing, or a glob that misses the path.
    Done(RepositoryQueryResult),
    /// Push the file's bytes in order with [`AdmittedTextScan::push`], then
    /// [`AdmittedTextScan::finish`].
    Scan(AdmittedTextScan<'a>),
}

/// Starts a search over bytes the caller admitted (the job-output read check).
///
/// `pattern` `None` lists the path (a glob listing). Matching otherwise follows
/// the repository scanner: one regex hit per line, or every literal hit. This
/// does not admit the path; the caller did.
///
/// # Errors
///
/// Returns a cancellation, or an invalid pattern or glob.
pub fn search_admitted_text<'a>(
    control: &OperationContext,
    query: AdmittedTextQuery<'a>,
) -> Result<AdmittedTextSearch<'a>, ExecutionBackendError> {
    let AdmittedTextQuery {
        path,
        pattern,
        regex,
        match_case,
        glob,
        limits,
    } = query;
    control.check_cancelled()?;
    let matcher = match pattern {
        Some(pattern) => Some(admitted_matcher(pattern, regex, match_case)?),
        None => None,
    };
    if let Some(glob) = glob {
        let matcher = PathGlob::new(glob)?;
        if !matcher.matches(path) {
            return Ok(AdmittedTextSearch::Done(admitted_result(
                Vec::new(),
                0,
                0,
                None,
            )));
        }
    }
    let Some(matcher) = matcher else {
        return Ok(AdmittedTextSearch::Done(admitted_path_list(path)?));
    };
    Ok(AdmittedTextSearch::Scan(AdmittedTextScan {
        path,
        matcher,
        limits,
        // The stream is never held whole, so no match can name the identity
        // of the complete file. An admitted file is not an edit target.
        content: WorkspaceContentIdentity::from_bytes(&[])?,
        matches: Vec::new(),
        result_bytes: format!("matches={} incomplete=true\n", limits.max_matches).len(),
        stop_reason: None,
        lossy_lines: 0,
        line_number: 1,
        partial: Vec::new(),
        bytes_scanned: 0,
    }))
}

enum AdmittedMatcher<'a> {
    Regex(regex::bytes::Regex),
    Literal(RepositoryLiteralMatcher<'a>),
}

fn admitted_matcher(
    pattern: &str,
    regex: bool,
    match_case: RepositoryMatchCase,
) -> Result<AdmittedMatcher<'_>, ExecutionBackendError> {
    let message = if regex {
        "repository query pattern is invalid"
    } else {
        "repository query literal is invalid"
    };
    if pattern.is_empty()
        || pattern.len() > super::super::MAX_EXECUTION_VALUE_BYTES
        || pattern.contains('\0')
    {
        return Err(super::super::invalid_request(message));
    }
    if regex {
        return Ok(AdmittedMatcher::Regex(compile_query_regex(
            pattern, match_case,
        )?));
    }
    if match_case == RepositoryMatchCase::AsciiInsensitive && !pattern.is_ascii() {
        return Err(super::super::invalid_request(
            "ASCII-insensitive query must be ASCII",
        ));
    }
    Ok(AdmittedMatcher::Literal(RepositoryLiteralMatcher::new(
        pattern.as_bytes(),
        match_case,
    )))
}

/// A line-by-line scan of one admitted file, fed in slices.
///
/// A line split across slices is matched whole. A line holding invalid UTF-8
/// is matched with U+FFFD in place of the invalid bytes and counted, so one
/// stray byte neither hides a log nor drops matches already found. The scan
/// stops at the match, retained-result or scanned-byte bound.
pub struct AdmittedTextScan<'a> {
    path: &'a Path,
    matcher: AdmittedMatcher<'a>,
    limits: RepositoryQueryLimits,
    content: WorkspaceContentIdentity,
    matches: Vec<RepositoryQueryMatch>,
    result_bytes: usize,
    stop_reason: Option<RepositoryQueryStopReason>,
    lossy_lines: u64,
    line_number: u64,
    /// The unterminated tail of the bytes pushed so far.
    partial: Vec<u8>,
    bytes_scanned: u64,
}

impl AdmittedTextScan<'_> {
    /// Scans the next slice of the file. Returns `false` once the scan has
    /// stopped: later bytes cannot change the result.
    ///
    /// # Errors
    ///
    /// Returns a cancellation.
    pub fn push(
        &mut self,
        control: &OperationContext,
        slice: &[u8],
    ) -> Result<bool, ExecutionBackendError> {
        control.check_cancelled()?;
        if self.stopped() {
            return Ok(false);
        }
        let room = self
            .limits
            .max_total_bytes
            .saturating_sub(self.bytes_scanned);
        let taken = &slice[..slice.len().min(usize::try_from(room).unwrap_or(usize::MAX))];
        self.bytes_scanned = self
            .bytes_scanned
            .saturating_add(u64::try_from(taken.len()).unwrap_or(u64::MAX));
        let mut rest = taken;
        while let Some(end) = rest.iter().position(|byte| *byte == b'\n') {
            if self.partial.is_empty() {
                self.scan_line(control, &rest[..end])?;
            } else {
                let mut line = std::mem::take(&mut self.partial);
                line.extend_from_slice(&rest[..end]);
                self.scan_line(control, &line)?;
                line.clear();
                self.partial = line;
            }
            rest = &rest[end + 1..];
            if self.stopped() {
                return Ok(false);
            }
            self.line_number = self.line_number.saturating_add(1);
        }
        self.partial.extend_from_slice(rest);
        if taken.len() < slice.len() {
            self.stop_reason = Some(RepositoryQueryStopReason::ScannedBytes);
            return Ok(false);
        }
        Ok(true)
    }

    /// Scans the final, unterminated line and returns the result.
    ///
    /// # Errors
    ///
    /// Returns a cancellation.
    pub fn finish(
        mut self,
        control: &OperationContext,
    ) -> Result<RepositoryQueryResult, ExecutionBackendError> {
        if !self.stopped() {
            let line = std::mem::take(&mut self.partial);
            self.scan_line(control, &line)?;
        }
        Ok(RepositoryQueryResult {
            lossy_lines: self.lossy_lines,
            ..admitted_result(self.matches, 1, self.bytes_scanned, self.stop_reason)
        })
    }

    const fn stopped(&self) -> bool {
        self.stop_reason.is_some()
    }

    fn scan_line(
        &mut self,
        control: &OperationContext,
        line: &[u8],
    ) -> Result<(), ExecutionBackendError> {
        let replaced;
        let line = if std::str::from_utf8(line).is_ok() {
            line
        } else {
            self.lossy_lines = self.lossy_lines.saturating_add(1);
            replaced = String::from_utf8_lossy(line).into_owned();
            replaced.as_bytes()
        };
        if self.line_number.is_multiple_of(4_096) {
            control.check_cancelled()?;
        }
        match &self.matcher {
            AdmittedMatcher::Regex(regex) => {
                // A regular expression reports a line once, as grep does.
                if let Some(found) = regex.find(line) {
                    let column = found.start();
                    self.record(line, column);
                }
            }
            AdmittedMatcher::Literal(pattern) => {
                let mut hits = Vec::new();
                let mut prefix_len = 0_usize;
                for (index, byte) in line.iter().copied().enumerate() {
                    if index.is_multiple_of(65_536) {
                        control.check_cancelled()?;
                    }
                    while prefix_len > 0 && !pattern.matches(byte, prefix_len) {
                        prefix_len = pattern.failure[prefix_len - 1];
                    }
                    if pattern.matches(byte, prefix_len) {
                        prefix_len = prefix_len.saturating_add(1);
                    }
                    if prefix_len == pattern.needle.len() {
                        hits.push(index.saturating_add(1).saturating_sub(pattern.needle.len()));
                        prefix_len = 0;
                    }
                }
                for column in hits {
                    if self.stopped() {
                        break;
                    }
                    self.record(line, column);
                }
            }
        }
        Ok(())
    }

    /// Keeps one match row, or stops at the match or retained-result bound.
    fn record(&mut self, line: &[u8], column: usize) {
        let retained = utf8_prefix_len(line, line.len().min(self.limits.max_match_bytes));
        let truncated = retained < line.len();
        let row_bytes =
            repository_row_bytes(self.path, self.line_number, column, retained, truncated);
        if self.matches.len() == self.limits.max_matches {
            self.stop_reason = Some(RepositoryQueryStopReason::Matches);
            return;
        }
        if row_bytes
            > self
                .limits
                .max_result_bytes
                .saturating_sub(self.result_bytes)
        {
            self.stop_reason = Some(RepositoryQueryStopReason::ResultBytes);
            return;
        }
        self.matches.push(RepositoryQueryMatch {
            entry_size: 0,
            is_directory: false,
            path: self.path.to_path_buf(),
            line: self.line_number,
            column: u64::try_from(column.saturating_add(1)).unwrap_or(u64::MAX),
            bytes: line[..retained].to_vec().into(),
            truncated,
            content: self.content,
        });
        self.result_bytes = self.result_bytes.saturating_add(row_bytes);
    }
}

fn admitted_result(
    matches: Vec<RepositoryQueryMatch>,
    files: usize,
    bytes_scanned: u64,
    stop_reason: Option<RepositoryQueryStopReason>,
) -> RepositoryQueryResult {
    RepositoryQueryResult {
        matches: matches.into(),
        files_considered: files,
        files_scanned: files,
        bytes_scanned,
        skipped_binary: 0,
        skipped_oversize: 0,
        skipped_changed: 0,
        entries_skipped: 0,
        lossy_lines: 0,
        stop_reason,
    }
}

fn admitted_path_list(path: &Path) -> Result<RepositoryQueryResult, ExecutionBackendError> {
    let empty = WorkspaceContentIdentity::from_bytes(&[])?;
    Ok(RepositoryQueryResult {
        matches: Arc::from([RepositoryQueryMatch {
            is_directory: false,
            entry_size: 0,
            path: path.to_path_buf(),
            line: 1,
            column: 1,
            bytes: Arc::from([]),
            truncated: false,
            content: empty,
        }]),
        files_considered: 1,
        files_scanned: 0,
        bytes_scanned: 0,
        skipped_binary: 0,
        skipped_oversize: 0,
        skipped_changed: 0,
        entries_skipped: 0,
        lossy_lines: 0,
        stop_reason: None,
    })
}
