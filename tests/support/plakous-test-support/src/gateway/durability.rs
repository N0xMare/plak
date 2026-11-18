//! API-key crash rows for the credential gateway.
//!
//! Subscription refresh claim steps stay names: the OAuth custodians that
//! own those writes are not in this tree. Switch rows name the recovery
//! edges the product cut walks later.

use std::io::BufRead as _;
use std::process::{Command, Stdio};
use std::time::Duration;

use plakous_http::{Phase, StatusCode, TransmissionFact};

use super::tests::{self};
use super::{FixtureError, GatewayScript};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

struct RefreshClaimStep {
    step: &'static str,
    durable: &'static str,
    remote_sees: &'static str,
}

/// Subscription kinds only. An API key has no refresh claim to kill.
const REFRESH_CLAIM_STEPS: &[RefreshClaimStep] = &[
    RefreshClaimStep {
        step: "before_claim",
        durable: "previous Ready generation",
        remote_sees: "current facts; no quarantine",
    },
    RefreshClaimStep {
        step: "claim_written",
        durable: "RefreshOutcomeUnknown",
        remote_sees: "credential_not_ready quarantined",
    },
    RefreshClaimStep {
        step: "provider_returned",
        durable: "RefreshOutcomeUnknown",
        remote_sees: "credential_not_ready quarantined; codex and claude need a login; xai reclaims",
    },
    RefreshClaimStep {
        step: "store_committed",
        durable: "Ready at the next generation",
        remote_sees: "ready facts",
    },
];

struct SwitchCrashRow {
    step: u8,
    durable: &'static str,
    reopen: &'static str,
    rebinds: u8,
    prompts: u8,
}

/// §7.3 as rewritten has four steps. Rows cover steps 2-4, one rebind and at
/// most one prompt. Earlier numbering through step 7 is not a separate edge.
const SWITCH_CRASH_ROWS: &[SwitchCrashRow] = &[
    SwitchCrashRow {
        step: 2,
        durable: "rebind workflow request unpublished",
        reopen: "withdrawn, never resubmitted",
        rebinds: 1,
        prompts: 0,
    },
    SwitchCrashRow {
        step: 2,
        durable: "rebind published, command absent",
        reopen: "notice and one-click continue",
        rebinds: 1,
        prompts: 0,
    },
    SwitchCrashRow {
        step: 3,
        durable: "root-start command submitted",
        reopen: "started, no second prompt",
        rebinds: 1,
        prompts: 1,
    },
    SwitchCrashRow {
        step: 4,
        durable: "unpublished rebind",
        reopen: "withdrawn",
        rebinds: 1,
        prompts: 0,
    },
    SwitchCrashRow {
        step: 4,
        durable: "rebound without the command",
        reopen: "notice and one-click continue",
        rebinds: 1,
        prompts: 0,
    },
];

#[test]
fn subscription_refresh_crash_rows_wait_for_oauth_custodians() {
    assert!(REFRESH_CLAIM_STEPS.len() >= 4);
    for step in REFRESH_CLAIM_STEPS {
        assert!(!step.step.is_empty(), "{}", step.durable);
    }
    let quarantined: Vec<_> = REFRESH_CLAIM_STEPS
        .iter()
        .filter(|step| step.durable.contains("RefreshOutcomeUnknown"))
        .collect();
    assert_eq!(quarantined.len(), 2);
    assert!(
        quarantined
            .iter()
            .all(|step| step.remote_sees.contains("quarantined"))
    );
}

#[test]
fn switch_crash_rows_name_one_rebind_and_at_most_one_prompt() {
    assert!(SWITCH_CRASH_ROWS.iter().any(|row| row.step == 2));
    assert!(SWITCH_CRASH_ROWS.iter().any(|row| row.step == 3));
    assert!(SWITCH_CRASH_ROWS.iter().any(|row| row.step == 4));
    for row in SWITCH_CRASH_ROWS {
        assert_eq!(row.rebinds, 1, "{}", row.durable);
        assert!(row.prompts <= 1, "{}", row.reopen);
        assert_ne!(row.reopen, "");
    }
}

#[tokio::test]
async fn holder_lock_crash_releases_the_kernel_lock() -> TestResult {
    let file = tempfile::NamedTempFile::new()?;
    let path = file.path().to_owned();
    let mut child = Command::new("python3")
        .arg("-c")
        .arg(
            "import fcntl, os, sys\n\
             fd = os.open(sys.argv[1], os.O_RDWR | os.O_CREAT, 0o600)\n\
             fcntl.flock(fd, fcntl.LOCK_EX)\n\
             sys.stdout.write('locked\\n')\n\
             sys.stdout.flush()\n\
             sys.stdin.read()\n",
        )
        .arg(&path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("no stdout"))?;
    let mut line = String::new();
    std::io::BufReader::new(&mut stdout).read_line(&mut line)?;
    assert_eq!(line, "locked\n");
    child.kill()?;
    let status = child.wait()?;
    assert!(!status.success());
    let probe = Command::new("python3")
        .arg("-c")
        .arg(
            "import fcntl, os, sys\n\
             fd = os.open(sys.argv[1], os.O_RDWR)\n\
             fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)\n",
        )
        .arg(&path)
        .status()?;
    assert!(probe.success(), "a dead holder's lock was not released");
    Ok(())
}

#[tokio::test]
async fn gateway_killed_mid_stream_is_may_have_transmitted_without_replay() -> TestResult {
    let gateway = hung_gateway()?;
    let client = gateway.relayed().await?;
    let response = client
        .execute(
            tests::api_request(logical()?, tests::placeholder()?)?,
            tests::control(),
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(gateway.dials(), 1);
    gateway.kill();
    let mut body = response.into_body();
    let outcome = tokio::time::timeout(Duration::from_secs(2), body.next_chunk())
        .await
        .map_err(|_| "the body did not break within 2s of the gateway being killed")?;
    let Some(result) = outcome else {
        return Err("the body ended cleanly after the gateway was killed".into());
    };
    let Err(failure) = result else {
        return Err("the body continued after the gateway was killed".into());
    };
    assert_eq!(failure.phase(), Phase::ResponseBody);
    assert_eq!(failure.transmission(), TransmissionFact::MayHaveTransmitted);
    assert_eq!(gateway.dials(), 1, "a killed gateway must not be replayed");
    Ok(())
}

#[tokio::test]
async fn host_killed_mid_stream_cancels_the_upstream() -> TestResult {
    let gateway = hung_gateway()?;
    let running = gateway.clone();
    let task = tokio::spawn(async move {
        let client = running.relayed().await?;
        let response = client
            .execute(
                tests::api_request(logical()?, tests::placeholder()?)?,
                tests::control(),
            )
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        let mut body = response.into_body();
        let _ = body.next_chunk().await;
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    });
    wait_until(|| gateway.dials() == 1).await?;
    task.abort();
    let _ = task.await;
    tokio::time::timeout(Duration::from_secs(1), gateway.wait_for_upstream_close())
        .await
        .map_err(|_| "the upstream was still open 1s after the host was killed")?;
    assert_eq!(gateway.dials(), 1);
    Ok(())
}

#[tokio::test]
async fn dropping_the_caller_cancels_the_upstream_within_a_bound() -> TestResult {
    let gateway = hung_gateway()?;
    let client = gateway.relayed().await?;
    let response = client
        .execute(
            tests::api_request(logical()?, tests::placeholder()?)?,
            tests::control(),
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(gateway.dials(), 1);
    drop(response);
    tokio::time::timeout(Duration::from_secs(1), gateway.wait_for_upstream_close())
        .await
        .map_err(|_| "dropping the caller did not cancel the upstream within 1s")?;
    assert_eq!(gateway.dials(), 1);
    Ok(())
}

#[tokio::test]
async fn a_restarted_gateway_serves_the_next_request() -> TestResult {
    let gateway = tests::open_gateway()?;
    let first = gateway.instance();
    let client = gateway.relayed().await?;
    let response = client
        .execute(
            tests::api_request(logical()?, tests::placeholder()?)?,
            tests::control(),
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let restarted = gateway.restart()?;
    assert_ne!(restarted, first);
    assert_eq!(gateway.instance(), restarted);
    let client = gateway.relayed().await?;
    let response = client
        .execute(
            tests::api_request(logical()?, tests::placeholder()?)?,
            tests::control(),
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(gateway.dials(), 2);
    Ok(())
}

fn hung_gateway() -> Result<super::FixtureGateway, FixtureError> {
    let gateway = tests::open_gateway()?;
    gateway.push(GatewayScript::DropAfterHead);
    Ok(gateway)
}

fn logical() -> TestResult<plakous_http::Origin> {
    Ok(plakous_http::Origin::parse(&format!(
        "{}/",
        super::service::LOGICAL
    ))?)
}

async fn wait_until(mut ready: impl FnMut() -> bool) -> TestResult {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        if ready() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Err("the upstream was not dialed".into())
}
