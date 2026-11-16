//! An explicit abandonment leaves ordinary duplicate-copy exports, not an executable move.
use super::Capsule;
use crate::carry::document::{
    CarryBinding, CarryDocument, CarryRunner, CarrySource, DocumentKind, PendingWorkEntry,
    ProducerBuild,
};
use crate::home::paths::PlakPaths;
use crate::{CliFailure, CliResult};

impl Capsule {
    pub(crate) fn write_backups(
        &self,
        paths: &PlakPaths,
        directory: &std::path::Path,
    ) -> CliResult<()> {
        for document in &self.products {
            let (record, _, _) = document
                .clone()
                .into_cut()
                .map_err(CliFailure::new)?
                .into_parts();
            let head = self
                .sessions
                .iter()
                .flat_map(|session| &session.heads)
                .find(|head| head.source == document.source().to_string())
                .ok_or_else(|| CliFailure::new("backup product has no represented source head"))?;
            let children = self
                .trees
                .iter()
                .filter(|tree| tree.cut.tree_id() == record.node.tree_id)
                .flat_map(|tree| tree.cut.nodes().values())
                .filter(|node| node.parent_agent_id.is_some())
                .collect::<Vec<_>>();
            let portable =
                CarryDocument {
                    local_digest: None,
                    format: super::super::document::FORMAT.into(),
                    version: super::super::VERSION,
                    kind: Some(DocumentKind::PortableSession),
                    source: CarrySource {
                        session_id: record.id.clone(),
                        state_key: self.source_key.clone(),
                        producer: ProducerBuild::Verified {
                            binary_id: self.source_binary.clone(),
                        },
                    },
                    workspace_root: record.workspace_root.clone(),
                    runner: Some(record.runner.as_ref().map_or_else(
                        CarryRunner::local,
                        |runner| CarryRunner {
                            name: runner.name.clone(),
                            runner_id: runner.runner_id.clone(),
                        },
                    )),
                    profile: Some(
                        super::super::export::session_profile(paths, &record)
                            .map_err(|error| CliFailure::new(error.to_string()))?,
                    ),
                    title: record.title.clone(),
                    binding: CarryBinding {
                        account: record.account.clone(),
                        model: record.model.clone(),
                        effort: record.effort.clone(),
                        composition: record.composition,
                        target_selection: record.target_selection.clone(),
                    },
                    named_state: head.named_state.clone(),
                    history: serde_json::from_value(
                        serde_json::to_value(&head.history)
                            .map_err(|error| CliFailure::new(error.to_string()))?,
                    )
                    .map_err(|error| CliFailure::new(error.to_string()))?,
                    pending_work: children
                        .iter()
                        .map(|node| PendingWorkEntry {
                            operation: node.agent_id.clone(),
                            kind: "source_child".into(),
                            state: format!("{:?}", node.status),
                            detail: Some(node.description.clone()),
                        })
                        .collect(),
                    queue: record
                        .pending_queue
                        .iter()
                        .map(|item| item.text.clone())
                        .collect(),
                    prompt_history: record.prompt_history.clone(),
                    transcript: record.transcript.to_vec(),
                    source_subagents: children.len(),
                    source_resolved_count: None,
                };
            super::super::write_document(&directory.join(format!("{}.json", record.id)), &portable)
                .map_err(CliFailure::new)?;
        }
        Ok(())
    }
}
