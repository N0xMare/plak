//! Stored attach facts are product-only reads, not runtime ownership.

#[cfg(test)]
use super::Services;
use super::{
    NodeAddress, PlakExtension, ProductSessions, ProductStore, RequestRefusal, runner_views,
};
use plakous_host_protocol::SessionObservation;
use plakous_host_protocol::views::SessionInfoView;
use plakous_hosted::host_service::wire::StoredSessionView;

impl ProductSessions {
    pub(super) fn read_stored(
        &self,
        id: &str,
        include_unarchived: bool,
    ) -> Result<Option<StoredSessionView<PlakExtension>>, RequestRefusal> {
        let unavailable = |detail| RequestRefusal::SessionUnavailable { detail };
        let paths = self
            .services
            .paths()
            .map_err(|failure| unavailable(failure.message().to_owned()))?;
        let record = ProductStore::inspect_session_read_only(paths, id).map_err(unavailable)?;
        if !record.archived && !include_unarchived {
            return Ok(None);
        }
        let root = NodeAddress {
            tree_id: record.node.tree_id.clone(),
            agent_id: record.node.agent_id.clone(),
        };
        let runner_id = record
            .runner
            .as_ref()
            .map_or("local", |runner| runner.runner_id.as_str());
        let info = SessionInfoView {
            runner: runner_views::label(runner_id, &self.services.runners().rows()),
            session_id: record.id.clone(),
            host_version: plakous::VERSION.to_owned(),
            working_dir: record.workspace_root.display().to_string(),
            root_template: record
                .root_profile
                .as_ref()
                .and_then(|profile| profile.root_template.clone()),
            root_model: record.model.clone(),
            root_effort: record.effort.clone(),
            ..Default::default()
        };
        let snapshot = plakous_hosted::runtime::session_restore_snapshot(&record);
        Ok(Some(StoredSessionView {
            archived: record.archived,
            session: record.id,
            root,
            observations: vec![
                SessionObservation::SessionInfo(info),
                SessionObservation::PresentationSnapshot(snapshot),
            ],
        }))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "read-only archived fixture")]
mod tests {
    use super::*;

    #[test]
    fn stored_projection_reads_product_rows_without_opening_runtime_or_record_services() {
        for archived in [false, true] {
            let (root, paths) = crate::home::paths::PlakPaths::isolated_test_home();
            let mut record =
                crate::store::sessions::SessionRecord::new(root.path().join("workspace"));
            record.archived = archived;
            record.title_override = Some("operator title".into());
            record.record_notice(plakous_hosted::sessions::TranscriptRow::info(
                plakous_hosted::sessions::NoticeScope::Session,
                "persisted fact",
            ));
            let mut store = ProductStore::open(&paths).unwrap();
            store.commit(&mut record).unwrap();
            drop(store);
            let services = Services::for_home(paths.clone());
            let product = ProductSessions::new(&services, None, None).unwrap();
            assert_eq!(services.product_store_opens(), 0);
            assert_eq!(
                product.read_stored(&record.id, false).unwrap().is_some(),
                archived
            );
            let view = product.read_stored(&record.id, true).unwrap().unwrap();
            assert_eq!(view.session, record.id);
            assert_eq!(view.archived, archived);
            assert!(view.observations.iter().any(|event| matches!(event,
            SessionObservation::PresentationSnapshot(snapshot) if serde_json::to_string(snapshot).unwrap().contains("persisted fact"))));
            assert_eq!(services.product_store_opens(), 0);
            assert!(!paths.runtime_database().exists());
        }
    }
}
