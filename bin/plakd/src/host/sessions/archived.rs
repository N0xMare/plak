//! Stored attach facts are product-only reads, not runtime ownership.

#[cfg(test)]
use super::Services;
use super::{
    NodeAddress, PlakExtension, ProductSessions, ProductStore, RequestRefusal, runner_views,
};
use plakous_host_protocol::SessionObservation;
use plakous_host_protocol::views::SessionInfoView;
use plakous_hosted::host_service::wire::StoredSessionView;
use plakous_hosted::runtime::SurfaceSession;

impl ProductSessions {
    pub(super) fn read_stored(
        &self,
        id: &str,
        include_unarchived: bool,
        presentation: bool,
    ) -> Result<Option<StoredSessionView<PlakExtension>>, RequestRefusal> {
        let unavailable = |detail| RequestRefusal::SessionUnavailable { detail };
        let paths = self
            .services
            .paths()
            .map_err(|failure| unavailable(failure.message().to_owned()))?;
        if !include_unarchived
            && !self
                .with_product(|store| store.archive_flag(id))
                .map_err(unavailable)?
        {
            return Ok(None);
        }
        // A paged viewport supplies the transcript: read only the row.
        let record = if presentation {
            ProductStore::inspect_session_read_only(paths, id)
        } else {
            ProductStore::inspect_session_metadata_read_only(paths, id)
        }
        .map_err(|error| unavailable(error.into()))?;
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
        let snapshot =
            presentation.then(|| plakous_hosted::runtime::session_restore_snapshot(&record));
        let context = if record.runner.is_some() {
            paths.for_remote_workspace(&record.workspace_root)
        } else {
            paths.for_workspace(&record.workspace_root)
        };
        let profile = crate::shell::settings::session_profile_for(
            &context,
            &SurfaceSession {
                runner: record.runner,
                id: record.id.clone(),
                account: record.account,
                model: record.model,
                effort: record.effort,
                service_tier: record
                    .target_selection
                    .native_service_tier()
                    .map(str::to_owned),
                workspace: record.workspace_root,
                roster: None,
                root_template: info.root_template.clone(),
                live_children: false,
                mcp: None,
                account_choices: record.account_choices,
            },
        );
        let mut observations = vec![
            SessionObservation::SessionInfo(info),
            SessionObservation::Extension(
                crate::surface_protocol::SurfaceObservation::SessionProfile(profile),
            ),
        ];
        observations.extend(snapshot.map(SessionObservation::PresentationSnapshot));
        Ok(Some(StoredSessionView {
            archived: record.archived,
            session: record.id,
            root,
            observations,
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
            // Open the shared metadata handle, as the real host's startup scan does.
            product.with_product(|_| Ok(())).unwrap();
            assert_eq!(services.product_store_opens(), 0);
            let database = paths.product_database();
            let opens = crate::store::open_counts::get(&database);
            assert_eq!(
                product
                    .read_stored(&record.id, false, true)
                    .unwrap()
                    .is_some(),
                archived
            );
            assert_eq!(
                crate::store::open_counts::get(&database) - opens,
                usize::from(archived),
                "an unarchived attach must not open a stored-history connection"
            );
            let view = product
                .read_stored(&record.id, true, true)
                .unwrap()
                .unwrap();
            assert_eq!(view.session, record.id);
            assert_eq!(view.archived, archived);
            assert!(view.observations.iter().any(|event| matches!(event,
            SessionObservation::PresentationSnapshot(snapshot) if serde_json::to_string(snapshot).unwrap().contains("persisted fact"))));
            let paged = product
                .read_stored(&record.id, true, false)
                .unwrap()
                .unwrap();
            assert_eq!(
                (paged.session, paged.archived),
                (record.id.clone(), archived)
            );
            assert!(
                !paged
                    .observations
                    .iter()
                    .any(|event| matches!(event, SessionObservation::PresentationSnapshot(_))),
                "a paged saved read must not build the whole presentation"
            );
            assert_unrecorded_root_template_is_null(view.observations);
            assert_eq!(services.product_store_opens(), 0);
            assert!(!paths.runtime_database().exists());
        }
    }

    fn assert_unrecorded_root_template_is_null(
        observations: Vec<SessionObservation<PlakExtension>>,
    ) {
        use plakous_host_protocol::wire::HostFrame;

        let observation = observations
            .into_iter()
            .find(|event| {
                matches!(
                    event,
                    SessionObservation::Extension(
                        crate::surface_protocol::SurfaceObservation::SessionProfile(_)
                    )
                )
            })
            .unwrap();
        let frame = HostFrame::Event {
            attachment: 1,
            seq: 1,
            class: observation.class(),
            observation,
            client_id: None,
            blobs: Vec::new(),
        };
        let encoded = crate::host::protocol_13::encode(frame, |_| Vec::new())
            .unwrap()
            .unwrap();
        let json: serde_json::Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(
            json.pointer("/observation/body/session_profile/root/template"),
            Some(&serde_json::Value::Null)
        );
    }
}
