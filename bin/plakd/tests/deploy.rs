//! CLI deployment tests with a local substitute for SSH and the target fleet.

use std::process::Command;

fn run(case: &str) -> std::io::Result<()> {
    let output = Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/deploy_fleet.py"
        ))
        .args([case, env!("CARGO_BIN_EXE_plakd")])
        .output()?;
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

#[test]
fn deploy_fake_fleet_plans_and_converges() -> std::io::Result<()> {
    run("fleet")
}

#[test]
fn deploy_canaries_are_keyed_by_exact_account() -> std::io::Result<()> {
    run("named-accounts")
}

#[test]
fn deploy_requester_loss_does_not_repeat_surrender() -> std::io::Result<()> {
    run("requester-loss")
}

#[test]
fn deploy_target_qualifies_the_actual_unit_and_serving_facts() -> std::io::Result<()> {
    let output = Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/deploy_target.py"
        ))
        .output()?;
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}
