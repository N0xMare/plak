//! Product policy composed around the hosted session: what the product does
//! beside the host loop rather than inside it.

pub mod binding_document;
pub mod catalog_refresh;
pub mod home;
pub mod host;
pub mod services;
pub mod settings;

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    //! The product half of the detached shell host's `/clear`.
    //!
    //! The hosted crate owns `commit_detached_clear` and its refusal path; what
    //! is product — and so what is pinned here — is that the shell host running
    //! over Plak's own SQLite store publishes the new product session durably
    //! before anything observes it.

    use std::sync::Arc;

    use crate::home::paths::PlakPaths;
    use crate::shell::settings::ProductSurface;
    use crate::store::sessions::{self, SessionRecord};
    use crate::surface_protocol::UserAction;
    use plakous_hosted::runtime::ShellHostConfig;

    fn durable_authority_count(database: &std::path::Path, table: &str) -> i64 {
        if !database.exists() {
            return 0;
        }
        let connection = rusqlite::Connection::open(database).unwrap();
        connection
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap()
    }

    #[test]
    fn detached_clear_publishes_only_after_durable_save() {
        let root = tempfile::tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("home"));
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let mut source = SessionRecord::new(workspace.clone());
        source.account = Some("codex".into());
        source.model = Some("gpt-test".into());
        source.effort = Some("high".into());
        sessions::save_session(&paths, &mut source).unwrap();
        let source_id = source.id.clone();
        let source_revision = source.store_revision;

        let (actions, _events, worker) = crate::shell::host::spawn_shell_host(ShellHostConfig {
            surface: Some(ProductSurface::boxed(&paths)),
            binding: crate::shell::binding_document::ProductRootBindingDocument::shared(&paths),
            subscriptions: crate::shell::host::open_product_subscriptions(),
            openrouter_credentials: crate::openrouter_product::ProductOpenRouterCredentials::shared(
                &paths,
            ),
            product_home: paths.home().to_path_buf(),
            home: paths.session_home(),
            store: Arc::new(crate::store::session_records::SqliteSessionStore::new(
                paths.clone(),
            )),
            session: source,
            cwd: workspace.clone(),
            reason: "detached fixture".into(),
            ready_catalog: plakous_host_protocol::views::ReadyCatalog::default(),
        });
        actions.send(UserAction::ClearSession).unwrap();
        actions.send(UserAction::Quit).unwrap();
        worker.join().unwrap().unwrap();

        let records = sessions::list_sessions_for_cwd(&paths, &workspace).unwrap();
        assert_eq!(records.len(), 2);
        let durable_source = sessions::load_session(&paths, &source_id).unwrap();
        assert_eq!(durable_source.store_revision, source_revision);
        let Some(target) = records.into_iter().find(|record| record.id != source_id) else {
            panic!("detached clear must durably publish one new product session");
        };
        let durable_target = sessions::load_session(&paths, &target.id).unwrap();
        assert!(durable_target.store_revision > 0);
        assert_eq!(durable_target.account.as_deref(), Some("codex"));
        assert_eq!(durable_target.model.as_deref(), Some("gpt-test"));
        assert_eq!(durable_target.effort.as_deref(), Some("high"));
        assert_eq!(
            durable_authority_count(&paths.runtime_database(), "sessions"),
            0
        );
    }
}
