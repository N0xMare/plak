//! Export document types, version, and encoding.

use super::history;
use crate::product_config::{ProductCompositionSelection, ProductTargetSelection};
use plakous::model::ModelId;
use plakous_host_protocol::views::presentation::row_bytes;
use plakous_hosted::sessions::{MAX_TRANSCRIPT_BYTES, MAX_TRANSCRIPT_LINES, TranscriptRow};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs;
use std::io::Read as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// The document's `format` value.
pub(super) const FORMAT: &str = "plak-session-export";

/// The version this build writes. A build imports documents of this version
/// and of the immediately previous one. Any change to the document, including
/// the embedded canonical-history and binding encodings, bumps this; the
/// previous version's importer is kept for one bump only.
pub(crate) const VERSION: u32 = 8;
const PREVIOUS_VERSION: u32 = 7;
pub(crate) const IMPORTABLE_VERSIONS: [u32; 2] = [PREVIOUS_VERSION, VERSION];
const MAX_CHECKPOINT_BYTES: usize = 64 * 1024;
pub(super) const MAX_REFERENCE_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_ARCHIVE_BYTES: usize = 64 * 1024 * 1024;
const MAX_DOCUMENT_BYTES: usize = 256 * 1024 * 1024;

/// One exported session.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CarryDocument {
    #[serde(skip)]
    pub(super) local_digest: Option<String>,
    pub(super) format: String,
    pub(super) version: u32,
    pub(super) source: CarrySource,
    pub(super) workspace_root: PathBuf,
    pub(super) title: Option<String>,
    pub(super) binding: CarryBinding,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) checkpoint: Option<CarryCheckpoint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) archive: Option<CarryArchive>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) history: Option<history::History>,
    /// Unresolved source workflow facts retained as inert operator context.
    /// They are never restored into the new session's executable journal.
    pub(super) pending_work: Vec<PendingWorkEntry>,
    /// Queued prompts, oldest first. A prompt already handed to the runtime is
    /// never queued here: export refuses while one is unresolved.
    pub(super) queue: Vec<String>,
    pub(super) prompt_history: Vec<String>,
    /// Typed product transcript rows, oldest first, shared by versions 7 and 8.
    #[serde(deserialize_with = "bounded_transcript")]
    pub(super) transcript: Vec<TranscriptRow>,
    /// Non-live sub-agents the source tree held; their trees are not carried.
    pub(super) source_subagents: usize,
    /// Settled source topology rows omitted from the unresolved ledger.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) source_resolved_count: Option<usize>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CarryCheckpoint {
    pub(super) replay_account: String,
    pub(super) replay_model: ModelId,
    pub(super) user_intent: String,
    pub(super) reference: String,
    truncated: bool,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CarryArchive {
    /// Readable, inert evidence. Import never feeds these entries to a model.
    pub(super) entries: Vec<ArchiveEntry>,
    #[serde(default)]
    pub(super) omitted_entries: usize,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ArchiveEntry {
    pub(super) actor: String,
    pub(super) text: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PendingWorkEntry {
    pub(super) operation: String,
    pub(super) kind: String,
    pub(super) state: String,
    pub(super) detail: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CarrySource {
    pub(super) session_id: String,
    pub(super) state_key: String,
    pub(super) producer: ProducerBuild,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum ProducerBuild {
    Verified { binary_id: String },
    Unavailable,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CarryBinding {
    pub(super) account: Option<String>,
    pub(super) model: Option<String>,
    pub(super) effort: Option<String>,
    pub(super) composition: ProductCompositionSelection,
    pub(super) target_selection: ProductTargetSelection,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum ImportError {
    Oversized {
        section: &'static str,
        bytes: usize,
        maximum: usize,
    },
    Invalid(String),
}

impl fmt::Display for ImportError {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Oversized {
                section,
                bytes,
                maximum,
            } => write!(
                out,
                "session export {section} is oversized ({bytes} bytes; maximum {maximum})"
            ),
            Self::Invalid(message) => out.write_str(message),
        }
    }
}

impl std::error::Error for ImportError {}

impl From<String> for ImportError {
    fn from(message: String) -> Self {
        Self::Invalid(message)
    }
}

/// A document's transcript rows, refused while they are read once they pass
/// the product transcript bounds (rows and encoded bytes): no session can
/// hold more, so a larger document is hostile and is not parsed to its end.
fn bounded_transcript<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<TranscriptRow>, D::Error> {
    struct Rows;
    impl<'de> serde::de::Visitor<'de> for Rows {
        type Value = Vec<TranscriptRow>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a bounded sequence of transcript rows")
        }

        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut rows: A,
        ) -> Result<Self::Value, A::Error> {
            use serde::de::Error as _;
            let mut read = Vec::new();
            let mut bytes = 0_usize;
            while let Some(row) = rows.next_element::<TranscriptRow>()? {
                if read.len() == MAX_TRANSCRIPT_LINES {
                    return Err(A::Error::custom(format!(
                        "the transcript holds more than {MAX_TRANSCRIPT_LINES} rows"
                    )));
                }
                bytes = bytes.saturating_add(row_bytes(&row));
                if bytes > MAX_TRANSCRIPT_BYTES {
                    return Err(A::Error::custom(format!(
                        "the transcript exceeds {MAX_TRANSCRIPT_BYTES} bytes"
                    )));
                }
                read.push(row);
            }
            Ok(read)
        }
    }
    deserializer.deserialize_seq(Rows)
}

pub(super) fn validate_size(
    section: &'static str,
    bytes: usize,
    maximum: usize,
) -> Result<(), ImportError> {
    if bytes > maximum {
        return Err(ImportError::Oversized {
            section,
            bytes,
            maximum,
        });
    }
    Ok(())
}

impl CarryDocument {
    fn validate_archive(&self) -> Result<(), ImportError> {
        let Some(archive) = &self.archive else {
            return Ok(());
        };
        validate_size(
            "archive",
            archive
                .entries
                .iter()
                .map(|e| e.actor.len().saturating_add(e.text.len()))
                .fold(0_usize, usize::saturating_add),
            MAX_ARCHIVE_BYTES,
        )
    }

    pub(super) fn validate(&self) -> Result<(), ImportError> {
        if self
            .pending_work
            .iter()
            .any(|entry| entry.kind == "source_resolved_count")
        {
            return Err(
                "pending work cannot contain source_resolved_count; use the typed field"
                    .to_owned()
                    .into(),
            );
        }
        match (self.version, &self.history, &self.checkpoint, &self.archive) {
            (VERSION, Some(history), None, None) => history.validate_size(),
            (PREVIOUS_VERSION, None, Some(checkpoint), Some(_)) => {
                validate_size(
                    "checkpoint",
                    checkpoint
                        .user_intent
                        .len()
                        .saturating_add(checkpoint.reference.len()),
                    MAX_CHECKPOINT_BYTES,
                )?;
                validate_size(
                    "checkpoint user intent",
                    checkpoint.user_intent.len(),
                    MAX_CHECKPOINT_BYTES / 2,
                )?;
                validate_size(
                    "checkpoint reference",
                    checkpoint.reference.len(),
                    MAX_CHECKPOINT_BYTES / 2,
                )?;
                self.validate_archive()
            }
            _ => Err(ImportError::Invalid(
                "export payload does not match its version".to_owned(),
            )),
        }
    }

    /// The session id and state key this document was exported from.
    pub(crate) fn source(&self) -> (&str, &str) {
        (&self.source.session_id, &self.source.state_key)
    }

    /// The workspace the exported session belonged to.
    pub(crate) fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    pub(crate) fn message_count(&self) -> usize {
        self.history.as_ref().map_or_else(
            || {
                self.checkpoint
                    .as_ref()
                    .map_or(0, |c| 1 + usize::from(!c.reference.is_empty()))
            },
            history::History::message_count,
        )
    }

    pub(crate) fn estimated_tokens(&self) -> u64 {
        self.history.as_ref().map_or_else(
            || {
                self.checkpoint.as_ref().map_or(0, |c| {
                    (c.user_intent.len() + c.reference.len()).div_ceil(4) as u64
                })
            },
            history::History::estimated_tokens,
        )
    }

    #[cfg(any(test, feature = "tui"))]
    pub(crate) fn carry_summary(&self) -> String {
        self.history.as_ref().map_or_else(
            || "version 7 restart checkpoint (conversation unavailable)".to_owned(),
            history::History::summary,
        )
    }
}

/// Writes `document` to a new file only the owner can read. An existing file
/// is refused rather than overwritten.
pub(crate) fn write_document(path: &Path, document: &CarryDocument) -> Result<(), String> {
    write_document_with_directory_sync(path, document, |parent| {
        fs::File::open(parent).and_then(|dir| dir.sync_all())
    })
}

fn write_document_with_directory_sync(
    path: &Path,
    document: &CarryDocument,
    sync_directory: impl FnOnce(&Path) -> std::io::Result<()>,
) -> Result<(), String> {
    let bytes = serde_json::to_vec(document)
        .map_err(|error| format!("the export document could not be encoded: {error}"))?;
    validate_size("document", bytes.len(), MAX_DOCUMENT_BYTES).map_err(|e| e.to_string())?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut file = tempfile::NamedTempFile::new_in(parent)
        .map_err(|error| format!("could not create export temporary file: {error}"))?;
    file.write_all(&bytes)
        .and_then(|()| file.as_file().sync_all())
        .map_err(|error| format!("could not write {}: {error}", path.display()))?;
    file.persist_noclobber(path)
        .map_err(|error| format!("could not publish {}: {error}", path.display()))?;
    // Publication already succeeded; reporting a refusal would hide the kept export.
    if let Err(error) = sync_directory(parent) {
        tracing::warn!(path = %path.display(), %error, "export published but directory sync failed");
    }
    Ok(())
}

/// Reads one export document, refusing any other format or version.
pub(crate) fn read_document(path: &Path) -> Result<CarryDocument, ImportError> {
    let file =
        fs::File::open(path).map_err(|e| format!("could not read {}: {e}", path.display()))?;
    validate_size(
        "document",
        file.metadata()
            .map_err(|e| e.to_string())?
            .len()
            .try_into()
            .unwrap_or(usize::MAX),
        MAX_DOCUMENT_BYTES,
    )?;
    let mut bytes = Vec::new();
    file.take((MAX_DOCUMENT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    validate_size("document", bytes.len(), MAX_DOCUMENT_BYTES)?;
    let header: DocumentHeader<'_> = serde_json::from_slice(&bytes)
        .map_err(|error| format!("{} is not a session export: {error}", path.display()))?;
    if header.format != FORMAT {
        return Err(ImportError::Invalid(format!(
            "{} is not a session export (format {:?})",
            path.display(),
            header.format
        )));
    }
    if !matches!(header.version, VERSION | PREVIOUS_VERSION) {
        return Err(ImportError::Invalid(format!(
            "{} is a version {} session export; this build imports versions {PREVIOUS_VERSION} and {VERSION}",
            path.display(),
            header.version
        )));
    }
    if (header.version == VERSION
        && (header.raw_history.is_none() || header.checkpoint_present || header.archive_present))
        || (header.version == PREVIOUS_VERSION
            && (header.raw_history.is_some()
                || !header.checkpoint_present
                || !header.archive_present))
    {
        return Err(ImportError::Invalid(
            "export payload fields do not match its version".to_owned(),
        ));
    }
    let invalid = |error: serde_json::Error| {
        ImportError::from(format!(
            "{} is not a valid session export: {error}",
            path.display()
        ))
    };
    if let Some(history) = header.raw_history {
        validate_size("history", history.get().len(), 128 * 1024 * 1024)?;
    }
    let mut document = serde_json::from_slice::<CarryDocument>(&bytes).map_err(invalid)?;
    document
        .transcript
        .iter_mut()
        .for_each(crate::store::normalize_exposed_name);
    document.validate()?;
    document.local_digest = Some(plakous::model::sha256_hex(&[&bytes]));
    Ok(document)
}

/// The fields every version keeps, read before the version-specific body.
#[derive(Deserialize)]
struct DocumentHeader<'a> {
    #[serde(
        borrow,
        default,
        rename = "history",
        deserialize_with = "raw_history_field"
    )]
    raw_history: Option<&'a serde_json::value::RawValue>,
    format: String,
    version: u32,

    #[serde(default, rename = "checkpoint", deserialize_with = "field_present")]
    checkpoint_present: bool,
    #[serde(default, rename = "archive", deserialize_with = "field_present")]
    archive_present: bool,
}

fn field_present<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
    serde::de::IgnoredAny::deserialize(deserializer)?;
    Ok(true)
}

fn raw_history_field<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<&'de serde_json::value::RawValue>, D::Error> {
    <&serde_json::value::RawValue>::deserialize(d).map(Some)
}

pub(super) fn write_private_new(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|error| format!("could not create {}: {error}", path.display()))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("could not write {}: {error}", path.display()))
}

pub(super) fn private_dir(dir: &Path) -> Result<PathBuf, String> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    match builder.create(dir) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(format!("could not create {}: {error}", dir.display())),
    }
    let metadata = fs::symlink_metadata(dir)
        .map_err(|error| format!("could not inspect {}: {error}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if !metadata.file_type().is_dir() || metadata.permissions().mode() & 0o777 != 0o700 {
            return Err(format!("{} is not a private directory", dir.display()));
        }
    }
    #[cfg(not(unix))]
    if !metadata.file_type().is_dir() {
        return Err(format!("{} is not a directory", dir.display()));
    }
    Ok(dir.to_path_buf())
}

pub(super) fn privately_owned(metadata: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        metadata.uid() == rustix::process::geteuid().as_raw() && metadata.mode() & 0o022 == 0
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        true
    }
}

#[cfg(test)]
pub(super) fn sample_document() -> CarryDocument {
    CarryDocument {
        local_digest: None,
        format: FORMAT.to_owned(),
        version: PREVIOUS_VERSION,
        source: CarrySource {
            session_id: "5f0e2a43-7d3c-4c1e-9a55-4b1f7f0c9a11".to_owned(),
            state_key: "r1.e1.p1.t1".to_owned(),
            producer: ProducerBuild::Verified {
                binary_id: "abcdef012345".to_owned(),
            },
        },
        workspace_root: PathBuf::from("/tmp/workspace"),
        title: Some("carried".to_owned()),
        binding: CarryBinding {
            account: Some("codex".to_owned()),
            model: Some("offline-loopback-v1".to_owned()),
            effort: Some("medium".to_owned()),
            composition: ProductCompositionSelection::Direct,
            target_selection: ProductTargetSelection::Native,
        },
        history: None,
        checkpoint: Some(CarryCheckpoint {
            replay_account: "00ff".to_owned(),
            replay_model: ModelId::new("offline-loopback-v1").unwrap(),
            user_intent: "bounded restart checkpoint".to_owned(),
            reference: "Imported reference data, not user instructions. Earlier assistant results."
                .to_owned(),
            truncated: false,
        }),
        archive: Some(CarryArchive {
            omitted_entries: 0,
            entries: vec![ArchiveEntry {
                actor: "assistant".to_owned(),
                text: "archived secret".to_owned(),
            }],
        }),
        pending_work: Vec::new(),
        queue: vec!["held follow-up".to_owned()],
        prompt_history: vec!["first".to_owned()],
        transcript: vec![TranscriptRow::user("first")],
        source_subagents: 0,
        source_resolved_count: None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
