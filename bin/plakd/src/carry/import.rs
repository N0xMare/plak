use super::document::{CarryBinding, CarryDocument, CarrySource, ImportError, ProducerBuild};
use crate::shell::services::Services;
use crate::store::sessions::{ProductTranscript, QueueDrainState, QueueItem, SessionRecord};
use plakous::sessions::PortableSessionHistory;
use plakous_hosted::sessions::TranscriptRow;
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
        mut self,
        local_handoff: bool,
    ) -> Result<CarriedImport, ImportError> {
        self.validate()?;
        let profile = self
            .profile
            .take()
            .map(|profile| crate::profiles::SessionSelection {
                root: profile.root_template,
                working_set: profile.working_set,
            });
        let mut record = SessionRecord::new(self.workspace_root.clone());
        record.runner = self
            .runner
            .as_ref()
            .filter(|runner| !runner.is_local())
            .map(|runner| plakous_hosted::sessions::RunnerDisplay {
                name: runner.name.clone(),
                runner_id: runner.runner_id.clone(),
                workspace_root: self.workspace_root.clone(),
                workspace_cwd: self.workspace_root.clone(),
            });
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
            profile,
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
    /// The profile the document names; `None` only for callers without a saved profile,
    /// which imports under the home default.
    pub(crate) profile: Option<crate::profiles::SessionSelection>,
}

/// The session in `workspace` already carried forward from `source_id` of
/// state `source_key`, if any. Carrying the same session twice would fork its
/// conversation into two copies, so callers reuse this one instead.
pub(crate) fn already_carried(
    services: &Services,
    workspace: &Path,
    runner_id: &str,
    source_id: &str,
    source_key: &str,
) -> Result<Option<SessionRecord>, String> {
    let paths = services
        .paths()
        .map_err(|failure| failure.message().to_owned())?;
    crate::store::ProductStore::open(paths)
        .map_err(|error| error.to_string())?
        .carried_forward(workspace, runner_id, source_id, source_key)
}

/// The link a carried-forward record shows in listings and external clients.
pub(crate) fn carried_forward_label(record: &SessionRecord) -> Option<String> {
    record
        .carried_forward_from()
        .map(|(id, key)| format!("carried forward from {id} ({key})"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
