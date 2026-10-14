//! Repository-only command boundary: rehearsal must bypass diagnostics writers.

#[test]
fn missing_fixture_session_does_not_create_logs_or_state() -> Result<(), Box<dyn std::error::Error>>
{
    let home = tempfile::tempdir()?;
    let result = std::process::Command::new(env!("CARGO_BIN_EXE_plak"))
        .args([
            "session",
            "rehearse",
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
        ])
        .env_clear()
        .env("PLAK_HOME", home.path())
        .output()?;
    assert!(!result.status.success());
    assert!(result.stdout.is_empty());
    assert_eq!(std::fs::read_dir(home.path())?.count(), 0);
    Ok(())
}
