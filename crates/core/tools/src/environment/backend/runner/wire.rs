//! The JSON schema of runner requests, results and errors. Every request is
//! decoded through the same validating constructors a local caller uses; a
//! result keeps the bounds its type carries.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use plakous_model::Digest;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;

use super::super::{
    ExecutionBackendError, ExecutionBackendErrorKind, ExecutionWorkspace, LiteralProcessIntent,
    LiteralProcessRequest, LiteralProcessResult, RepositoryMatchCase, RepositoryQuery,
    RepositoryQueryLimits, RepositoryQueryMatch, RepositoryQueryResult, RepositoryQueryStopReason,
    WorkspaceContentIdentity, WorkspaceExpectedFileState, WorkspaceMutation,
    WorkspaceMutationDisposition, WorkspaceMutationResultItem, WorkspaceMutationTransaction,
    WorkspaceMutationTransactionResult, WorkspaceReadCursor, WorkspaceReadSlice,
    WorkspaceReadSliceRequest, WorkspaceReplacement, invalid_request,
};
use crate::ExecutionEnvironment;

pub(super) fn malformed() -> ExecutionBackendError {
    invalid_request("runner wire value is malformed")
}

pub(super) fn to_value(value: &impl Serialize) -> Result<Value, ExecutionBackendError> {
    serde_json::to_value(value).map_err(|_| malformed())
}

pub(super) fn from_value<T: DeserializeOwned>(value: Value) -> Result<T, ExecutionBackendError> {
    serde_json::from_value(value).map_err(|_| malformed())
}

pub(super) fn path_text(path: &Path) -> Result<String, ExecutionBackendError> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| invalid_request("runner wire path is not UTF-8"))
}

/// Encodes raw instruction paths and file slices for runner JSON payloads.
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

/// Decodes raw bytes from runner JSON payloads.
///
/// # Errors
/// Refuses odd-length text or a non-hexadecimal byte.
pub fn unhex(text: &str) -> Result<Vec<u8>, ExecutionBackendError> {
    if !text.len().is_multiple_of(2) {
        return Err(malformed());
    }
    (0..text.len())
        .step_by(2)
        .map(|index| {
            text.get(index..index + 2)
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
                .ok_or_else(malformed)
        })
        .collect()
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    root: String,
    cwd: String,
    request: Value,
}

/// Wraps a request with the workspace it is for.
pub(super) fn envelope(
    workspace: &ExecutionWorkspace,
    request: Value,
) -> Result<Value, ExecutionBackendError> {
    to_value(&Envelope {
        root: path_text(workspace.root())?,
        cwd: path_text(workspace.cwd())?,
        request,
    })
}

/// The workspace root and cwd `payload` names, when it is an envelope.
pub(super) fn envelope_workspace(payload: &Value) -> Option<(PathBuf, PathBuf)> {
    let root = payload.get("root")?.as_str()?;
    let cwd = payload.get("cwd")?.as_str()?;
    Some((PathBuf::from(root), PathBuf::from(cwd)))
}

/// The request inside `payload`, which must name `environment`'s own
/// workspace.
pub(super) fn open_envelope(
    environment: &ExecutionEnvironment,
    payload: Value,
) -> Result<Value, ExecutionBackendError> {
    let envelope: Envelope = from_value(payload)?;
    if Path::new(&envelope.root) != environment.root()
        || Path::new(&envelope.cwd) != environment.cwd()
    {
        return Err(ExecutionBackendError::new(
            ExecutionBackendErrorKind::WorkspacePolicy,
            "the request names another workspace than this runner serves",
        ));
    }
    Ok(envelope.request)
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ContentWire {
    byte_len: u64,
    digest: Digest,
}

impl ContentWire {
    const fn of(content: WorkspaceContentIdentity) -> Self {
        Self {
            byte_len: content.byte_len,
            digest: content.digest,
        }
    }

    fn identity(self) -> Result<WorkspaceContentIdentity, ExecutionBackendError> {
        WorkspaceContentIdentity::new(self.byte_len, self.digest)
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ReadRequestWire {
    path: String,
    expected: Option<ContentWire>,
    offset: u64,
    max_bytes: u64,
}

pub(super) fn encode_read_request(
    request: &WorkspaceReadSliceRequest,
) -> Result<Value, ExecutionBackendError> {
    to_value(&ReadRequestWire {
        path: path_text(request.path())?,
        expected: request.expected_content().map(ContentWire::of),
        offset: request.offset(),
        max_bytes: request.max_bytes(),
    })
}

pub(super) fn decode_read_request(
    value: Value,
) -> Result<WorkspaceReadSliceRequest, ExecutionBackendError> {
    let wire: ReadRequestWire = from_value(value)?;
    match wire.expected {
        None if wire.offset == 0 => WorkspaceReadSliceRequest::start(wire.path, wire.max_bytes),
        None => Err(malformed()),
        Some(content) => WorkspaceReadSliceRequest::resume(
            WorkspaceReadCursor::new(wire.path, content.identity()?, wire.offset)?,
            wire.max_bytes,
        ),
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CursorWire {
    path: String,
    content: ContentWire,
    offset: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ReadSliceWire {
    content: ContentWire,
    offset: u64,
    bytes: String,
    next: Option<CursorWire>,
}

pub(super) fn encode_read_slice(
    slice: &WorkspaceReadSlice,
) -> Result<Value, ExecutionBackendError> {
    to_value(&ReadSliceWire {
        content: ContentWire::of(slice.content()),
        offset: slice.offset(),
        bytes: hex(slice.bytes()),
        next: slice
            .next()
            .map(|cursor| {
                Ok::<_, ExecutionBackendError>(CursorWire {
                    path: path_text(cursor.path())?,
                    content: ContentWire::of(cursor.content()),
                    offset: cursor.offset(),
                })
            })
            .transpose()?,
    })
}

pub(super) fn decode_read_slice(value: Value) -> Result<WorkspaceReadSlice, ExecutionBackendError> {
    let wire: ReadSliceWire = from_value(value)?;
    let content = wire.content.identity()?;
    let bytes = unhex(&wire.bytes)?;
    let end = wire
        .offset
        .checked_add(u64::try_from(bytes.len()).map_err(|_| malformed())?)
        .ok_or_else(malformed)?;
    if end > content.byte_len() {
        return Err(malformed());
    }
    Ok(WorkspaceReadSlice {
        content,
        offset: wire.offset,
        bytes: bytes.into(),
        next: wire
            .next
            .map(|cursor| {
                WorkspaceReadCursor::new(cursor.path, cursor.content.identity()?, cursor.offset)
            })
            .transpose()?,
    })
}

#[derive(Clone, Copy, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum MatchCaseWire {
    Sensitive,
    AsciiInsensitive,
}

impl MatchCaseWire {
    const fn of(case: RepositoryMatchCase) -> Self {
        match case {
            RepositoryMatchCase::Sensitive => Self::Sensitive,
            RepositoryMatchCase::AsciiInsensitive => Self::AsciiInsensitive,
        }
    }

    const fn case(self) -> RepositoryMatchCase {
        match self {
            Self::Sensitive => RepositoryMatchCase::Sensitive,
            Self::AsciiInsensitive => RepositoryMatchCase::AsciiInsensitive,
        }
    }
}

#[derive(Clone, Copy, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[allow(
    clippy::struct_field_names,
    reason = "mirrors RepositoryQueryLimits field for field"
)]
struct LimitsWire {
    max_files: usize,
    max_tree_entries: usize,
    max_file_bytes: u64,
    max_total_bytes: u64,
    max_matches: usize,
    max_match_bytes: usize,
    max_result_bytes: usize,
}

#[derive(Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct QueryWire {
    root: String,
    literal: String,
    match_case: MatchCaseWire,
    limits: LimitsWire,
    path_glob: Option<String>,
    list_only: bool,
    list_depth: Option<usize>,
    regex: Option<String>,
}

fn query_wire(query: &RepositoryQuery) -> Result<QueryWire, ExecutionBackendError> {
    let limits = query.limits;
    Ok(QueryWire {
        root: path_text(query.root())?,
        literal: hex(query.literal()),
        match_case: MatchCaseWire::of(query.match_case()),
        limits: LimitsWire {
            max_files: limits.max_files,
            max_tree_entries: limits.max_tree_entries,
            max_file_bytes: limits.max_file_bytes,
            max_total_bytes: limits.max_total_bytes,
            max_matches: limits.max_matches,
            max_match_bytes: limits.max_match_bytes,
            max_result_bytes: limits.max_result_bytes,
        },
        path_glob: query.path_glob.as_deref().map(str::to_owned),
        list_only: query.list_only,
        list_depth: query.list_depth,
        regex: query.regex.as_deref().map(str::to_owned),
    })
}

pub(super) fn encode_query(query: &RepositoryQuery) -> Result<Value, ExecutionBackendError> {
    to_value(&query_wire(query)?)
}

/// Rebuilds the query through the constructor its fields name, and refuses
/// a combination no constructor produces.
pub(super) fn decode_query(value: Value) -> Result<RepositoryQuery, ExecutionBackendError> {
    let wire: QueryWire = from_value(value)?;
    let limits = RepositoryQueryLimits::new(
        wire.limits.max_files,
        wire.limits.max_tree_entries,
        wire.limits.max_file_bytes,
        wire.limits.max_total_bytes,
        wire.limits.max_matches,
        wire.limits.max_match_bytes,
        wire.limits.max_result_bytes,
    )?;
    let case = wire.match_case.case();
    let query = match (&wire.regex, wire.list_only, wire.list_depth) {
        (Some(pattern), false, None) => {
            RepositoryQuery::regex(wire.root.clone(), pattern.clone(), case, limits)?
        }
        (None, true, Some(depth)) => {
            RepositoryQuery::list(wire.root.clone(), wire.path_glob.clone(), depth, limits)?
        }
        (None, true, None) => RepositoryQuery::glob(
            wire.root.clone(),
            wire.path_glob.clone().ok_or_else(malformed)?,
            limits,
        )?,
        (None, false, None) => {
            RepositoryQuery::new(wire.root.clone(), unhex(&wire.literal)?, case, limits)?
        }
        _ => return Err(malformed()),
    };
    let query = match (&wire.path_glob, query.path_glob.is_some()) {
        (Some(glob), false) => query.with_path_glob(glob.clone())?,
        _ => query,
    };
    if query_wire(&query)? != wire {
        return Err(malformed());
    }
    Ok(query)
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct MatchWire {
    is_directory: bool,
    entry_size: u64,
    path: String,
    line: u64,
    column: u64,
    bytes: String,
    truncated: bool,
    content: ContentWire,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum StopReasonWire {
    TreeEntries,
    Files,
    Matches,
    ScannedBytes,
    ResultBytes,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct QueryResultWire {
    matches: Vec<MatchWire>,
    files_considered: usize,
    files_scanned: usize,
    bytes_scanned: u64,
    skipped_binary: usize,
    skipped_oversize: usize,
    skipped_changed: usize,
    entries_skipped: usize,
    lossy_lines: u64,
    stop_reason: Option<StopReasonWire>,
}

pub(super) fn encode_query_result(
    result: &RepositoryQueryResult,
) -> Result<Value, ExecutionBackendError> {
    let matches = result
        .matches()
        .iter()
        .map(|item| {
            Ok(MatchWire {
                is_directory: item.is_directory(),
                entry_size: item.entry_size(),
                path: path_text(item.path())?,
                line: item.line(),
                column: item.column(),
                bytes: hex(item.bytes()),
                truncated: item.truncated(),
                content: ContentWire::of(item.content()),
            })
        })
        .collect::<Result<Vec<_>, ExecutionBackendError>>()?;
    to_value(&QueryResultWire {
        matches,
        files_considered: result.files_considered(),
        files_scanned: result.files_scanned(),
        bytes_scanned: result.bytes_scanned(),
        skipped_binary: result.skipped_binary(),
        skipped_oversize: result.skipped_oversize(),
        skipped_changed: result.skipped_changed(),
        entries_skipped: result.entries_skipped(),
        lossy_lines: result.lossy_lines(),
        stop_reason: result.stop_reason().map(|reason| match reason {
            RepositoryQueryStopReason::TreeEntries => StopReasonWire::TreeEntries,
            RepositoryQueryStopReason::Files => StopReasonWire::Files,
            RepositoryQueryStopReason::Matches => StopReasonWire::Matches,
            RepositoryQueryStopReason::ScannedBytes => StopReasonWire::ScannedBytes,
            RepositoryQueryStopReason::ResultBytes => StopReasonWire::ResultBytes,
        }),
    })
}

pub(super) fn decode_query_result(
    value: Value,
) -> Result<RepositoryQueryResult, ExecutionBackendError> {
    let wire: QueryResultWire = from_value(value)?;
    let matches = wire
        .matches
        .into_iter()
        .map(|item| {
            Ok(RepositoryQueryMatch {
                is_directory: item.is_directory,
                entry_size: item.entry_size,
                path: PathBuf::from(item.path),
                line: item.line,
                column: item.column,
                bytes: unhex(&item.bytes)?.into(),
                truncated: item.truncated,
                content: item.content.identity()?,
            })
        })
        .collect::<Result<Vec<_>, ExecutionBackendError>>()?;
    Ok(RepositoryQueryResult {
        matches: matches.into(),
        files_considered: wire.files_considered,
        files_scanned: wire.files_scanned,
        bytes_scanned: wire.bytes_scanned,
        skipped_binary: wire.skipped_binary,
        skipped_oversize: wire.skipped_oversize,
        skipped_changed: wire.skipped_changed,
        entries_skipped: wire.entries_skipped,
        lossy_lines: wire.lossy_lines,
        stop_reason: wire.stop_reason.map(|reason| match reason {
            StopReasonWire::TreeEntries => RepositoryQueryStopReason::TreeEntries,
            StopReasonWire::Files => RepositoryQueryStopReason::Files,
            StopReasonWire::Matches => RepositoryQueryStopReason::Matches,
            StopReasonWire::ScannedBytes => RepositoryQueryStopReason::ScannedBytes,
            StopReasonWire::ResultBytes => RepositoryQueryStopReason::ResultBytes,
        }),
    })
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
enum ExpectedWire {
    Missing,
    Any,
    Content { content: ContentWire },
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
enum ReplacementWire {
    Missing,
    Content { bytes: String },
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct MutationWire {
    path: String,
    expected: ExpectedWire,
    replacement: ReplacementWire,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TransactionWire {
    mutations: Vec<MutationWire>,
}

pub(super) fn encode_transaction(
    transaction: &WorkspaceMutationTransaction,
) -> Result<Value, ExecutionBackendError> {
    let mutations = transaction
        .mutations()
        .iter()
        .map(|mutation| {
            Ok(MutationWire {
                path: path_text(mutation.path())?,
                expected: match mutation.expected() {
                    WorkspaceExpectedFileState::Missing => ExpectedWire::Missing,
                    WorkspaceExpectedFileState::Any => ExpectedWire::Any,
                    WorkspaceExpectedFileState::Content(content) => ExpectedWire::Content {
                        content: ContentWire::of(content),
                    },
                },
                replacement: match mutation.replacement() {
                    WorkspaceReplacement::Missing => ReplacementWire::Missing,
                    WorkspaceReplacement::Content(bytes) => {
                        ReplacementWire::Content { bytes: hex(bytes) }
                    }
                },
            })
        })
        .collect::<Result<Vec<_>, ExecutionBackendError>>()?;
    to_value(&TransactionWire { mutations })
}

pub(super) fn decode_transaction(
    value: Value,
) -> Result<WorkspaceMutationTransaction, ExecutionBackendError> {
    let wire: TransactionWire = from_value(value)?;
    let mutations = wire
        .mutations
        .into_iter()
        .map(|mutation| {
            let expected = match mutation.expected {
                ExpectedWire::Missing => WorkspaceExpectedFileState::Missing,
                ExpectedWire::Any => WorkspaceExpectedFileState::Any,
                ExpectedWire::Content { content } => {
                    WorkspaceExpectedFileState::Content(content.identity()?)
                }
            };
            let replacement = match mutation.replacement {
                ReplacementWire::Missing => WorkspaceReplacement::Missing,
                ReplacementWire::Content { bytes } => WorkspaceReplacement::content(unhex(&bytes)?),
            };
            WorkspaceMutation::new(mutation.path, expected, replacement)
        })
        .collect::<Result<Vec<_>, ExecutionBackendError>>()?;
    WorkspaceMutationTransaction::new(mutations)
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum DispositionWire {
    Created,
    Updated,
    Deleted,
    Unchanged,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ResultItemWire {
    path: String,
    disposition: DispositionWire,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TransactionResultWire {
    items: Vec<ResultItemWire>,
}

pub(super) fn encode_transaction_result(
    result: &WorkspaceMutationTransactionResult,
) -> Result<Value, ExecutionBackendError> {
    let items = result
        .items()
        .iter()
        .map(|item| {
            Ok(ResultItemWire {
                path: path_text(item.path())?,
                disposition: match item.disposition() {
                    WorkspaceMutationDisposition::Created => DispositionWire::Created,
                    WorkspaceMutationDisposition::Updated => DispositionWire::Updated,
                    WorkspaceMutationDisposition::Deleted => DispositionWire::Deleted,
                    WorkspaceMutationDisposition::Unchanged => DispositionWire::Unchanged,
                },
            })
        })
        .collect::<Result<Vec<_>, ExecutionBackendError>>()?;
    to_value(&TransactionResultWire { items })
}

pub(super) fn decode_transaction_result(
    value: Value,
) -> Result<WorkspaceMutationTransactionResult, ExecutionBackendError> {
    let wire: TransactionResultWire = from_value(value)?;
    let items: Arc<[WorkspaceMutationResultItem]> = wire
        .items
        .into_iter()
        .map(|item| WorkspaceMutationResultItem {
            path: PathBuf::from(item.path),
            disposition: match item.disposition {
                DispositionWire::Created => WorkspaceMutationDisposition::Created,
                DispositionWire::Updated => WorkspaceMutationDisposition::Updated,
                DispositionWire::Deleted => WorkspaceMutationDisposition::Deleted,
                DispositionWire::Unchanged => WorkspaceMutationDisposition::Unchanged,
            },
        })
        .collect();
    Ok(WorkspaceMutationTransactionResult { items })
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ProcessRequestWire {
    program: String,
    arguments: Vec<String>,
    cwd: String,
    stdin: String,
    destructive: bool,
    network: bool,
}

pub(super) fn encode_process_request(
    request: &LiteralProcessRequest,
) -> Result<Value, ExecutionBackendError> {
    to_value(&ProcessRequestWire {
        program: request.program().to_owned(),
        arguments: request.arguments().to_vec(),
        cwd: path_text(request.cwd())?,
        stdin: hex(request.stdin()),
        destructive: request.intent().destructive(),
        network: request.intent().network(),
    })
}

pub(super) fn decode_process_request(
    value: Value,
) -> Result<LiteralProcessRequest, ExecutionBackendError> {
    let wire: ProcessRequestWire = from_value(value)?;
    LiteralProcessRequest::new(
        wire.program,
        wire.arguments,
        wire.cwd,
        unhex(&wire.stdin)?,
        LiteralProcessIntent::new(wire.destructive, wire.network),
    )
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ProcessResultWire {
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
}

pub(super) fn encode_process_result(
    result: &LiteralProcessResult,
) -> Result<Value, ExecutionBackendError> {
    to_value(&ProcessResultWire {
        exit_code: result.exit_code(),
        stdout: hex(result.stdout()),
        stderr: hex(result.stderr()),
    })
}

pub(super) fn decode_process_result(
    value: Value,
) -> Result<LiteralProcessResult, ExecutionBackendError> {
    let wire: ProcessResultWire = from_value(value)?;
    LiteralProcessResult::new(wire.exit_code, unhex(&wire.stdout)?, unhex(&wire.stderr)?)
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ErrorWire {
    kind: String,
    message: String,
    proven_no_effect: bool,
    conflict_path: Option<String>,
}

const KINDS: [(ExecutionBackendErrorKind, &str); 18] = [
    (
        ExecutionBackendErrorKind::IdentityMismatch,
        "identity_mismatch",
    ),
    (ExecutionBackendErrorKind::InvalidRequest, "invalid_request"),
    (
        ExecutionBackendErrorKind::WorkspacePolicy,
        "workspace_policy",
    ),
    (ExecutionBackendErrorKind::NotFound, "not_found"),
    (ExecutionBackendErrorKind::IsADirectory, "is_a_directory"),
    (
        ExecutionBackendErrorKind::NotARegularFile,
        "not_a_regular_file",
    ),
    (ExecutionBackendErrorKind::NotADirectory, "not_a_directory"),
    (
        ExecutionBackendErrorKind::OutsideWorkspace,
        "outside_workspace",
    ),
    (ExecutionBackendErrorKind::SymlinkEscape, "symlink_escape"),
    (ExecutionBackendErrorKind::ResourceLimit, "resource_limit"),
    (ExecutionBackendErrorKind::Conflict, "conflict"),
    (
        ExecutionBackendErrorKind::RecoveryRequired,
        "recovery_required",
    ),
    (ExecutionBackendErrorKind::Denied, "denied"),
    (ExecutionBackendErrorKind::Cancelled, "cancelled"),
    (ExecutionBackendErrorKind::TimedOut, "timed_out"),
    (
        ExecutionBackendErrorKind::CleanupUnresolved,
        "cleanup_unresolved",
    ),
    (ExecutionBackendErrorKind::Unsupported, "unsupported"),
    (ExecutionBackendErrorKind::Io, "io"),
];

pub(super) fn encode_error(error: &ExecutionBackendError) -> Value {
    let kind = KINDS
        .iter()
        .find(|(kind, _)| *kind == error.kind())
        .map_or("io", |(_, name)| name);
    serde_json::to_value(ErrorWire {
        kind: kind.to_owned(),
        message: error.message().as_str().to_owned(),
        proven_no_effect: error.proven_no_effect(),
        conflict_path: error
            .workspace_conflict_path()
            .and_then(Path::to_str)
            .map(str::to_owned),
    })
    .unwrap_or(Value::Null)
}

/// A runner's error as this process's error. One that does not decode is of
/// unknown outcome.
pub(super) fn decode_error(value: Value) -> ExecutionBackendError {
    let Ok(wire) = serde_json::from_value::<ErrorWire>(value) else {
        return ExecutionBackendError::new(
            ExecutionBackendErrorKind::RecoveryRequired,
            "the runner's error could not be read; its outcome is unknown",
        );
    };
    let Some(kind) = KINDS
        .iter()
        .find(|(_, name)| *name == wire.kind)
        .map(|(kind, _)| *kind)
    else {
        return ExecutionBackendError::new(
            ExecutionBackendErrorKind::RecoveryRequired,
            "the runner's error kind is unknown; its outcome is unknown",
        );
    };
    let mut error =
        ExecutionBackendError::new(kind, wire.message).with_proven_no_effect(wire.proven_no_effect);
    if let Some(path) = wire.conflict_path {
        error = error.at_workspace_path(Path::new(&path));
    }
    error
}
