//! Validated workspace request and response values.

use plakous_model::{Digest, DigestDomain};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::{
    ExecutionBackendError, ExecutionBackendErrorKind, MAX_ENVIRONMENT_OUTPUT_BYTES,
    MAX_EXECUTION_VALUE_BYTES, MAX_REPOSITORY_QUERY_MATCHES, MAX_WORKSPACE_FILE_BYTES,
    MAX_WORKSPACE_MUTATION_ITEMS, MAX_WORKSPACE_READ_SLICE_BYTES, MAX_WORKSPACE_WALK_ENTRIES,
    WORKSPACE_CONTENT_IDENTITY_REVISION, backend_error, invalid_request, normalize_relative_path,
    resource_error, validate_workspace_byte_limit,
};

/// Immutable identity of one complete regular-file content state.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct WorkspaceContentIdentity {
    pub(super) byte_len: u64,
    pub(super) digest: Digest,
}

impl WorkspaceContentIdentity {
    /// Reconstructs comparison data from the exact canonical digest fields.
    ///
    /// # Errors
    ///
    /// Rejects an oversized length or a digest outside the exact blob domain/revision.
    pub fn new(byte_len: u64, digest: Digest) -> Result<Self, ExecutionBackendError> {
        if byte_len > MAX_ENVIRONMENT_OUTPUT_BYTES
            || digest.domain() != DigestDomain::Blob
            || digest.version() != WORKSPACE_CONTENT_IDENTITY_REVISION
        {
            return Err(invalid_request("workspace content identity is invalid"));
        }
        Ok(Self { byte_len, digest })
    }

    /// Computes the canonical identity of exact bytes.
    ///
    /// # Errors
    ///
    /// Rejects bytes exceeding the hard workspace-content bound.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ExecutionBackendError> {
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_ENVIRONMENT_OUTPUT_BYTES {
            return Err(resource_error(
                "workspace content exceeds the hard identity limit",
            ));
        }
        Ok(Self {
            byte_len: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            digest: Digest::sha256(
                DigestDomain::Blob,
                WORKSPACE_CONTENT_IDENTITY_REVISION,
                &[bytes],
            )
            .map_err(|_| invalid_request("workspace content identity could not be computed"))?,
        })
    }

    /// Returns the exact content length.
    #[must_use]
    pub const fn byte_len(self) -> u64 {
        self.byte_len
    }
    /// Returns the domain-separated content digest.
    #[must_use]
    pub const fn digest(self) -> Digest {
        self.digest
    }

    /// The opaque revision token a composition cell sees for this content:
    /// `r<revision>-<byte length>-<64 lowercase hex digest>`.
    #[must_use]
    pub fn revision_token(self) -> String {
        format!(
            "r{WORKSPACE_CONTENT_IDENTITY_REVISION}-{}-{}",
            self.byte_len, self.digest
        )
    }

    /// Parses a token [`Self::revision_token`] produced.
    ///
    /// # Errors
    ///
    /// Rejects any other spelling.
    pub fn from_revision_token(token: &str) -> Result<Self, ExecutionBackendError> {
        let invalid = || invalid_request("revision token is invalid");
        let rest = token
            .strip_prefix(&format!("r{WORKSPACE_CONTENT_IDENTITY_REVISION}-"))
            .ok_or_else(invalid)?;
        let (length, hex) = rest.split_once('-').ok_or_else(invalid)?;
        if length.is_empty()
            || (length.len() > 1 && length.starts_with('0'))
            || hex.len() != 64
            || !hex
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        {
            return Err(invalid());
        }
        let byte_len = length.parse::<u64>().map_err(|_| invalid())?;
        let mut bytes = [0_u8; 32];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte =
                u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).map_err(|_| invalid())?;
        }
        let digest = Digest::from_sha256_bytes(
            DigestDomain::Blob,
            WORKSPACE_CONTENT_IDENTITY_REVISION,
            bytes,
        )
        .map_err(|_| invalid())?;
        Self::new(byte_len, digest)
    }
}

/// Cursor bound to one normalized path and immutable content state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceReadCursor {
    pub(super) path: PathBuf,
    pub(super) content: WorkspaceContentIdentity,
    pub(super) offset: u64,
}

impl WorkspaceReadCursor {
    /// Reconstructs a normalized continuation from model-visible comparison data.
    ///
    /// # Errors
    ///
    /// Rejects an invalid path or an offset outside the named content.
    pub fn new(
        path: impl Into<PathBuf>,
        content: WorkspaceContentIdentity,
        offset: u64,
    ) -> Result<Self, ExecutionBackendError> {
        if offset == 0 || offset >= content.byte_len() {
            return Err(invalid_request("workspace read cursor offset is invalid"));
        }
        Ok(Self {
            path: normalize_relative_path(&path.into(), false)?,
            content,
            offset,
        })
    }

    /// Returns the normalized path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
    /// Returns the required immutable content state.
    #[must_use]
    pub const fn content(&self) -> WorkspaceContentIdentity {
        self.content
    }
    /// Returns the next byte offset.
    #[must_use]
    pub const fn offset(&self) -> u64 {
        self.offset
    }
}

/// One bounded, version-bound regular-file read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceReadSliceRequest {
    pub(super) path: PathBuf,
    pub(super) expected: Option<WorkspaceContentIdentity>,
    pub(super) offset: u64,
    pub(super) max_bytes: u64,
}

impl WorkspaceReadSliceRequest {
    /// Starts a read at byte zero.
    ///
    /// # Errors
    ///
    /// Rejects an invalid path or byte bound.
    pub fn start(path: impl Into<PathBuf>, max_bytes: u64) -> Result<Self, ExecutionBackendError> {
        let path = normalize_relative_path(&path.into(), false)?;
        validate_workspace_byte_limit(max_bytes, "workspace read byte limit is invalid")?;
        Ok(Self {
            path,
            expected: None,
            offset: 0,
            max_bytes,
        })
    }

    /// Continues only the exact content named by a prior slice.
    ///
    /// # Errors
    ///
    /// Rejects an invalid byte bound.
    pub fn resume(
        cursor: WorkspaceReadCursor,
        max_bytes: u64,
    ) -> Result<Self, ExecutionBackendError> {
        validate_workspace_byte_limit(max_bytes, "workspace read byte limit is invalid")?;
        Ok(Self {
            path: cursor.path,
            expected: Some(cursor.content),
            offset: cursor.offset,
            max_bytes,
        })
    }

    /// Returns the normalized path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
    /// Returns the exact content precondition carried by a resumed read.
    #[must_use]
    pub const fn expected_content(&self) -> Option<WorkspaceContentIdentity> {
        self.expected
    }
    /// Returns the requested byte offset.
    #[must_use]
    pub const fn offset(&self) -> u64 {
        self.offset
    }
    /// Returns the retained-byte bound.
    #[must_use]
    pub const fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    pub(super) fn enforce_limit(&self, maximum: u64) -> Result<(), ExecutionBackendError> {
        if self.max_bytes > maximum || self.max_bytes > MAX_WORKSPACE_READ_SLICE_BYTES {
            Err(resource_error(
                "workspace read slice exceeds environment limits",
            ))
        } else {
            Ok(())
        }
    }
}

/// Exact bounded bytes plus a continuation bound to the same content.
#[derive(Clone, Eq, PartialEq)]
pub struct WorkspaceReadSlice {
    pub(super) content: WorkspaceContentIdentity,
    pub(super) offset: u64,
    pub(super) bytes: Arc<[u8]>,
    pub(super) next: Option<WorkspaceReadCursor>,
}

impl WorkspaceReadSlice {
    /// Returns immutable whole-file content identity.
    #[must_use]
    pub const fn content(&self) -> WorkspaceContentIdentity {
        self.content
    }
    /// Returns the first byte offset in this slice.
    #[must_use]
    pub const fn offset(&self) -> u64 {
        self.offset
    }
    /// Returns exact retained bytes.
    #[must_use]
    pub const fn bytes(&self) -> &Arc<[u8]> {
        &self.bytes
    }
    /// Returns a continuation only when more bytes exist.
    #[must_use]
    pub const fn next(&self) -> Option<&WorkspaceReadCursor> {
        self.next.as_ref()
    }
}

impl fmt::Debug for WorkspaceReadSlice {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkspaceReadSlice")
            .field("content", &self.content)
            .field("offset", &self.offset)
            .field("bytes", &self.bytes.len())
            .field("has_next", &self.next.is_some())
            .finish()
    }
}

/// Exact precondition for one workspace path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceExpectedFileState {
    /// The path must not exist.
    Missing,
    /// The path may be absent or a regular file; publication creates or replaces.
    Any,
    /// The path must name a regular file with this exact content identity.
    Content(WorkspaceContentIdentity),
}

/// Exact terminal state requested for one workspace path.
#[derive(Clone, Eq, PartialEq)]
pub enum WorkspaceReplacement {
    /// The path must be absent after publication.
    Missing,
    /// The path must contain these exact bytes after publication.
    Content(Arc<[u8]>),
}

impl WorkspaceReplacement {
    /// Creates exact replacement bytes.
    #[must_use]
    pub fn content(bytes: impl Into<Vec<u8>>) -> Self {
        Self::Content(bytes.into().into())
    }
    /// Returns replacement bytes when present.
    #[must_use]
    pub const fn bytes(&self) -> Option<&Arc<[u8]>> {
        if let Self::Content(bytes) = self {
            Some(bytes)
        } else {
            None
        }
    }
    pub(super) fn byte_len(&self) -> usize {
        self.bytes().map_or(0, |bytes| bytes.len())
    }
}

impl fmt::Debug for WorkspaceReplacement {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing => formatter.write_str("WorkspaceReplacement::Missing"),
            Self::Content(bytes) => formatter
                .debug_tuple("WorkspaceReplacement::Content")
                .field(&bytes.len())
                .finish(),
        }
    }
}

/// One exact precondition and replacement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceMutation {
    pub(super) path: PathBuf,
    pub(super) expected: WorkspaceExpectedFileState,
    pub(super) replacement: WorkspaceReplacement,
}

impl WorkspaceMutation {
    /// Creates one normalized path mutation.
    ///
    /// # Errors
    ///
    /// Rejects an invalid workspace-relative path.
    pub fn new(
        path: impl Into<PathBuf>,
        expected: WorkspaceExpectedFileState,
        replacement: WorkspaceReplacement,
    ) -> Result<Self, ExecutionBackendError> {
        if matches!(
            (expected, &replacement),
            (
                WorkspaceExpectedFileState::Any,
                WorkspaceReplacement::Missing
            )
        ) {
            return Err(backend_error(
                ExecutionBackendErrorKind::InvalidRequest,
                "workspace mutation cannot delete under an unspecified expected state",
            ));
        }
        Ok(Self {
            path: normalize_relative_path(&path.into(), false)?,
            expected,
            replacement,
        })
    }
    /// Returns the normalized path relative to the environment cwd.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
    /// Returns the exact state that must hold before publication.
    #[must_use]
    pub const fn expected(&self) -> WorkspaceExpectedFileState {
        self.expected
    }
    /// Returns the exact state requested after publication.
    #[must_use]
    pub const fn replacement(&self) -> &WorkspaceReplacement {
        &self.replacement
    }
}

/// Canonically ordered all-preflight workspace transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceMutationTransaction {
    pub(super) mutations: Arc<[WorkspaceMutation]>,
    pub(super) replacement_bytes: usize,
}

impl WorkspaceMutationTransaction {
    /// Creates a nonempty transaction, sorting by normalized path.
    ///
    /// # Errors
    ///
    /// Rejects empty, duplicate, excessive, or oversized mutations.
    pub fn new(
        mutations: impl IntoIterator<Item = WorkspaceMutation>,
    ) -> Result<Self, ExecutionBackendError> {
        let mut mutations = mutations.into_iter().collect::<Vec<_>>();
        if mutations.is_empty() || mutations.len() > MAX_WORKSPACE_MUTATION_ITEMS {
            return Err(invalid_request("workspace mutation count is invalid"));
        }
        mutations.sort_by(|left, right| left.path.cmp(&right.path));
        if mutations
            .windows(2)
            .any(|pair| pair[0].path == pair[1].path)
        {
            return Err(invalid_request(
                "workspace mutation contains a duplicate path",
            ));
        }
        let replacement_bytes = mutations.iter().try_fold(0_usize, |sum, item| {
            sum.checked_add(item.replacement.byte_len())
                .ok_or_else(|| resource_error("workspace mutation size overflowed"))
        })?;
        if u64::try_from(replacement_bytes).unwrap_or(u64::MAX) > MAX_ENVIRONMENT_OUTPUT_BYTES {
            return Err(resource_error(
                "workspace mutation exceeds the hard content limit",
            ));
        }
        Ok(Self {
            mutations: mutations.into(),
            replacement_bytes,
        })
    }
    /// Returns the mutations in canonical path order.
    #[must_use]
    pub fn mutations(&self) -> &[WorkspaceMutation] {
        &self.mutations
    }
    pub(super) fn enforce_limit(&self, maximum: u64) -> Result<(), ExecutionBackendError> {
        let replacement = u64::try_from(self.replacement_bytes).unwrap_or(u64::MAX);
        let expected = self.mutations.iter().any(|item| matches!(item.expected, WorkspaceExpectedFileState::Content(content) if content.byte_len() > maximum));
        if replacement > maximum
            || expected
            || self.mutations.iter().any(|item| {
                u64::try_from(item.replacement.byte_len()).unwrap_or(u64::MAX) > maximum
            })
        {
            Err(resource_error(
                "workspace mutation content exceeds environment limits",
            ))
        } else {
            Ok(())
        }
    }
}

/// Typed disposition of one successfully published mutation item.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum WorkspaceMutationDisposition {
    /// A missing file was created.
    Created,
    /// An existing regular file was replaced.
    Updated,
    /// An existing regular file was removed.
    Deleted,
    /// Expected and replacement states were identical.
    Unchanged,
}

/// One bounded mutation-result item.
#[derive(Clone, Eq, PartialEq)]
pub struct WorkspaceMutationResultItem {
    pub(super) path: PathBuf,
    pub(super) disposition: WorkspaceMutationDisposition,
}

impl WorkspaceMutationResultItem {
    /// Returns the normalized path relative to the environment cwd.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the published disposition.
    #[must_use]
    pub const fn disposition(&self) -> WorkspaceMutationDisposition {
        self.disposition
    }
}

impl fmt::Debug for WorkspaceMutationResultItem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkspaceMutationResultItem")
            .field("path", &"<redacted>")
            .field("disposition", &self.disposition)
            .finish()
    }
}

/// Bounded ordered result of one successful workspace transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceMutationTransactionResult {
    pub(super) items: Arc<[WorkspaceMutationResultItem]>,
}

impl WorkspaceMutationTransactionResult {
    /// Returns one result per mutation in canonical path order.
    #[must_use]
    pub fn items(&self) -> &[WorkspaceMutationResultItem] {
        &self.items
    }
}

/// Repository literal matching mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepositoryMatchCase {
    /// Matches literal bytes exactly.
    Sensitive,
    /// Matches ASCII letters without regard to case.
    AsciiInsensitive,
}

/// Complete independently bounded repository-query resources.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(
    clippy::struct_field_names,
    reason = "the max prefix makes every independent authority bound explicit"
)]
pub struct RepositoryQueryLimits {
    pub(super) max_files: usize,
    pub(super) max_tree_entries: usize,
    pub(super) max_file_bytes: u64,
    pub(super) max_total_bytes: u64,
    pub(super) max_matches: usize,
    pub(super) max_match_bytes: usize,
    pub(super) max_result_bytes: usize,
}

impl RepositoryQueryLimits {
    /// Creates validated independent traversal, input, match, and result bounds.
    ///
    /// # Errors
    ///
    /// Rejects zero, inconsistent, or hard-limit-exceeding bounds.
    #[allow(
        clippy::too_many_arguments,
        reason = "independent query bounds are explicit authority"
    )]
    pub fn new(
        max_files: usize,
        max_tree_entries: usize,
        max_file_bytes: u64,
        max_total_bytes: u64,
        max_matches: usize,
        max_match_bytes: usize,
        max_result_bytes: usize,
    ) -> Result<Self, ExecutionBackendError> {
        let counts = max_files > 0
            && max_files <= MAX_WORKSPACE_WALK_ENTRIES
            && max_tree_entries > 0
            && max_tree_entries <= MAX_WORKSPACE_WALK_ENTRIES
            && max_matches > 0
            && max_matches <= MAX_REPOSITORY_QUERY_MATCHES;
        let bytes = max_file_bytes > 0
            && max_file_bytes <= MAX_WORKSPACE_FILE_BYTES
            && max_total_bytes >= max_file_bytes
            && max_total_bytes <= MAX_WORKSPACE_FILE_BYTES.saturating_mul(16)
            && max_match_bytes > 0
            && u64::try_from(max_match_bytes).unwrap_or(u64::MAX) <= MAX_ENVIRONMENT_OUTPUT_BYTES
            && max_result_bytes > 0
            && u64::try_from(max_result_bytes).unwrap_or(u64::MAX)
                <= MAX_WORKSPACE_READ_SLICE_BYTES;
        if !counts || !bytes {
            return Err(invalid_request("repository query limits are invalid"));
        }
        Ok(Self {
            max_files,
            max_tree_entries,
            max_file_bytes,
            max_total_bytes,
            max_matches,
            max_match_bytes,
            max_result_bytes,
        })
    }
}

/// One canonical bounded literal repository query.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryQuery {
    pub(super) root: PathBuf,
    pub(super) literal: Arc<[u8]>,
    pub(super) match_case: RepositoryMatchCase,
    pub(super) limits: RepositoryQueryLimits,
    pub(super) path_glob: Option<Arc<str>>,
    pub(super) list_only: bool,
    pub(super) regex: Option<Arc<str>>,
}

impl RepositoryQuery {
    /// Creates a bounded literal query rooted at a normalized path under the cwd.
    ///
    /// # Errors
    ///
    /// Rejects an invalid root, literal, case mode, or limit combination.
    pub fn new(
        root: impl Into<PathBuf>,
        literal: impl Into<Vec<u8>>,
        match_case: RepositoryMatchCase,
        limits: RepositoryQueryLimits,
    ) -> Result<Self, ExecutionBackendError> {
        let root = normalize_relative_path(&root.into(), true)?;
        let literal = literal.into();
        if literal.is_empty() || literal.len() > MAX_EXECUTION_VALUE_BYTES || literal.contains(&0) {
            return Err(invalid_request("repository query literal is invalid"));
        }
        if match_case == RepositoryMatchCase::AsciiInsensitive && !literal.is_ascii() {
            return Err(invalid_request("ASCII-insensitive query must be ASCII"));
        }
        Ok(Self {
            root,
            literal: literal.into(),
            match_case,
            limits,
            path_glob: None,
            list_only: false,
            regex: None,
        })
    }

    /// Creates a bounded regular-expression query: each line of each walked
    /// UTF-8 file is matched, and a line is reported once. `match_case`
    /// selects case-insensitive matching.
    ///
    /// # Errors
    ///
    /// Rejects an invalid root, a pattern that does not compile, or an
    /// invalid limit combination.
    pub fn regex(
        root: impl Into<PathBuf>,
        pattern: impl Into<String>,
        match_case: RepositoryMatchCase,
        limits: RepositoryQueryLimits,
    ) -> Result<Self, ExecutionBackendError> {
        let root = normalize_relative_path(&root.into(), true)?;
        let pattern = pattern.into();
        if pattern.is_empty() || pattern.len() > MAX_EXECUTION_VALUE_BYTES || pattern.contains('\0')
        {
            return Err(invalid_request("repository query pattern is invalid"));
        }
        compile_query_regex(&pattern, match_case)?;
        Ok(Self {
            root,
            literal: Arc::from([]),
            match_case,
            limits,
            path_glob: None,
            list_only: false,
            regex: Some(pattern.into()),
        })
    }

    /// Creates a path-glob listing query (no content needle).
    ///
    /// # Errors
    ///
    /// Rejects an invalid root, empty glob, or limit combination.
    pub fn glob(
        root: impl Into<PathBuf>,
        pattern: impl Into<String>,
        limits: RepositoryQueryLimits,
    ) -> Result<Self, ExecutionBackendError> {
        let root = normalize_relative_path(&root.into(), true)?;
        let pattern = pattern.into();
        if pattern.is_empty() || pattern.len() > MAX_EXECUTION_VALUE_BYTES || pattern.contains('\0')
        {
            return Err(invalid_request("repository glob pattern is invalid"));
        }
        Ok(Self {
            root,
            literal: Arc::from([]),
            match_case: RepositoryMatchCase::Sensitive,
            limits,
            path_glob: Some(pattern.into()),
            list_only: true,
            regex: None,
        })
    }

    /// Restricts an existing content query to paths matching `pattern`.
    ///
    /// # Errors
    ///
    /// Rejects an empty glob.
    pub fn with_path_glob(
        mut self,
        pattern: impl Into<String>,
    ) -> Result<Self, ExecutionBackendError> {
        let pattern = pattern.into();
        if pattern.is_empty() || pattern.len() > MAX_EXECUTION_VALUE_BYTES || pattern.contains('\0')
        {
            return Err(invalid_request("repository glob pattern is invalid"));
        }
        self.path_glob = Some(pattern.into());
        Ok(self)
    }
    /// Returns the normalized query root relative to the environment cwd.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }
    /// Returns the nonempty literal byte sequence to match.
    #[must_use]
    pub fn literal(&self) -> &[u8] {
        &self.literal
    }
    /// Returns the query's literal matching mode.
    #[must_use]
    pub const fn match_case(&self) -> RepositoryMatchCase {
        self.match_case
    }
    /// Returns the complete resource bounds for the query.
    #[must_use]
    pub const fn limits(&self) -> RepositoryQueryLimits {
        self.limits
    }
}

/// Compiles a query pattern for line-by-line byte matching.
pub(super) fn compile_query_regex(
    pattern: &str,
    match_case: RepositoryMatchCase,
) -> Result<regex::bytes::Regex, ExecutionBackendError> {
    regex::bytes::RegexBuilder::new(pattern)
        .case_insensitive(match_case == RepositoryMatchCase::AsciiInsensitive)
        .size_limit(1 << 22)
        .build()
        .map_err(|_| invalid_request("repository query pattern is not a valid regular expression"))
}

/// One deterministic repository match.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryQueryMatch {
    pub(super) path: PathBuf,
    pub(super) line: u64,
    pub(super) column: u64,
    pub(super) bytes: Arc<[u8]>,
    pub(super) truncated: bool,
    pub(super) content: WorkspaceContentIdentity,
}
impl RepositoryQueryMatch {
    /// Returns the matched path relative to the environment cwd.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
    /// Returns the one-based line number containing the match.
    #[must_use]
    pub const fn line(&self) -> u64 {
        self.line
    }
    /// Returns the one-based byte column at which the match begins.
    #[must_use]
    pub const fn column(&self) -> u64 {
        self.column
    }
    /// Returns the bounded bytes retained from the matching line.
    #[must_use]
    pub const fn bytes(&self) -> &Arc<[u8]> {
        &self.bytes
    }
    /// Reports whether the matching line exceeded its retained byte bound.
    #[must_use]
    pub const fn truncated(&self) -> bool {
        self.truncated
    }
    /// Returns the exact identity of the complete matched file contents.
    #[must_use]
    pub const fn content(&self) -> WorkspaceContentIdentity {
        self.content
    }
}

/// Resource bound that stopped a repository query.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepositoryQueryStopReason {
    /// Directory traversal reached its entry bound.
    TreeEntries,
    /// Discovered files exceeded the consideration bound.
    Files,
    /// Matching rows exceeded the match bound.
    Matches,
    /// The next file would exceed the cumulative scan bound.
    ScannedBytes,
    /// The next matching row would exceed the retained-result bound.
    ResultBytes,
}

/// Bounded deterministic repository-query result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryQueryResult {
    pub(super) matches: Arc<[RepositoryQueryMatch]>,
    pub(super) files_considered: usize,
    pub(super) files_scanned: usize,
    pub(super) bytes_scanned: u64,
    pub(super) skipped_binary: usize,
    pub(super) skipped_oversize: usize,
    pub(super) skipped_changed: usize,
    pub(super) entries_skipped: usize,
    pub(super) stop_reason: Option<RepositoryQueryStopReason>,
}
impl RepositoryQueryResult {
    /// Returns deterministic matches ordered by path and file position.
    #[must_use]
    pub fn matches(&self) -> &[RepositoryQueryMatch] {
        &self.matches
    }
    /// Returns the number of discovered files admitted for consideration.
    #[must_use]
    pub const fn files_considered(&self) -> usize {
        self.files_considered
    }
    /// Returns the number of admitted files whose contents were scanned.
    #[must_use]
    pub const fn files_scanned(&self) -> usize {
        self.files_scanned
    }
    /// Returns the total bytes read from scanned files.
    #[must_use]
    pub const fn bytes_scanned(&self) -> u64 {
        self.bytes_scanned
    }
    /// Returns the number of scanned files skipped after binary detection.
    #[must_use]
    pub const fn skipped_binary(&self) -> usize {
        self.skipped_binary
    }
    /// Returns the number of files skipped for exceeding the per-file bound.
    #[must_use]
    pub const fn skipped_oversize(&self) -> usize {
        self.skipped_oversize
    }
    /// Returns the number of files skipped because they changed between the
    /// walk and the read, or while being read (an appended log, for example).
    #[must_use]
    pub const fn skipped_changed(&self) -> usize {
        self.skipped_changed
    }
    /// Returns the number of directory entries the walk could not traverse.
    ///
    /// Sockets, fifos, devices, and entries whose open or metadata failed are
    /// surveyed past, never fatal. Symbolic links, which the walk refuses by
    /// policy, and entries that vanished mid-walk are not counted.
    #[must_use]
    pub const fn entries_skipped(&self) -> usize {
        self.entries_skipped
    }
    /// Reports whether a resource bound prevented an exhaustive result.
    #[must_use]
    pub const fn incomplete(&self) -> bool {
        self.stop_reason.is_some()
    }
    /// Returns the resource bound that prevented an exhaustive result.
    #[must_use]
    pub const fn stop_reason(&self) -> Option<RepositoryQueryStopReason> {
        self.stop_reason
    }
}
