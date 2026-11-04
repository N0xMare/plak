#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]

use super::*;
use crate::product_session_log;

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
