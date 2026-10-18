#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]

use super::*;

#[test]
fn reload_handoff_reuses_the_tui_probe_verdict_without_reprobing() {
    let root = tempfile::tempdir().unwrap();
    let paths = product_paths::PlakPaths::explicit(root.path().join("home"));
    let mut record = product_sessions::SessionRecord::new(root.path().to_path_buf());
    let key = home::state::state_key();
    let target = carry::ReloadTarget {
        executable: root.path().join("absent-plak"),
        state_key: Some(key.clone()),
        probe: Some(carry::ReloadProbe {
            state_key: key,
            export_versions: carry::IMPORTABLE_VERSIONS.to_vec(),
            config_error: Some("synthetic config refusal".to_owned()),
        }),
    };
    let note = reload_installed_plak(
        &paths,
        &store::session_records::SqliteSessionStore::new(paths.clone()),
        &mut record,
        Some(target),
    )
    .unwrap();
    assert!(note.contains("synthetic config refusal"), "{note}");
    assert!(!paths.runtime_database().exists());
}

/// A piped `plak` keeps exit 2 but must hand the reader a command, not a
/// refusal. Both forms of that command are pinned here.
#[test]
fn the_non_terminal_refusal_names_the_exact_command_to_run() {
    assert_eq!(headless_run_command_for(true), "plak run -- \"<prompt>\"");
    assert_eq!(
        headless_run_command_for(false),
        "plak run --account <codex|grok|claude|openrouter|openai|anthropic|xai-api> --model <model> -- \"<prompt>\""
    );
}

#[test]
fn exact_resume_launch_is_read_only_until_runtime_attachment() {
    let root = tempfile::tempdir().expect("temporary product home");
    let paths = product_paths::PlakPaths::explicit(root.path().join("product"));
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace directory");
    let mut session = product_sessions::SessionRecord::new(workspace.clone());
    let id = session.id.clone();
    product_sessions::save_session(&paths, &mut session).expect("persist resume source");
    let before = product_sessions::load_session(&paths, &id).expect("load resume source");
    let revision = before.store_revision;
    let updated_at = before.updated_at_unix;
    let mut candidate = before;

    assert!(begin_tui_session_launch(
        &paths,
        &mut candidate,
        true,
        &workspace
    ));

    let after = product_sessions::load_session(&paths, &id).expect("reload resume source");
    assert_eq!(after.store_revision, revision);
    assert_eq!(after.updated_at_unix, updated_at);
    assert!(
        product_session_log::read_tail(&paths.session_home(), &id, 8)
            .expect("read observation tail")
            .iter()
            .all(|line| !line.contains("session.resumed"))
    );
}

#[test]
fn fresh_tui_launch_remains_unpublished_until_user_intent() {
    let root = tempfile::tempdir().expect("temporary product home");
    let paths = product_paths::PlakPaths::explicit(root.path().join("product"));
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace directory");
    let mut candidate = product_sessions::SessionRecord::new(workspace.clone());
    let id = candidate.id.clone();

    assert!(!begin_tui_session_launch(
        &paths,
        &mut candidate,
        false,
        &workspace
    ));
    assert_eq!(candidate.store_revision, 0);
    assert!(product_sessions::load_session(&paths, &id).is_err());
    assert_eq!(
        product_session_log::read_tail(&paths.session_home(), &id, 8)
            .expect("missing observation log is empty"),
        [] as [String; 0]
    );
}

/// "Edit raw config" returned from `$EDITOR`
/// bound to an earlier session, discarding a rebind, a `/clear`, and every
/// turn after it. `/clear` replaces the attached session inside the host;
/// the launcher's own record still names the session the process started
/// on, so every terminal re-entry adopts the host-authoritative identity
/// the presentation carried out with it.
#[test]
fn a_terminal_handoff_reenters_on_the_host_authoritative_session() {
    let root = tempfile::tempdir().unwrap();
    let paths = product_paths::PlakPaths::explicit(root.path().join("home"));
    paths.ensure_layout().unwrap();
    let cwd = root.path().join("workspace");
    std::fs::create_dir_all(&cwd).unwrap();

    let mut launched = product_sessions::SessionRecord::new(cwd.clone());
    product_sessions::save_session(&paths, &mut launched).unwrap();
    // What `/clear` produced inside the host while the TUI was attached.
    let mut cleared = product_sessions::SessionRecord::new(cwd.clone());
    product_sessions::save_session(&paths, &mut cleared).unwrap();
    assert_ne!(launched.id, cleared.id);

    let mut record = launched.clone();
    let mut resume = true;
    let mut resume_pending = true;
    let mut former = Some((launched.clone(), true, true));

    adopt_attached_session(
        &store::session_records::SqliteSessionStore::new(paths.clone()),
        &mut record,
        &mut resume,
        &mut resume_pending,
        &mut former,
        Some(cleared.id.as_str()),
    )
    .unwrap();

    assert_eq!(
        record.id, cleared.id,
        "the editor must return to /clear's session"
    );
    assert!(
        !resume,
        "the adopted attachment is live, not an exact resume"
    );
    assert!(!resume_pending);
    assert!(
        former.is_none(),
        "a pre-resume rollback source cannot outlive a different attachment"
    );
}

/// The adoption is inert when the host is still on the launcher's session,
/// so it never disturbs a pending exact resume that has not been published.
#[test]
fn adopting_the_same_session_leaves_a_pending_resume_intact() {
    let root = tempfile::tempdir().unwrap();
    let paths = product_paths::PlakPaths::explicit(root.path().join("home"));
    paths.ensure_layout().unwrap();
    let cwd = root.path().join("workspace");
    std::fs::create_dir_all(&cwd).unwrap();
    let mut attached = product_sessions::SessionRecord::new(cwd);
    product_sessions::save_session(&paths, &mut attached).unwrap();

    let mut record = attached.clone();
    let mut resume = true;
    let mut resume_pending = true;
    let mut former = Some((attached.clone(), true, true));
    for reported in [Some(attached.id.as_str()), None, Some("")] {
        adopt_attached_session(
            &store::session_records::SqliteSessionStore::new(paths.clone()),
            &mut record,
            &mut resume,
            &mut resume_pending,
            &mut former,
            reported,
        )
        .unwrap();
        assert_eq!(record.id, attached.id);
        assert!(resume && resume_pending && former.is_some());
    }
}

/// The exit line names the picker's short id and the workspace, so the
/// operator can paste it from any directory and know where it lands.
#[test]
fn resume_exit_line_names_the_short_id_and_the_workspace() {
    let line = resume_exit_line(
        "1a3c1adc-22f2-4b89-afc6-784807a2f1a5",
        Path::new("/home/op/project"),
    );
    assert_eq!(
        line,
        "Resume this session with: plak resume 1a3c1adc  (workspace /home/op/project)"
    );
    assert!(
        !line.contains("1a3c1adc-22f2"),
        "the hint is the short id, not the full uuid"
    );
}

#[test]
fn combine_cleanup_preserves_the_worker_failure() {
    let error =
        combine_cleanup(Err(CliFailure::new("release checkpoint refused")), Ok(())).unwrap_err();
    assert_eq!(error.message(), "release checkpoint refused");
}

#[test]
fn combine_cleanup_fails_a_successful_round_on_restoration_failure() {
    let error = combine_cleanup(Ok(()), Err(CliFailure::new("raw mode refused"))).unwrap_err();
    assert_eq!(error.message(), "raw mode refused");
}

#[test]
fn combine_cleanup_retains_both_failure_reasons() {
    let error = combine_cleanup(
        Err(CliFailure::new("release checkpoint refused")),
        Err(CliFailure::new("raw mode refused")),
    )
    .unwrap_err();
    assert!(
        error.message().contains("release checkpoint refused")
            && error.message().contains("raw mode refused"),
        "{}",
        error.message()
    );
}
