//! Bounded deterministic repository search and directory listing.
//!
//! Its own bound table and its own walker: search changes independently of
//! edit semantics, so it is its own pack.

#![forbid(unsafe_code)]
#![allow(
    clippy::doc_markdown,
    reason = "doc comments name exact provider tool identifiers"
)]

mod text;

pub use text::{
    PLAKOUS_GROK_GREP_DESCRIPTION, PLAKOUS_GROK_LIST_DIR_DESCRIPTION,
    PLAKOUS_TOOL_SEARCH_REPOSITORY_DESCRIPTION, PLAKOUS_TOOL_SEARCH_REQUIRES_GLOB,
    PLAKOUS_TOOL_SEARCH_REQUIRES_QUERY, PLAKOUS_TOOL_SEARCH_RESULT_BOUND,
};

use std::collections::BTreeMap;
use std::fmt;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use plakous_model::{ToolConcurrency, ToolDefinition, ToolEffect};
use serde::Deserialize;
use serde_json::json;

use plakous_tools::CodingPresentation;
use plakous_tools::PromptPin;
use plakous_tools::ToolProjectionLoweringV2;
use plakous_tools::object_schema;
use plakous_tools::{MenuBinding, MenuEntry, MenuShape, ToolPack, tool_name};
use plakous_tools::{
    RepositoryMatchCase, RepositoryQuery, RepositoryQueryLimits, RepositoryQueryStopReason,
    ToolContext, ToolExecutor, ToolFuture, ToolInvocation, ToolOutcome, ToolRegistration,
    ToolRuntimeError, ToolRuntimeErrorKind,
};

use plakous_tools::{
    MAX_STANDARD_SEARCH_FILES, MAX_STANDARD_SEARCH_MATCHES, SEARCH_REPOSITORY_CAPABILITY_NAME,
};
use plakous_tools::{
    backend_runtime_error, decode_arguments, deserialize_optional_semantic_u64,
    generation_stamp_prefix, operation_control,
};
use plakous_tools::{standard_definition, standard_registration};

const STANDARD_SEARCH_MATCHES: usize = 256;
const STANDARD_SEARCH_FILE_BYTES: u64 = 1024 * 1024;
const STANDARD_SEARCH_TREE_ENTRIES: usize = 10_000;
// Covers ordinary source trees well beyond this repository while bounding I/O.
const STANDARD_SEARCH_TOTAL_BYTES: u64 = 256 * 1024 * 1024;
const STANDARD_SEARCH_MATCH_BYTES: usize = 4 * 1024;
const STANDARD_SEARCH_RESULT_BYTES: usize = 64 * 1024;
// A regex walk may collect more rows than one page prints: a count or a path
// list folds many matches into one line.
const STANDARD_SEARCH_REGEX_RESULT_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone)]
pub(crate) struct SearchRepositoryTool;

struct BoundedSearchResult {
    text: String,
    max_bytes: usize,
}

impl BoundedSearchResult {
    fn new(max_bytes: usize) -> Self {
        Self {
            text: String::new(),
            max_bytes,
        }
    }

    fn finish(self) -> String {
        self.text
    }
}

impl fmt::Write for BoundedSearchResult {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        if value.len() > self.max_bytes.saturating_sub(self.text.len()) {
            return Err(fmt::Error);
        }
        self.text.push_str(value);
        Ok(())
    }
}

/// Reports entries the repository walk could not traverse, and only then.
///
/// A clean walk keeps the existing result text byte for byte; a walk that
/// surveyed past a socket, fifo, device or unreadable entry says so, so the
/// model reads a short result as bounded rather than as an empty repository.
fn write_entries_skipped(
    output: &mut BoundedSearchResult,
    entries_skipped: usize,
) -> Result<(), ToolRuntimeError> {
    if entries_skipped == 0 {
        return Ok(());
    }
    writeln!(output, "entries_skipped={entries_skipped}").map_err(|_| search_result_bound_error())
}

fn search_result_bound_error() -> ToolRuntimeError {
    ToolRuntimeError::new(
        ToolRuntimeErrorKind::ResourceLimit,
        PLAKOUS_TOOL_SEARCH_RESULT_BOUND,
    )
}

fn query_root(context: &ToolContext, path: &Path) -> Result<PathBuf, ToolRuntimeError> {
    context.workspace().relative_input(path)
}

fn execute_list_directory(
    context: &ToolContext,
    args: &SearchArgs,
) -> Result<ToolOutcome, ToolRuntimeError> {
    context.check_cancelled()?;
    let directory = context.workspace().resolve_existing(&args.path)?;
    let metadata = fs::metadata(&directory).map_err(|_| {
        ToolRuntimeError::new(
            ToolRuntimeErrorKind::InvalidArguments,
            "list_dir target is not accessible",
        )
    })?;
    if !metadata.is_dir() {
        return ToolOutcome::failure_text("list_dir target is not a directory");
    }
    let mut entries = fs::read_dir(&directory)
        .map_err(|_| {
            ToolRuntimeError::new(
                ToolRuntimeErrorKind::Execution,
                "list_dir could not read the directory",
            )
        })?
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| !name.starts_with('.'))
        })
        .map(|entry| {
            let suffix = if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                "/"
            } else {
                ""
            };
            format!("{}{suffix}", entry.file_name().to_string_lossy())
        })
        .collect::<Vec<_>>();
    entries.sort();
    let stamp = generation_stamp_prefix(context);
    ToolOutcome::success_text(format!("{stamp}{}", entries.join("\n")))
}

async fn execute_regex_search(
    context: &ToolContext,
    args: &SearchArgs,
    advice: &str,
) -> Result<ToolOutcome, ToolRuntimeError> {
    context.check_cancelled()?;
    let pattern = args.query.as_deref().ok_or_else(|| {
        ToolRuntimeError::new(
            ToolRuntimeErrorKind::InvalidArguments,
            PLAKOUS_TOOL_SEARCH_REQUIRES_QUERY,
        )
    })?;
    let before = args.context.or(args.before_context).unwrap_or(0).min(32) as usize;
    let after = args.context.or(args.after_context).unwrap_or(0).min(32) as usize;
    let counts_only = args.counts_only.unwrap_or(false);
    let paths_only = args.paths_only.unwrap_or(false);
    let offset = args
        .offset
        .and_then(|skip| usize::try_from(skip).ok())
        .unwrap_or(0);
    let head_limit = args
        .head_limit
        .and_then(|limit| usize::try_from(limit).ok())
        .unwrap_or(200);
    // The walk stops on matches, never on a file count: counts and path lists
    // need every match, a content page needs only its window.
    let match_cap = if counts_only || paths_only {
        MAX_STANDARD_SEARCH_MATCHES
    } else {
        offset.saturating_add(head_limit)
    };
    let match_cap = match_cap.clamp(1, MAX_STANDARD_SEARCH_MATCHES);
    let limits = RepositoryQueryLimits::new(
        MAX_STANDARD_SEARCH_FILES,
        STANDARD_SEARCH_TREE_ENTRIES,
        STANDARD_SEARCH_FILE_BYTES,
        STANDARD_SEARCH_TOTAL_BYTES,
        match_cap,
        STANDARD_SEARCH_MATCH_BYTES,
        STANDARD_SEARCH_REGEX_RESULT_BYTES,
    )
    .map_err(backend_runtime_error)?;
    let match_case = if args.case_sensitive.unwrap_or(false) {
        RepositoryMatchCase::Sensitive
    } else {
        RepositoryMatchCase::AsciiInsensitive
    };
    let mut query = RepositoryQuery::regex(
        query_root(context, &args.path)?,
        pattern,
        match_case,
        limits,
    )
    .map_err(|error| {
        ToolRuntimeError::new(
            ToolRuntimeErrorKind::InvalidArguments,
            error.message().as_str(),
        )
    })?;
    if let Some(glob) = args.filename_glob.clone() {
        query = query.with_path_glob(glob).map_err(backend_runtime_error)?;
    }
    let result = context
        .environment()
        .query_repository(query, operation_control(context))
        .await
        .map_err(backend_runtime_error)?;
    context.check_cancelled()?;
    let entries = regex_entries(context, &result, before, after, counts_only, paths_only)?;
    let mut output = BoundedSearchResult::new(STANDARD_SEARCH_RESULT_BYTES - 1024);
    let stamp = generation_stamp_prefix(context);
    let mut clipped = false;
    for line in entries.into_iter().skip(offset).take(head_limit) {
        if writeln!(output, "{line}").is_err() {
            clipped = true;
            break;
        }
    }
    let mut notes = search_limit_notes(&result, match_cap, STANDARD_SEARCH_REGEX_RESULT_BYTES);
    if clipped {
        notes.push(format!(
            "output stopped at {} KiB",
            (STANDARD_SEARCH_RESULT_BYTES - 1024) / 1024
        ));
    }
    let header = search_notes_header(&notes, result.incomplete(), clipped, advice);
    ToolOutcome::success_text(format!("{stamp}{header}{}", output.finish()))
}

fn search_narrowing_advice(exposed_name: &str) -> &'static str {
    match exposed_name {
        "grep" => "narrow path or pattern",
        _ => "narrow path, filename_glob or query",
    }
}

fn search_notes_header(notes: &[String], incomplete: bool, clipped: bool, advice: &str) -> String {
    if notes.is_empty() {
        return String::new();
    }
    let advice = if incomplete || clipped {
        format!("; {advice}")
    } else {
        String::new()
    };
    format!("[incomplete={incomplete}; {}{advice}]\n", notes.join("; "))
}

/// Formats a regex walk's matches as ripgrep-style rows, one per line, a
/// count per file, or a path per file.
fn regex_entries(
    context: &ToolContext,
    result: &plakous_tools::RepositoryQueryResult,
    before: usize,
    after: usize,
    counts_only: bool,
    paths_only: bool,
) -> Result<Vec<String>, ToolRuntimeError> {
    let mut entries = Vec::new();
    let mut by_file: Vec<(&Path, Vec<&plakous_tools::RepositoryQueryMatch>)> = Vec::new();
    for matched in result.matches() {
        match by_file.last_mut() {
            Some((path, hits)) if *path == matched.path() => hits.push(matched),
            _ => by_file.push((matched.path(), vec![matched])),
        }
    }
    for (path, hits) in by_file {
        let file_display = path.to_string_lossy();
        if counts_only {
            entries.push(format!("{file_display}:{}", hits.len()));
        } else if paths_only {
            entries.push(file_display.into_owned());
        } else if before == 0 && after == 0 {
            for hit in hits {
                let line = String::from_utf8_lossy(hit.bytes());
                let clipped = if hit.truncated() { " …" } else { "" };
                entries.push(format!("{file_display}:{}:{line}{clipped}", hit.line()));
            }
        } else {
            append_context_lines(context, path, &hits, before, after, &mut entries)?;
        }
    }
    Ok(entries)
}

/// Reports skipped files and the resource that stopped a repository query.
fn search_limit_notes(
    result: &plakous_tools::RepositoryQueryResult,
    match_cap: usize,
    result_bytes: usize,
) -> Vec<String> {
    let mut notes = Vec::new();
    if result.skipped_oversize() != 0 {
        notes.push(format!(
            "{} files over {} KiB were not searched",
            result.skipped_oversize(),
            STANDARD_SEARCH_FILE_BYTES / 1024
        ));
    }
    if result.skipped_binary() != 0 {
        notes.push(format!(
            "{} binary or non-UTF-8 files were not searched",
            result.skipped_binary()
        ));
    }
    if result.skipped_changed() != 0 {
        notes.push(format!(
            "{} files changed during the search and were not searched",
            result.skipped_changed()
        ));
    }
    if result.entries_skipped() != 0 {
        notes.push(format!(
            "{} unreadable entries were skipped",
            result.entries_skipped()
        ));
    }
    if let Some(reason) = result.stop_reason() {
        let reason = match reason {
            RepositoryQueryStopReason::Matches => {
                format!("match bound: stopped after {match_cap} matches")
            }
            RepositoryQueryStopReason::ScannedBytes => format!(
                "scanned-byte bound: {} MiB",
                STANDARD_SEARCH_TOTAL_BYTES / (1024 * 1024)
            ),
            RepositoryQueryStopReason::TreeEntries => {
                format!("tree-entry bound: {STANDARD_SEARCH_TREE_ENTRIES} entries")
            }
            RepositoryQueryStopReason::Files => {
                format!("file bound: {MAX_STANDARD_SEARCH_FILES} files")
            }
            RepositoryQueryStopReason::ResultBytes => {
                format!("retained-result bound: {} KiB", result_bytes / 1024)
            }
        };
        notes.insert(0, reason);
    }
    notes
}

/// Emits ripgrep-style match (`:`) and context (`-`) lines for one file.
fn append_context_lines(
    context: &ToolContext,
    path: &Path,
    hits: &[&plakous_tools::RepositoryQueryMatch],
    before: usize,
    after: usize,
    entries: &mut Vec<String>,
) -> Result<(), ToolRuntimeError> {
    let resolved = context.workspace().resolve_existing(path)?;
    let bytes = fs::read(&resolved).map_err(|_| {
        ToolRuntimeError::new(
            ToolRuntimeErrorKind::Execution,
            "grep could not reread a matching file for context",
        )
    })?;
    let text = String::from_utf8_lossy(&bytes);
    let file_lines: Vec<&str> = text.lines().collect();
    let file_display = path.to_string_lossy();
    let mut emitted = BTreeMap::<usize, char>::new();
    for hit in hits {
        let line_index = usize::try_from(hit.line().saturating_sub(1)).unwrap_or(usize::MAX);
        let start = line_index.saturating_sub(before);
        let end = line_index.saturating_add(after + 1).min(file_lines.len());
        for index in start..end {
            let marker = if index == line_index { ':' } else { '-' };
            emitted
                .entry(index)
                .and_modify(|existing| {
                    if marker == ':' {
                        *existing = ':';
                    }
                })
                .or_insert(marker);
        }
    }
    for (index, marker) in emitted {
        let line = file_lines.get(index).copied().unwrap_or_default();
        entries.push(format!("{file_display}{marker}{}{marker}{line}", index + 1));
    }
    Ok(())
}

impl SearchRepositoryTool {
    pub(crate) fn new() -> Result<(ToolDefinition, Self), ToolRuntimeError> {
        Ok((
            standard_definition(
                SEARCH_REPOSITORY_CAPABILITY_NAME,
                PLAKOUS_TOOL_SEARCH_REPOSITORY_DESCRIPTION,
                object_schema(
                    json!({
                        "query": {"type": "string"},
                        "path": {"type": "string"},
                        "case_sensitive": {"type": "boolean"},
                        "filename_glob": {"type": "string"},
                        "paths_only": {"type": "boolean"},
                        "counts_only": {"type": "boolean"},
                        "head_limit": {"type": "integer", "minimum": 1, "maximum": 4096},
                        "offset": {"type": "integer", "minimum": 0},
                        "use_regex": {"type": "boolean"},
                        "list_directory": {"type": "boolean"},
                        "before_context": {"type": "integer", "minimum": 0},
                        "after_context": {"type": "integer", "minimum": 0},
                        "context": {"type": "integer", "minimum": 0}
                    }),
                    &["path", "query"],
                ),
                ToolConcurrency::Parallel,
                ToolEffect::ReadOnly,
            )?,
            Self,
        ))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    query: Option<String>,
    path: PathBuf,
    case_sensitive: Option<bool>,
    filename_glob: Option<String>,
    paths_only: Option<bool>,
    counts_only: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_optional_semantic_u64")]
    head_limit: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_optional_semantic_u64")]
    offset: Option<u64>,
    #[serde(default)]
    use_regex: Option<bool>,
    #[serde(default)]
    list_directory: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_optional_semantic_u64")]
    before_context: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_optional_semantic_u64")]
    after_context: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_optional_semantic_u64")]
    context: Option<u64>,
}

impl ToolExecutor for SearchRepositoryTool {
    #[allow(
        clippy::too_many_lines,
        reason = "search execute keeps query, stamp, and bounded match formatting together"
    )]
    fn execute(self: Arc<Self>, invocation: ToolInvocation, context: ToolContext) -> ToolFuture {
        Box::pin(async move {
            context.check_cancelled()?;
            let args: SearchArgs = decode_arguments(&invocation)?;
            let advice = search_narrowing_advice(
                invocation
                    .exposure()
                    .map_or(invocation.identity().name(), |binding| {
                        binding.exposed_name()
                    })
                    .as_str(),
            );
            if args.list_directory.unwrap_or(false) {
                return execute_list_directory(&context, &args);
            }
            if args.use_regex.unwrap_or(false) {
                return execute_regex_search(&context, &args, advice).await;
            }
            {
                let q: String = args
                    .query
                    .as_deref()
                    .or(args.filename_glob.as_deref())
                    .unwrap_or_default()
                    .chars()
                    .take(100)
                    .collect();
                let _ = context.progress().report(&q, None, None);
            }
            let match_cap = repository_match_cap(
                &args,
                if args.paths_only.unwrap_or(false) {
                    MAX_STANDARD_SEARCH_MATCHES
                } else {
                    STANDARD_SEARCH_MATCHES
                },
            );
            let limits = if args.paths_only.unwrap_or(false) {
                repository_query_limits(&args, MAX_STANDARD_SEARCH_MATCHES)?
            } else {
                repository_query_limits(&args, STANDARD_SEARCH_MATCHES)?
            };
            let root = query_root(&context, &args.path)?;
            let query = if args.paths_only.unwrap_or(false) {
                let glob = args.filename_glob.ok_or_else(|| {
                    ToolRuntimeError::new(
                        ToolRuntimeErrorKind::InvalidArguments,
                        PLAKOUS_TOOL_SEARCH_REQUIRES_GLOB,
                    )
                })?;
                RepositoryQuery::glob(root, glob, limits).map_err(backend_runtime_error)?
            } else {
                let needle = args.query.ok_or_else(|| {
                    ToolRuntimeError::new(
                        ToolRuntimeErrorKind::InvalidArguments,
                        PLAKOUS_TOOL_SEARCH_REQUIRES_QUERY,
                    )
                })?;
                let mut query = RepositoryQuery::new(
                    root,
                    needle.into_bytes(),
                    if args.case_sensitive.unwrap_or(false) {
                        RepositoryMatchCase::Sensitive
                    } else {
                        RepositoryMatchCase::AsciiInsensitive
                    },
                    limits,
                )
                .map_err(backend_runtime_error)?;
                if let Some(glob) = args.filename_glob {
                    query = query.with_path_glob(glob).map_err(backend_runtime_error)?;
                }
                query
            };
            let result = context
                .environment()
                .query_repository(query, operation_control(&context))
                .await
                .map_err(backend_runtime_error)?;
            context.check_cancelled()?;
            let stamp = generation_stamp_prefix(&context);
            let header = search_notes_header(
                &search_limit_notes(&result, match_cap, STANDARD_SEARCH_RESULT_BYTES - 1024),
                result.incomplete(),
                false,
                advice,
            );
            if args.paths_only.unwrap_or(false) {
                let offset = args
                    .offset
                    .and_then(|skip| usize::try_from(skip).ok())
                    .unwrap_or(0);
                let head_limit = args
                    .head_limit
                    .and_then(|limit| usize::try_from(limit).ok())
                    .unwrap_or(result.matches().len());
                let mut output = BoundedSearchResult::new(STANDARD_SEARCH_RESULT_BYTES);
                for matched in result.matches().iter().skip(offset).take(head_limit) {
                    writeln!(output, "{}", matched.path().to_string_lossy())
                        .map_err(|_| search_result_bound_error())?;
                }
                write_entries_skipped(&mut output, result.entries_skipped())?;
                return ToolOutcome::success_text(format!("{stamp}{header}{}", output.finish()));
            }
            let mut output = BoundedSearchResult::new(STANDARD_SEARCH_RESULT_BYTES);
            writeln!(
                output,
                "{stamp}{header}matches={} files_considered={} files_scanned={} bytes_scanned={} skipped_binary={} skipped_oversize={} incomplete={}",
                result.matches().len(),
                result.files_considered(),
                result.files_scanned(),
                result.bytes_scanned(),
                result.skipped_binary(),
                result.skipped_oversize(),
                result.incomplete(),
            )
            .map_err(|_| search_result_bound_error())?;
            write_entries_skipped(&mut output, result.entries_skipped())?;
            let match_limit = args
                .head_limit
                .and_then(|limit| usize::try_from(limit).ok())
                .unwrap_or(result.matches().len())
                .min(result.matches().len());
            for matched in result.matches().iter().take(match_limit) {
                let line = std::str::from_utf8(matched.bytes()).map_err(|error| {
                    ToolRuntimeError::from_sensitive_source(
                        ToolRuntimeErrorKind::Execution,
                        "repository query returned non-UTF-8 text",
                        &error,
                    )
                })?;
                writeln!(
                    output,
                    "{}:{}:{}:{}{}",
                    matched.path().to_string_lossy(),
                    matched.line(),
                    matched.column(),
                    line,
                    if matched.truncated() { " …" } else { "" },
                )
                .map_err(|_| search_result_bound_error())?;
            }
            ToolOutcome::success_text(output.finish())
        })
    }
}

fn repository_query_limits(
    args: &SearchArgs,
    default_match_cap: usize,
) -> Result<RepositoryQueryLimits, ToolRuntimeError> {
    let max_matches = repository_match_cap(args, default_match_cap);
    RepositoryQueryLimits::new(
        MAX_STANDARD_SEARCH_FILES,
        STANDARD_SEARCH_TREE_ENTRIES,
        STANDARD_SEARCH_FILE_BYTES,
        STANDARD_SEARCH_TOTAL_BYTES,
        max_matches,
        STANDARD_SEARCH_MATCH_BYTES,
        STANDARD_SEARCH_RESULT_BYTES - 1024,
    )
    .map_err(backend_runtime_error)
}

fn repository_match_cap(args: &SearchArgs, default_match_cap: usize) -> usize {
    let offset = args
        .offset
        .and_then(|skip| usize::try_from(skip).ok())
        .unwrap_or(0);
    let head = args
        .head_limit
        .and_then(|limit| usize::try_from(limit).ok())
        .unwrap_or(default_match_cap);
    // `head_limit`/`offset` cap printed rows. The walker must still collect
    // enough matches for the header and skip window; never shrink below the
    // standard search match cap just because the caller asked to print fewer.
    offset
        .saturating_add(head)
        .max(STANDARD_SEARCH_MATCHES)
        .clamp(1, MAX_STANDARD_SEARCH_MATCHES)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests {
    use std::error::Error;

    use super::*;
    use plakous_model::SemanticJsonV1;

    #[test]
    fn search_args_decode_semantic_json_float_head_limit() -> Result<(), Box<dyn Error>> {
        let lowered = json!({
            "query": "needle",
            "path": ".",
            "case_sensitive": false,
            "filename_glob": "*.rs",
            "head_limit": 3_u64
        });
        let semantic = SemanticJsonV1::from_value(lowered)?;
        assert_eq!(semantic.value()["head_limit"].as_u64(), Some(3));
        assert_eq!(semantic.value()["head_limit"].as_f64(), Some(3.0));
        let args: SearchArgs = serde_json::from_value(semantic.value().as_ref().clone())?;
        assert_eq!(args.head_limit, Some(3));
        let uncapped: SearchArgs = serde_json::from_value(json!({
            "query": "needle",
            "path": ".",
            "case_sensitive": false
        }))?;
        assert_eq!(uncapped.head_limit, None);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The search pack.
// ---------------------------------------------------------------------------

/// Its own bound table and its own walker: bounded deterministic repository
/// search, directory listing, and every provider lowering of them.
#[derive(Clone, Copy, Debug, Default)]
pub struct SearchToolPack;

impl ToolPack for SearchToolPack {
    fn id(&self) -> &'static str {
        "plakous.search.v3"
    }

    fn registrations(&self) -> Result<Vec<ToolRegistration>, ToolRuntimeError> {
        Ok(vec![standard_registration(
            "search-repository",
            SearchRepositoryTool::new(),
        )?])
    }

    fn menu(&self, presentation: CodingPresentation) -> Result<Vec<MenuEntry>, ToolRuntimeError> {
        let search = tool_name(
            SEARCH_REPOSITORY_CAPABILITY_NAME,
            "canonical coding capability name is invalid",
        )?;
        Ok(match presentation {
            // Codex has no search tool; Claude and pi search through the shell.
            CodingPresentation::Codex | CodingPresentation::Claude | CodingPresentation::Pi => {
                Vec::new()
            }
            CodingPresentation::Grok => vec![
                MenuEntry::new(
                    "grep",
                    vec![MenuBinding::by_effect(search.clone())],
                    MenuShape::Structured {
                        description: Arc::from(PLAKOUS_GROK_GREP_DESCRIPTION),
                        schema: object_schema(
                            json!({
                                "pattern": {"type": "string"},
                                "path": {"type": "string"},
                                "glob": {"type": "string"},
                                "-B": {"type": "integer"},
                                "-A": {"type": "integer"},
                                "-C": {"type": "integer"},
                                "-i": {"type": "boolean"},
                                "type": {"type": "string"},
                                "head_limit": {"type": "integer", "maximum": 4096},
                                "multiline": {"type": "boolean"}
                            }),
                            &["pattern"],
                        ),
                    },
                    ToolProjectionLoweringV2::GrokGrep,
                ),
                MenuEntry::new(
                    "list_dir",
                    vec![MenuBinding::by_effect(search)],
                    MenuShape::Structured {
                        description: Arc::from(PLAKOUS_GROK_LIST_DIR_DESCRIPTION),
                        schema: object_schema(
                            json!({
                                "target_directory": {"type": "string"}
                            }),
                            &["target_directory"],
                        ),
                    },
                    ToolProjectionLoweringV2::GrokListDir,
                ),
            ],
        })
    }

    fn pins(&self) -> &'static [PromptPin] {
        SEARCH_PROMPT_PINS
    }
}

/// Every model-visible string the search pack owns.
static SEARCH_PROMPT_PINS: &[PromptPin] = &[
    PromptPin::new(
        "search.search_repository",
        PLAKOUS_TOOL_SEARCH_REPOSITORY_DESCRIPTION,
        "3484b387e9918a7d4fc28e44ab457950d504c8a2ad33f95bae8fd6f578017c1f",
    ),
    PromptPin::new(
        "search.grok.grep",
        PLAKOUS_GROK_GREP_DESCRIPTION,
        "7bf816477c14ebbc3c39d1a36d48dc5f3899cb0f5c24eba457e3a6269cd710c6",
    ),
    PromptPin::new(
        "search.grok.list_dir",
        PLAKOUS_GROK_LIST_DIR_DESCRIPTION,
        "65e9c798849978cb0433bfcd031e586d40bf44a5b0f74ae6a45fbd8aae1d6b89",
    ),
];
