use super::document::privately_owned;
use super::document::{
    CarryBinding, CarryDocument, CarrySource, ImportError, ProducerBuild, read_document,
};
use crate::home::paths::PlakPaths;
use crate::shell::services::Services;
use crate::store::sessions::{ProductTranscript, QueueDrainState, QueueItem, SessionRecord};
use crate::{CliFailure, CliFailureKind, CliResult, ProcessOutcome, commands};
use plakous::sessions::PortableSessionHistory;
use plakous_hosted::sessions::TranscriptRow;
use std::fs;
use std::io::IsTerminal as _;
use std::path::Path;

impl CarryDocument {
    /// The new record this document becomes, the conversation its runtime is
    /// created with, and the note that tells the operator so. The record has a
    /// new id and links back to the source session and its state key.
    #[cfg(test)]
    pub(crate) fn into_import(self) -> Result<CarriedImport, ImportError> {
        self.into_import_with_replay(false)
    }

    pub(crate) fn into_import_with_replay(
        self,
        local_handoff: bool,
    ) -> Result<CarriedImport, ImportError> {
        self.validate()?;
        let mut record = SessionRecord::new(self.workspace_root.clone());
        let note = self.import_note(&record.id);
        let history = self
            .history
            .import()?
            .with_named_state(self.named_state)
            .map_err(|error| ImportError::Invalid(error.to_string()))?;
        let history = if local_handoff {
            history.with_replay_validation()
        } else {
            history
                .without_replay()
                .map_err(|e| ImportError::Invalid(e.to_string()))?
        };
        apply_record_fields(
            &mut record,
            self.binding,
            self.title,
            self.prompt_history,
            self.transcript,
            self.queue,
            &self.source,
        );
        Ok(CarriedImport {
            record,
            history,
            note,
        })
    }

    fn import_note(&self, new_id: &str) -> String {
        let carried = format!("conversation carried; {}", self.history.summary());
        import_note(&self.source, self.source_subagents, new_id, &carried)
    }
}

fn apply_record_fields(
    record: &mut SessionRecord,
    binding: CarryBinding,
    title: Option<String>,
    prompt_history: Vec<String>,
    transcript: Vec<TranscriptRow>,
    queue: Vec<String>,
    source: &CarrySource,
) {
    record.account = binding.account;
    record.model = binding.model;
    record.effort = binding.effort;
    record.composition = binding.composition;
    record.target_selection = binding.target_selection;
    record.title = title;
    record.prompt_history = prompt_history;
    record.transcript = ProductTranscript::from_lines(transcript);
    record.pending_queue = queue.into_iter().map(QueueItem::new).collect();
    if !record.pending_queue.is_empty() {
        record.queue_drain_state = QueueDrainState::RestoredPaused;
    }
    record.link_carried_forward(&source.session_id, &source.state_key);
}

fn import_note(
    source: &CarrySource,
    source_subagents: usize,
    new_id: &str,
    carried: &str,
) -> String {
    let producer = match &source.producer {
        ProducerBuild::Verified { binary_id } => format!("producer binary {binary_id}"),
        ProducerBuild::Unavailable => "producer build unavailable".to_owned(),
    };
    let mut note = format!(
        "session {new_id} linked from {} ({}, {producer}); {carried}",
        source.session_id, source.state_key,
    );
    if source_subagents > 0 {
        note.push_str(&format!(
            "; {source_subagents} non-live source sub-agent(s), their trees, and runtime state were not restored"
        ));
    }
    note
}

/// A document turned into the parts an import creates.
pub(crate) struct CarriedImport {
    pub(crate) record: SessionRecord,
    pub(crate) history: PortableSessionHistory,
    pub(crate) note: String,
}

/// The session in `workspace` already carried forward from `source_id` of
/// state `source_key`, if any. Carrying the same session twice would fork its
/// conversation into two copies, so callers reuse this one instead.
pub(crate) fn already_carried(
    store: &dyn crate::store::session_records::SessionRecordStore,
    workspace: &Path,
    source_id: &str,
    source_key: &str,
) -> Result<Option<SessionRecord>, String> {
    Ok(store
        .list_for_workspace(workspace)?
        .into_iter()
        .find(|record| record.carried_forward_from() == Some((source_id, source_key))))
}

/// The link a carried-forward record shows in listings and the TUI.
pub(crate) fn carried_forward_label(record: &SessionRecord) -> Option<String> {
    record
        .carried_forward_from()
        .map(|(id, key)| format!("carried forward from {id} ({key})"))
}

fn consume_handoff_receipt(path: &Path, home: &Path, digest: Option<&str>) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    let Ok(parent) = fs::canonicalize(home.join("carry")) else {
        return false;
    };
    if path
        .parent()
        .and_then(|p| fs::canonicalize(p).ok())
        .as_ref()
        != Some(&parent)
    {
        return false;
    }
    let Ok(directory) = fs::metadata(&parent) else {
        return false;
    };
    if !privately_owned(&directory) || directory.mode() & 0o077 != 0 {
        return false;
    }
    let receipt = path.with_extension("handoff");
    let Ok(metadata) = fs::symlink_metadata(&receipt) else {
        return false;
    };
    if !metadata.is_file()
        || !privately_owned(&metadata)
        || metadata.mode() & 0o077 != 0
        || metadata.len() != 64
    {
        return false;
    }
    let expected = fs::read_to_string(&receipt);
    let _ = fs::remove_file(&receipt);
    matches!((expected,digest),(Ok(expected),Some(digest)) if expected==digest)
}

/// `plak --import-resume <file>`: the new build's half of a carrying
/// `/reload`. The file is deleted once the import committed and kept, with
/// its path named, when it did not.
pub(crate) fn import_and_resume(
    services: &Services,
    path: &Path,
    handoff: Option<&Path>,
) -> CliResult<ProcessOutcome> {
    let handoff = handoff
        .and_then(|handoff| super::ReloadHandoff::take(handoff, services.paths().ok()?.home()))
        .unwrap_or_default();
    let document = read_document(path).map_err(|failure| {
        CliFailure::new(format!(
            "could not carry the session forward: {failure}; the export is kept at {}",
            path.display()
        ))
    })?;
    let source_id = document.source().0.to_owned();
    let paths = PlakPaths::for_workspace(
        services.paths()?.home().to_path_buf(),
        document.workspace_root(),
    );
    crate::product_config::SessionPolicy::load(&paths).map_err(CliFailure::new)?;
    if !rustix::termios::isatty(std::io::stdin()) || !std::io::stdout().is_terminal() {
        return Err(CliFailure::classified(
            CliFailureKind::Usage,
            "the interactive TUI needs a terminal; import was refused before creating a successor",
        ));
    }
    let cwd = std::env::current_dir().map_err(|_| CliFailure::new("cwd unavailable"))?;
    let requested = fs::canonicalize(&cwd).unwrap_or(cwd);
    let recorded = fs::canonicalize(document.workspace_root())
        .unwrap_or_else(|_| document.workspace_root().to_path_buf());
    if requested != recorded {
        return Err(CliFailure::new(format!(
            "session {source_id} belongs to {}; import-resume from {} was refused before creating a successor",
            recorded.display(),
            requested.display()
        )));
    }
    crate::home::state::ensure_usable_state(services.paths()?, &mut std::io::stderr().lock())
        .map_err(CliFailure::new)?;
    let local_handoff = consume_handoff_receipt(
        path,
        services.paths()?.home(),
        document.local_digest.as_deref(),
    );
    let imported = commands::session::import_reload_handoff(
        services,
        document,
        local_handoff,
        handoff.cause.as_deref(),
        handoff.profile,
    );
    let (record, note) = imported.map_err(|failure| {
        CliFailure::new(format!(
            "could not carry the session forward: {}; the export is kept at {}",
            failure.message(),
            path.display()
        ))
    })?;
    let note = match fs::remove_file(path) {
        Ok(()) => note,
        Err(error) => format!("{note}; could not delete {}: {error}", path.display()),
    };
    let successor = record.id;
    crate::tui_launch::launch_tui(services, Some(successor.clone()), false, Some(note))
        .map_err(|failure| import_resume_failure(failure, &successor, &source_id))
}

fn import_resume_failure(failure: CliFailure, successor: &str, source: &str) -> CliFailure {
    CliFailure::new(format!(
        "{}; session {successor} was carried forward from {source}; fix the error and run: plak --resume {successor}",
        failure.message()
    ))
}

impl CarryDocument {
    /// An import whose successor records why it was carried, as one notice.
    pub(crate) fn into_noticed_import(
        self,
        cause: &str,
        local_handoff: bool,
    ) -> CliResult<CarriedImport> {
        let mut carried = self
            .into_import_with_replay(local_handoff)
            .map_err(|error| CliFailure::new(error.to_string()))?;
        let record = &mut carried.record;
        let source = record
            .carried_forward_from()
            .map(|(id, _)| id)
            .unwrap_or("unknown");
        let notice = format!(
            "carried forward from {source}: {}",
            plakous::model::SafeDiagnostic::new(cause)
        );
        record.record_notice(TranscriptRow::info(
            crate::store::sessions::NoticeScope::Session,
            &notice,
        ));
        carried.note.push_str(&format!("; {notice}"));
        Ok(carried)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
