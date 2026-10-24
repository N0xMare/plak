use super::*;
use crate::CliFailure;
use crate::carry::document::{private_dir, write_private_new};

#[test]
fn only_an_exact_private_local_handoff_receipt_retains_replay() {
    let home = tempfile::tempdir().unwrap();
    let carry = private_dir(&home.path().join("carry")).unwrap();
    let path = carry.join("session.json");
    write_private_new(&path, b"exported bytes").unwrap();
    let digest = plakous::model::sha256_hex(&[b"exported bytes"]);
    assert!(!consume_handoff_receipt(&path, home.path(), Some(&digest)));
    write_private_new(&path.with_extension("handoff"), digest.as_bytes()).unwrap();
    assert!(!consume_handoff_receipt(
        &path,
        home.path(),
        Some("tampered")
    ));
    assert!(!path.with_extension("handoff").exists());
    write_private_new(&path.with_extension("handoff"), digest.as_bytes()).unwrap();
    assert!(consume_handoff_receipt(&path, home.path(), Some(&digest)));
    assert!(!consume_handoff_receipt(&path, home.path(), Some(&digest)));
    let foreign = tempfile::tempdir().unwrap();
    assert!(!consume_handoff_receipt(
        &path,
        foreign.path(),
        Some(&digest)
    ));
}

#[test]
fn import_resume_post_commit_failure_names_the_successor_and_recovery_command() {
    let failure = import_resume_failure(
        CliFailure::new("synthetic TUI failure"),
        "new-session",
        "old-session",
    );
    assert_eq!(
        failure.message(),
        "synthetic TUI failure; session new-session was carried forward from old-session; fix the error and run: plak --resume new-session"
    );
}
