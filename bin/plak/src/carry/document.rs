//! Export document types, version, and encoding.

use super::history;
use crate::product_config::{ProductCompositionSelection, ProductTargetSelection};
use plakous_host_protocol::views::presentation::row_bytes;
use plakous_hosted::sessions::{
    MAX_TRANSCRIPT_BYTES, MAX_TRANSCRIPT_LINES, RootProfile, TranscriptRow,
};
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
pub(crate) const VERSION: u32 = 12;
const PREVIOUS_VERSION: u32 = 11;
pub(crate) const IMPORTABLE_VERSIONS: [u32; 2] = [PREVIOUS_VERSION, VERSION];
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
    /// The runner the session's tools ran on. Version 12 names it; a version
    /// 11 document has none and imports onto the local runner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) runner: Option<CarryRunner>,
    /// The session's profile: its tree's when it has rows, otherwise its
    /// record's, otherwise the home default it ran under. Version 12 names
    /// it; a version 11 document has none and imports under the
    /// destination's home default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) profile: Option<RootProfile>,
    pub(super) title: Option<String>,
    pub(super) binding: CarryBinding,
    /// Committed session-owned state, shared by both importable versions.
    pub(super) named_state: std::collections::BTreeMap<String, serde_json::Value>,
    pub(super) history: history::History,
    /// Unresolved source workflow facts retained as inert operator context.
    /// They are never restored into the new session's executable journal.
    pub(super) pending_work: Vec<PendingWorkEntry>,
    /// Queued prompts, oldest first. A prompt already handed to the runtime is
    /// never queued here: export refuses while one is unresolved.
    pub(super) queue: Vec<String>,
    pub(super) prompt_history: Vec<String>,
    /// Typed product transcript rows, oldest first, with their semantic
    /// notice remedies.
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

/// The runner a session's tools ran on, by its configured name and stable
/// identity.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CarryRunner {
    pub(super) name: String,
    pub(super) runner_id: String,
}

impl CarryRunner {
    /// The host's own local runner.
    pub(super) fn local() -> Self {
        let local = plakous::tools::RunnerId::local();
        Self {
            name: local.as_str().to_owned(),
            runner_id: local.as_str().to_owned(),
        }
    }

    fn validate(&self) -> Result<(), ImportError> {
        let visible = |value: &str| {
            (1..=64).contains(&value.len())
                && !value
                    .chars()
                    .any(|character| character.is_whitespace() || character.is_control())
        };
        if !visible(&self.name) || plakous::tools::RunnerId::new(self.runner_id.clone()).is_err() {
            return Err(ImportError::Invalid(
                "$.runner: export runner is not a runner name and identity".to_owned(),
            ));
        }
        Ok(())
    }

    /// Whether this is the host-local placement.
    pub(super) fn is_local(&self) -> bool {
        plakous::tools::RunnerId::new(self.runner_id.clone()).is_ok_and(|runner| runner.is_local())
    }
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
        for (field, present) in [
            ("runner", self.runner.is_some()),
            ("profile", self.profile.is_some()),
        ] {
            let mismatch = match (self.version, present) {
                (VERSION, true) | (PREVIOUS_VERSION, false) => continue,
                (VERSION, false) => format!("version {VERSION} requires `{field}` at $.{field}"),
                (PREVIOUS_VERSION, true) => {
                    format!("version {PREVIOUS_VERSION} has no `{field}`, but one is at $.{field}")
                }
                (version, _) => format!("version {version} is not importable"),
            };
            return Err(ImportError::Invalid(format!(
                "export payload does not match its version: {mismatch}"
            )));
        }
        if let Some(runner) = &self.runner {
            runner.validate()?;
        }
        if let Some(profile) = &self.profile {
            profile
                .encode()
                .map_err(|error| ImportError::Invalid(format!("$.profile: {error}")))?;
        }
        self.history.validate_size()
    }

    /// The session id and state key this document was exported from.
    pub(crate) fn source(&self) -> (&str, &str) {
        (&self.source.session_id, &self.source.state_key)
    }

    /// The workspace the exported session belonged to.
    pub(crate) fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    /// Saved placement identity; version 11 has no runner and is local.
    pub(crate) fn runner_id(&self) -> &str {
        self.runner
            .as_ref()
            .map_or("local", |runner| runner.runner_id.as_str())
    }

    pub(crate) fn message_count(&self) -> usize {
        self.history.message_count()
    }

    pub(crate) fn estimated_tokens(&self) -> u64 {
        self.history.estimated_tokens()
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
    // The CLI edge made every operator path absolute; a relative one here
    // would resolve against the process working directory.
    if path.is_relative() {
        return Err(format!(
            "the export path {} is not absolute",
            path.display()
        ));
    }
    let parent = path
        .parent()
        .ok_or_else(|| format!("the export path {} names no directory", path.display()))?;
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
    if header.raw_history.is_none() || header.checkpoint_present || header.archive_present {
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
        version: VERSION,
        source: CarrySource {
            session_id: "5f0e2a43-7d3c-4c1e-9a55-4b1f7f0c9a11".to_owned(),
            state_key: "r1.e1.p1.t1".to_owned(),
            producer: ProducerBuild::Verified {
                binary_id: "abcdef012345".to_owned(),
            },
        },
        workspace_root: PathBuf::from("/tmp/workspace"),
        runner: Some(CarryRunner::local()),
        profile: Some(RootProfile {
            root_template: Some("lead".to_owned()),
            working_set: vec!["review".to_owned()],
        }),
        title: Some("carried".to_owned()),
        binding: CarryBinding {
            account: Some("codex".to_owned()),
            model: Some("offline-loopback-v1".to_owned()),
            effort: Some("medium".to_owned()),
            composition: ProductCompositionSelection::Direct,
            target_selection: ProductTargetSelection::Native,
        },
        history: serde_json::from_value(serde_json::json!({"replay_account":"00ff","replay_model":"offline-loopback-v1","groups":[]})).unwrap(),
        named_state: std::collections::BTreeMap::new(),
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
