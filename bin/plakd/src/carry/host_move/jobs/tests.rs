//! Export-v13 allocation identity is metadata, never fabricated exit evidence.
use super::*;

type TestResult = Result<(), Box<dyn std::error::Error>>;

const RETAINED: &str = r#"{"next_number":2,"rows":[{"number":1,"command":"retained process","started_seconds":1,"started_nanos":0,"owner":{"line":"00000000-0000-0000-0000-000000000001","generation":1,"session":"00000000-0000-0000-0000-000000000002","branch":"00000000-0000-0000-0000-000000000003","fence":4},"session":[5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5],"owner_generation":3,"terminal":null}]}"#;

#[test]
fn live_job_join_requires_exact_allocations_and_accepts_a_concurrent_terminal() -> TestResult {
    use plakous::tools::{JobStatus, JobView};
    let cut: Jobs = serde_json::from_str(RETAINED)?;
    let running = JobView {
        number: 1,
        command: "retained process".into(),
        started_at: Timestamp::new(1, 0)?,
        owner: "observational attribution".into(),
        elapsed: std::time::Duration::ZERO,
        status: JobStatus::Running,
        output: String::new(),
        session_id: [5; 16],
        owner_generation: 3,
    };
    cut.verify_inventory(std::slice::from_ref(&running), JobCut::Retained)?;
    assert!(cut.verify_inventory(&[], JobCut::Retained).is_err());
    assert!(
        cut.verify_inventory(std::slice::from_ref(&running), JobCut::Settled)
            .is_err()
    );
    assert!(
        cut.verify_inventory(&[running.clone(), running.clone()], JobCut::Retained)
            .is_err()
    );
    for changed in [
        JobView {
            session_id: [6; 16],
            ..running.clone()
        },
        JobView {
            owner_generation: 4,
            ..running.clone()
        },
        JobView {
            number: 2,
            ..running.clone()
        },
        JobView {
            status: JobStatus::OutcomeUnknown,
            ..running.clone()
        },
    ] {
        assert!(cut.verify_inventory(&[changed], JobCut::Retained).is_err());
    }
    cut.verify_inventory(
        &[JobView {
            status: JobStatus::Exited(Some(0)),
            ..running
        }],
        JobCut::Retained,
    )?;
    Ok(())
}

#[test]
fn live_cut_job_metadata_pin_roundtrips_without_claiming_liveness_or_exit() -> TestResult {
    let wire: Jobs = serde_json::from_str(RETAINED)?;
    assert!(JobCut::Settled.validate(&wire).is_err());
    JobCut::Retained.validate(&wire)?;
    let jobs = wire.decode()?;
    assert_eq!(jobs.next_id(), 2);
    assert_eq!(jobs.rows()[0].session_id.as_bytes(), [5; 16]);
    assert_eq!(jobs.rows()[0].owner_generation.get(), 3);
    assert!(jobs.rows()[0].terminal.is_none());
    assert_eq!(serde_json::to_string(&Jobs::capture(&jobs)?)?, RETAINED);
    Ok(())
}

#[test]
fn live_cut_job_metadata_refuses_unknown_fields_and_terminal_allocation_conflicts() -> TestResult {
    let value: serde_json::Value = serde_json::from_str(RETAINED)?;
    let mut unknown = value.clone();
    unknown["rows"][0]["pid"] = serde_json::json!(42);
    assert!(serde_json::from_value::<Jobs>(unknown).is_err());
    for field in ["session", "owner_generation"] {
        let mut missing = value.clone();
        missing["rows"][0]
            .as_object_mut()
            .ok_or("row")?
            .remove(field);
        assert!(serde_json::from_value::<Jobs>(missing).is_err());
    }
    let mut invalid = value.clone();
    invalid["rows"][0]["owner_generation"] = serde_json::json!(0);
    assert!(serde_json::from_value::<Jobs>(invalid)?.decode().is_err());
    let mut conflict = value;
    conflict["rows"][0]["terminal"] = serde_json::json!({
        "number":1,"process":[5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5],
        "generation":4,"status":{"kind":"exited","code":0},
        "elapsed_seconds":1,"elapsed_nanos":0,"tail":"done"
    });
    assert!(
        serde_json::from_value::<Jobs>(conflict.clone())?
            .decode()
            .is_err()
    );
    conflict["rows"][0]["terminal"]["generation"] = serde_json::json!(3);
    let wire: Jobs = serde_json::from_value(conflict)?;
    JobCut::Settled.validate(&wire)?;
    assert!(wire.decode()?.rows()[0].terminal.is_some());
    let malformed = serde_json::json!({"kind":"released","ignored":true});
    assert!(serde_json::from_value::<Status>(malformed).is_err());
    Ok(())
}
