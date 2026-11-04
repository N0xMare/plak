//! Portable literal, glob and row-size primitives of repository search.

use std::path::Path;

use super::{ExecutionBackendError, ExecutionBackendErrorKind, RepositoryMatchCase, backend_error};

mod admitted;

pub use admitted::{AdmittedTextQuery, AdmittedTextScan, AdmittedTextSearch, search_admitted_text};

/// One compiled path glob. A pattern without `/` matches the file name at any
/// depth, as ripgrep's `-g` does; a pattern with `/` matches the path relative
/// to the query root. `*` and `?` stay within one component, `**` crosses
/// components, and `[...]` classes and `{a,b}` alternatives are supported.
pub(super) struct PathGlob {
    matcher: globset::GlobMatcher,
    name_only: bool,
}

impl PathGlob {
    pub(super) fn new(pattern: &str) -> Result<Self, ExecutionBackendError> {
        let name_only = !pattern.contains('/');
        let pattern = pattern.strip_prefix("./").unwrap_or(pattern);
        let matcher = globset::GlobBuilder::new(pattern)
            .literal_separator(true)
            .build()
            .map_err(|_| {
                backend_error(
                    ExecutionBackendErrorKind::InvalidRequest,
                    "repository glob pattern is invalid",
                )
            })?
            .compile_matcher();
        Ok(Self { matcher, name_only })
    }

    pub(super) fn matches(&self, path: &Path) -> bool {
        if self.name_only {
            return path
                .file_name()
                .is_some_and(|name| self.matcher.is_match(Path::new(name)));
        }
        self.matcher.is_match(path)
    }
}

/// Bytes one reported match row costs in the model-visible result.
pub(super) fn repository_row_bytes(
    relative: &Path,
    line_number: u64,
    column: usize,
    retained: usize,
    truncated: bool,
) -> usize {
    relative
        .to_string_lossy()
        .len()
        .saturating_add(decimal_len(line_number))
        .saturating_add(decimal_len(
            u64::try_from(column.saturating_add(1)).unwrap_or(u64::MAX),
        ))
        .saturating_add(retained)
        .saturating_add(4)
        .saturating_add(if truncated { " …".len() } else { 0 })
}

pub(super) struct RepositoryLiteralMatcher<'a> {
    pub(super) needle: &'a [u8],
    match_case: RepositoryMatchCase,
    pub(super) failure: Vec<usize>,
}

impl<'a> RepositoryLiteralMatcher<'a> {
    pub(super) fn new(needle: &'a [u8], match_case: RepositoryMatchCase) -> Self {
        let mut failure = vec![0; needle.len()];
        let mut matched = 0_usize;
        for index in 1..needle.len() {
            while matched > 0 && !literal_byte_eq(needle[index], needle[matched], match_case) {
                matched = failure[matched - 1];
            }
            if literal_byte_eq(needle[index], needle[matched], match_case) {
                matched = matched.saturating_add(1);
            }
            failure[index] = matched;
        }
        Self {
            needle,
            match_case,
            failure,
        }
    }

    pub(super) fn matches(&self, byte: u8, needle_index: usize) -> bool {
        literal_byte_eq(byte, self.needle[needle_index], self.match_case)
    }
}

fn literal_byte_eq(left: u8, right: u8, match_case: RepositoryMatchCase) -> bool {
    match match_case {
        RepositoryMatchCase::Sensitive => left == right,
        RepositoryMatchCase::AsciiInsensitive => left.eq_ignore_ascii_case(&right),
    }
}

pub(super) fn utf8_prefix_len(bytes: &[u8], maximum: usize) -> usize {
    let mut retained = maximum.min(bytes.len());
    while retained > 0 && std::str::from_utf8(&bytes[..retained]).is_err() {
        retained = retained.saturating_sub(1);
    }
    retained
}

fn decimal_len(value: u64) -> usize {
    value.checked_ilog10().map_or(1, |log| log as usize + 1)
}
