#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]

use super::*;

#[test]
fn reload_applies_orchestration_in_the_installed_build_without_a_second_import() {
    let executable = Path::new("/new-build/plak");
    for (installed_key, applies_on_resume) in [("same", true), ("other", false)] {
        let plan = carry::reload_plan(true, "same", Some(installed_key), false);
        let command = reload_command(executable, &plan, "source-session");
        assert_eq!(command.get_program(), executable.as_os_str());
        let marker = command
            .get_envs()
            .find(|(key, _)| *key == super::super::RELOAD_EPOCH_ENV)
            .unwrap()
            .1;
        assert_eq!(
            marker,
            applies_on_resume.then_some(std::ffi::OsStr::new("1"))
        );
        let args: Vec<_> = command.get_args().collect();
        if applies_on_resume {
            assert_eq!(args, ["--resume", "source-session"]);
        } else {
            assert!(
                args.is_empty(),
                "cross-state reload imports a fresh genesis, not --resume"
            );
        }
    }
}

#[test]
fn reload_handoff_reuses_the_tui_probe_verdict_without_reprobing() {
    let root = tempfile::tempdir().unwrap();
    let paths = product_paths::PlakPaths::explicit(root.path().join("home"));
    let mut record = product_sessions::SessionRecord::new(root.path().to_path_buf());
    let key = home::state::state_key();
    let target = carry::ReloadTarget {
        capacity: None,
        capacity_error: None,
        executable: root.path().join("absent-plak"),
        state_key: Some(key.clone()),
        probe: Some(carry::ReloadProbe {
            state_key: key,
            export_versions: carry::IMPORTABLE_VERSIONS.to_vec(),
            config_error: Some("synthetic config refusal".to_owned()),
            binary_id: None,
        }),
        reopen: None,
        request: None,
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

/// "Edit raw config" returned from `$EDITOR`
/// bound to an earlier session, discarding a rebind, a `/new`, and every
/// turn after it. `/new` replaces the attached session inside the host;
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
    // What `/new` produced inside the host while the TUI was attached.
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
        "the editor must return to /new's session"
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

#[test]
fn failed_exec_removes_only_the_receipt_and_keeps_the_export() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("session.json");
    std::fs::write(&path, "export").unwrap();
    std::fs::write(path.with_extension("handoff"), "receipt").unwrap();
    remove_failed_handoff_receipt(Some(&path));
    assert!(path.is_file());
    assert!(!path.with_extension("handoff").exists());
    remove_failed_handoff_receipt(None);
}

/// A round that resumes another session settles the reload requests its
/// face still held for the session it left; a resume of that same session,
/// or any other request, settles none.
#[test]
fn a_round_resuming_another_session_settles_the_reload_requests_it_held() {
    let root = tempfile::tempdir().unwrap();
    let paths = product_paths::PlakPaths::explicit(root.path().join("home"));
    paths.ensure_layout().unwrap();
    let left = uuid::Uuid::new_v4().to_string();
    let next = uuid::Uuid::new_v4().to_string();
    let held = |session: &str| -> crate::reload_request::ReloadRequest {
        serde_json::from_value(serde_json::json!({
            "request_id": uuid::Uuid::new_v4().to_string(),
            "session_id": session,
            "expect_binary": "build-1",
        }))
        .unwrap()
    };
    let outcome = |request: &crate::reload_request::ReloadRequest| {
        std::fs::read(crate::reload_request::outcome_path(
            &paths,
            &request.request_id,
        ))
        .ok()
        .map(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).unwrap())
    };
    let requests = vec![held(&left), held(&left)];
    let resume = |id: &str| tui::SessionRequest::ResumeSession {
        id: id.to_owned(),
        held_reload_requests: requests.clone(),
    };
    settle_left_reload_requests(&paths, &resume(&left));
    settle_left_reload_requests(&paths, &tui::SessionRequest::EditConfig);
    assert!(
        requests.iter().all(|request| outcome(request).is_none()),
        "a resume of the same session, or another request, settles nothing"
    );
    settle_left_reload_requests(&paths, &resume(&next));
    for request in &requests {
        let settled = outcome(request).unwrap();
        assert_eq!(settled["outcome"], "refused");
        assert_eq!(settled["reason"], "not taken: the session switched");
    }
}
