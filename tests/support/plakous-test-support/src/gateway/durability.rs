//! Crash skeleton for the credential gateway.
//!
//! The switch rows name the recovery edges. The product cuts that walk them
//! land with session account choice. Refresh claim steps name what a restart
//! sees; the in-process abort below proves a killed refresh dials nothing.

use std::io::BufRead as _;
use std::process::{Command, Stdio};
use std::time::Duration;

use plakous_gateway_protocol::call;
use plakous_gateway_protocol::facts::{RefreshMode, RefreshRequest};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::tests::{self};
use super::{FixtureError, GatewayScript, serve_one};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

struct RefreshClaimStep {
    step: &'static str,
    durable: &'static str,
    remote_sees: &'static str,
}

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
fn refresh_claim_steps_name_the_quarantine() {
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
async fn gateway_killed_mid_refresh_dials_nothing() -> TestResult {
    let gateway = tests::gateway()?;
    gateway.push(GatewayScript::Stall(Duration::from_secs(30)));
    let (client, server) = tokio::io::duplex(8 * 1024);
    let running = gateway.clone();
    let task = tokio::spawn(async move { serve_one(server, &running).await });
    let (mut read, mut write) = tokio::io::split(client);
    let body = refresh_body()?;
    write.write_all(&http_post("/v1/refresh", &body)).await?;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(gateway.dials(), 0);
    task.abort();
    let _ = task.await;
    let mut response = Vec::new();
    let _ = read.read_to_end(&mut response).await;
    assert!(!response.starts_with(b"HTTP/1.1 200"));
    assert_eq!(gateway.dials(), 0);
    Ok(())
}

#[tokio::test]
async fn gateway_killed_mid_stream_ends_the_body_after_head() -> TestResult {
    let gateway = tests::gateway()?;
    gateway.push(GatewayScript::Relay(large_reply()));
    let (client, server) = tokio::io::duplex(256);
    let running = gateway.clone();
    let task = tokio::spawn(async move { serve_one(server, &running).await });
    let (mut read, mut write) = tokio::io::split(client);
    write
        .write_all(&http_post("/v1/relay", &tests::relay_body()?))
        .await?;
    write.shutdown().await?;
    let mut response = Vec::new();
    let mut tmp = [0_u8; 64];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while response.len() < 180 && tokio::time::Instant::now() < deadline {
        let read = tokio::time::timeout(Duration::from_millis(200), read.read(&mut tmp)).await;
        match read {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => response.extend_from_slice(&tmp[..n]),
            _ => break,
        }
    }
    assert!(response.contains(&0x01), "head frame missing");
    assert!(
        response.len() < 4_096,
        "the body finished before the gateway was killed"
    );
    task.abort();
    let _ = task.await;
    assert_eq!(gateway.dials(), 1);
    Ok(())
}

#[tokio::test]
async fn host_killed_mid_stream_cancels_after_one_dial() -> TestResult {
    let gateway = tests::gateway()?;
    gateway.push(GatewayScript::Relay(large_reply()));
    let (client, server) = tokio::io::duplex(256);
    let running = gateway.clone();
    let task = tokio::spawn(async move { serve_one(server, &running).await });
    let (mut read, mut write) = tokio::io::split(client);
    write
        .write_all(&http_post("/v1/relay", &tests::relay_body()?))
        .await?;
    write.shutdown().await?;
    let mut tmp = [0_u8; 32];
    let _ = tokio::time::timeout(Duration::from_secs(2), read.read(&mut tmp)).await;
    drop(read);
    drop(write);
    let report = tokio::time::timeout(Duration::from_secs(2), task).await??;
    match report {
        Ok(report) => {
            assert!(report.dialed || gateway.dials() == 1);
            assert!(report.caller_cancelled || gateway.dials() == 1);
        }
        Err(FixtureError::Io(_)) => {}
        Err(error) => return Err(error.into()),
    }
    assert_eq!(gateway.dials(), 1);
    Ok(())
}

fn large_reply() -> super::UpstreamReply {
    super::UpstreamReply {
        status: 200,
        headers: Vec::new(),
        body: vec![b'x'; 8 * 1024],
        trailers: Vec::new(),
    }
}

fn http_post(path: &str, body: &[u8]) -> Vec<u8> {
    let mut bytes = format!(
        "POST {path} HTTP/1.1\r\nhost: gateway.invalid\r\ncontent-length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

fn refresh_body() -> TestResult<Vec<u8>> {
    let request = RefreshRequest {
        account: plakous_gateway_protocol::GatewayAccount::new("anthropic")?,
        mode: RefreshMode::IfDue {
            expected: plakous_gateway_protocol::envelope::ExpectedFence {
                account_binding: plakous_gateway_protocol::HexDigest::new("ab".repeat(32))?,
                generation: 1,
            },
        },
    };
    Ok(call::encode(&request)?)
}
