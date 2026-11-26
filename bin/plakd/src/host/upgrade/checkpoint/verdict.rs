//! The outer verdict fields and the Ready tag remain stable on rollback.

use std::{fs, io::Write};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use super::{
    FRAME, GateTiming, Outcome, baseline, check_store_identities, claim, decode, error, open, root,
    verdict_path,
};
use crate::home::paths::PlakPaths;
use crate::{CliFailure, CliFailureKind, CliResult, ProcessOutcome};

#[derive(Serialize)]
pub(super) struct WriteVerdict {
    version: u32,
    state_key: String,
    nonce: Uuid,
    runtime_expected: Option<bool>,
    outcome: Outcome,
    // Optional diagnostics: every version-1 reader ignores the field.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    timings: Vec<GateTiming>,
}

impl WriteVerdict {
    pub(super) fn new(
        paths: &PlakPaths,
        nonce: Uuid,
        baseline_runtime: Option<bool>,
        outcome: Outcome,
        timings: Vec<GateTiming>,
    ) -> CliResult<Self> {
        let previously_expected = read(paths)?
            .filter(|prior| prior.nonce == nonce)
            .is_some_and(|prior| prior.runtime_expected == Some(true));
        let runtime_expected = if previously_expected || runtime_present(paths)? {
            Some(true)
        } else {
            baseline_runtime
        };
        Ok(Self {
            version: 1,
            state_key: crate::home::state::state_key(),
            nonce,
            runtime_expected,
            outcome,
            timings,
        })
    }
}

#[derive(Deserialize)]
struct ReadVerdict {
    #[serde(default)]
    version: u32,
    state_key: Option<String>,
    nonce: Uuid,
    runtime_expected: Option<bool>,
    outcome: Value,
    #[serde(default)]
    timings: Value,
}

fn runtime_present(paths: &PlakPaths) -> CliResult<bool> {
    match fs::symlink_metadata(paths.runtime_database()) {
        Ok(_) => Ok(true),
        Err(failure) if failure.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(failure) => Err(error(failure)),
    }
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum State {
    Pending,
    Ready,
    Failed,
    Unknown,
}

impl ReadVerdict {
    fn tag(&self, label: &str) -> bool {
        self.outcome
            .as_object()
            .is_some_and(|map| map.len() == 1 && map.contains_key(label))
    }

    fn state(&self) -> State {
        if self.tag("Ready") {
            State::Ready
        } else if self.tag("Failed") {
            State::Failed
        } else if self.outcome.as_str() == Some("Pending") {
            State::Pending
        } else {
            State::Unknown
        }
    }
}

fn read(paths: &PlakPaths) -> CliResult<Option<ReadVerdict>> {
    open(&verdict_path(paths), FRAME)
        .and_then(|file| file.map(|file| decode(file, FRAME)).transpose())
        .map_err(|_| CliFailure::classified(
            CliFailureKind::RecoveryRequired,
            "host_upgrade_verdict_unreadable: stop the service and all home writers; keep the verdict and checkpoint, and repair the original verdict or recover the home offline (see the host service guide)",
        ))
}

pub(super) fn outcome(
    paths: &PlakPaths,
    nonce: Uuid,
) -> CliResult<Option<(Outcome, Vec<GateTiming>)>> {
    read(paths)?
        .filter(|verdict| verdict.nonce == nonce)
        .map(|verdict| {
            let outcome = serde_json::from_value(verdict.outcome).map_err(error)?;
            // Diagnostics never decide the outcome.
            Ok((
                outcome,
                serde_json::from_value(verdict.timings).unwrap_or_default(),
            ))
        })
        .transpose()
}

pub(super) fn fenced(paths: &PlakPaths) -> CliResult<bool> {
    Ok(read(paths)?.is_some_and(|verdict| !matches!(verdict.state(), State::Ready)))
}

pub(in crate::host) fn operate(
    paths: &PlakPaths,
    acknowledge: Option<Uuid>,
    output: &mut dyn Write,
) -> CliResult<ProcessOutcome> {
    #[derive(Serialize)]
    struct Report {
        id: Option<Uuid>,
        version: Option<u32>,
        state: Option<State>,
        phase: Option<&'static str>,
        acknowledged: bool,
    }
    let verdict = if let Some(id) = acknowledge {
        acknowledge_verdict(paths, id)?
    } else {
        read(paths)?
    };
    let phase = verdict
        .as_ref()
        .and_then(|verdict| verdict.outcome.get("Failed"))
        .and_then(Value::as_str)
        .and_then(|phase| match phase {
            "Health" => Some("health"),
            "Preservation" => Some("preservation"),
            "Startup" => Some("startup"),
            _ => None,
        });
    serde_json::to_writer(
        &mut *output,
        &Report {
            id: verdict.as_ref().map(|value| value.nonce),
            version: verdict.as_ref().map(|value| value.version),
            state: verdict.as_ref().map(ReadVerdict::state),
            phase,
            acknowledged: acknowledge.is_some(),
        },
    )
    .map_err(error)?;
    writeln!(output).map_err(error)?;
    Ok(ProcessOutcome::Success)
}

fn acknowledge_verdict(paths: &PlakPaths, id: Uuid) -> CliResult<Option<ReadVerdict>> {
    let stale = || {
        CliFailure::new(
            "host_upgrade_verdict_changed: inspect with `plakd host upgrade-verdict` before acknowledging",
        )
    };
    if read(paths)?.is_none() {
        return Err(stale());
    }
    let _slot = claim(paths)?;
    let _owners = crate::host::lock::HostLock::take_home(paths, crate::build_identity::binary_id())
        .map_err(|_| CliFailure::classified(
            CliFailureKind::Retryable,
            "host_upgrade_acknowledge_requires_stopped_home: stop every host for this home, including a serving predecessor, before acknowledging",
        ))?;
    let _writers = crate::carry::upgrade::admission(paths, true)?;
    let verdict = read(paths)?
        .filter(|verdict| verdict.nonce == id)
        .ok_or_else(stale)?;
    if matches!(verdict.state(), State::Ready) {
        return Err(CliFailure::new(
            "host_upgrade_verdict_not_fenced: a Ready verdict requires no acknowledgement",
        ));
    }
    if verdict
        .state_key
        .as_ref()
        .is_some_and(|key| key != &crate::home::state::state_key())
        || !paths.product_database().is_file()
        || !paths.subagents_database().is_file()
        || (verdict.runtime_expected != Some(false) && !runtime_present(paths)?)
    {
        return Err(CliFailure::new(
            "host_upgrade_acknowledge_wrong_state: use the destination build and check the original home stores before acknowledging",
        ));
    }
    super::super::health::stores(paths)?;
    check_store_identities(paths, id)?;
    let directory = fs::File::open(root(paths)).map_err(error)?;
    fs::remove_file(verdict_path(paths)).map_err(error)?;
    directory.sync_all().map_err(error)?;
    // A crash before the fence is durably cleared must leave its evidence.
    // The next cold start also removes a leftover checkpoint.
    let _ = fs::remove_file(baseline(paths));
    Ok(Some(verdict))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn verdict_compatibility_keeps_the_fence_and_ready_tag() {
        #[derive(Deserialize)]
        struct Legacy {
            nonce: Uuid,
            outcome: Outcome,
        }
        let id = Uuid::new_v4();
        let (_root, paths) = PlakPaths::isolated_test_home();
        let written = serde_json::to_value(
            WriteVerdict::new(&paths, id, Some(false), Outcome::Pending, Vec::new()).unwrap(),
        )
        .unwrap();
        let legacy: Legacy = serde_json::from_value(written).unwrap();
        assert_eq!(legacy.nonce, id);
        assert!(matches!(legacy.outcome, Outcome::Pending));

        for (outcome, expected) in [
            (json!("Pending"), "pending"),
            (json!({"Failed": "FutureStage"}), "failed"),
            (json!({"Ready": {"future": true}}), "ready"),
            (json!({"FutureOutcome": null}), "unknown"),
        ] {
            let decoded: ReadVerdict = serde_json::from_value(json!({
                "nonce": id, "outcome": outcome, "future": true,
            }))
            .unwrap();
            assert_eq!(decoded.version, 0);
            assert_eq!(serde_json::to_value(decoded.state()).unwrap(), expected);
        }
    }

    #[test]
    fn gate_timings_are_optional_diagnostics_in_the_verdict() {
        let id = Uuid::new_v4();
        let (_root, paths) = PlakPaths::isolated_test_home();
        let timing = GateTiming::new(
            super::super::GatePhase::StoreHealthRuntime,
            std::time::Duration::from_millis(1234),
        );
        // The largest outcome: Ready with the longest instance and evidence
        // names, every phase recorded at the longest elapsed value.
        let ready = Outcome::Ready(
            format!("{}-{:016x}", u32::MAX, u64::MAX),
            super::super::Evidence::BaselinePredatesDrain,
        );
        let longest = GateTiming::new(
            super::super::GatePhase::StoreHealthProduct,
            std::time::Duration::MAX,
        );
        let bound = WriteVerdict::new(&paths, id, Some(true), ready, vec![longest; 8]).unwrap();
        assert!(serde_json::to_vec(&bound).unwrap().len() < usize::try_from(FRAME).unwrap());
        let failed = Outcome::Failed(super::super::Failure::Health);
        let written = serde_json::to_value(
            WriteVerdict::new(&paths, id, Some(true), failed, vec![timing]).unwrap(),
        )
        .unwrap();
        let decoded: ReadVerdict = serde_json::from_value(written).unwrap();
        let timings: Vec<GateTiming> = serde_json::from_value(decoded.timings).unwrap();
        assert_eq!(
            timings[0].detail(),
            "destination phase=store_health_runtime elapsed_ms=1234"
        );
        let future: Vec<GateTiming> =
            serde_json::from_value(json!([{"phase": "future_step", "elapsed_ms": 1}])).unwrap();
        assert_eq!(future[0].detail(), "destination phase=unknown elapsed_ms=1");
        let pending = serde_json::to_value(
            WriteVerdict::new(&paths, id, None, Outcome::Pending, Vec::new()).unwrap(),
        )
        .unwrap();
        assert!(pending.get("timings").is_none());
    }
}
