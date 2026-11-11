//! Host-level allowance failures preserve the independent retained usage view.

#![cfg(feature = "test-hooks")]

use super::*;
use plakd_wire_types::console::{Decimal, ReadAction, UsageWindow};
use plakous::model::{SubscriptionUsageRead, SubscriptionUsageUnavailable, Timestamp};
use std::sync::atomic::AtomicUsize;
use std::time::Duration;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn fixture() -> Result<(tempfile::TempDir, Arc<Console>), Box<dyn std::error::Error>> {
    let root = tempfile::tempdir()?;
    let paths = PlakPaths::explicit(root.path().join("home"));
    let _store = crate::telemetry_store::TelemetryStore::open(&paths)?;
    let connection = rusqlite::Connection::open(paths.telemetry_database())?;
    connection.execute("UPDATE recording_evidence SET started_at_ms=0", [])?;
    for (id, account) in [
        ("selected", Some("codex")),
        ("other", Some("openai/other")),
        ("unknown", None),
    ] {
        connection.execute("INSERT INTO recorded_terminals VALUES(?1,'session','branch',?2,?3,'123','4','0','0','0','2','2','7')", rusqlite::params![id, clock::now_ms()-1_000, account])?;
    }
    let console = Console::new(
        paths,
        root.path().join("state"),
        Some(tokio::runtime::Handle::current()),
        ConfiguredSwitches {
            web_accounts_read: false,
            web_config_read: false,
            web_credentials_write: false,
            web_config_write: false,
        },
    );
    Ok((root, console))
}

fn context() -> HostRequestContext {
    HostRequestContext {
        principal: HostPrincipal::Socket(7),
        connection_id: 1,
        host_generation: 1,
        request_id: 1,
    }
}

#[tokio::test]
async fn review_exhausted_budget_keeps_recorded_rows_instead_of_refusing_all() -> TestResult {
    let (_root, console) = fixture()?;
    let response = console
        .usage(
            Some("codex".into()),
            UsageWindow::Hours24,
            clock::utc(clock::now_ms()).ok_or("clock")?,
            Instant::now(),
            Arc::new(AtomicBool::new(false)),
        )
        .await
        .map_err(|_| "usage refused")?;
    let HostResponse::AccountsUsage { rows, .. } = response else {
        return Err("usage response".into());
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0]
            .allowance
            .unavailable
            .as_ref()
            .map(|d| d.code.as_str()),
        Some("deadline")
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn console_accounts_usage_is_one_native_get_without_refresh_and_keeps_recorded_subtotals()
-> TestResult {
    let (_root, console) = fixture()?;
    let gets = Arc::new(AtomicUsize::new(0));
    let refreshes = Arc::new(AtomicUsize::new(0));
    let paths = plakous::subscriptions::SubscriptionPaths::from_xdg_state_home(&console.state)?;
    let subscriptions = plakous::subscriptions::NativeSubscriptions::new(paths)
        .with_usage_fixture_for_tests({
            let gets = Arc::clone(&gets);
            let refreshes = Arc::clone(&refreshes);
            move |_, refreshing| {
                gets.fetch_add(1, Ordering::SeqCst);
                if refreshing {
                    refreshes.fetch_add(1, Ordering::SeqCst);
                }
                Box::pin(async {
                    SubscriptionUsageRead::Unavailable {
                        observed_at: Timestamp::new(1, 0).unwrap(),
                        reason: SubscriptionUsageUnavailable::Unauthorized,
                    }
                })
            }
        });
    *console.native_fixture.lock().unwrap() = Some(subscriptions);
    let request = HostRequest::AccountsUsage {
        account_id: Some("codex".into()),
        window: UsageWindow::Hours24,
        cursor: None,
        limit: Some(1),
    };
    let response = console
        .request(context(), request)
        .await
        .map_err(|_| "wire refusal")?;
    let HostResponse::AccountsUsage {
        rows, unattributed, ..
    } = response
    else {
        return Err("usage response".into());
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].account_id, "codex");
    assert_eq!(
        rows[0].recorded.calls.as_ref().map(Decimal::as_str),
        Some("2")
    );
    assert_eq!(
        rows[0].recorded.input_tokens.as_ref().map(Decimal::as_str),
        Some("123")
    );
    assert_eq!(unattributed.calls.as_ref().map(Decimal::as_str), Some("2"));
    assert_eq!(
        rows[0].allowance.remedies,
        vec![ReadRemedy {
            action: ReadAction::Refresh,
            account_id: Some("codex".into())
        }]
    );
    assert_eq!(gets.load(Ordering::SeqCst), 1);
    assert_eq!(refreshes.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stalled_native_providers_leave_time_for_host_paging_and_recorded_rows() -> TestResult {
    let (_root, console) = fixture()?;
    let gets = Arc::new(AtomicUsize::new(0));
    let paths = plakous::subscriptions::SubscriptionPaths::from_xdg_state_home(&console.state)?;
    let subscriptions = plakous::subscriptions::NativeSubscriptions::new(paths)
        .with_usage_fixture_for_tests({
            let gets = Arc::clone(&gets);
            move |_, _| {
                gets.fetch_add(1, Ordering::SeqCst);
                Box::pin(std::future::pending())
            }
        });
    *console.native_fixture.lock().unwrap() = Some(subscriptions);
    let response = console
        .request(
            context(),
            HostRequest::AccountsUsage {
                account_id: None,
                window: UsageWindow::Hours24,
                cursor: None,
                limit: None,
            },
        )
        .await
        .map_err(|_| "wire refusal")?;
    let HostResponse::AccountsUsage { rows, .. } = response else {
        return Err("stalled allowance erased usage".into());
    };
    assert!(gets.load(Ordering::SeqCst) >= 2);
    let recorded = rows
        .iter()
        .find(|row| row.account_id == "codex")
        .ok_or("codex row")?;
    assert_eq!(
        recorded.recorded.calls.as_ref().map(Decimal::as_str),
        Some("2")
    );
    assert!(
        rows.iter()
            .filter(|row| matches!(row.account_id.as_str(), "codex" | "grok"))
            .all(|row| row
                .allowance
                .unavailable
                .as_ref()
                .is_some_and(|d| d.code == "deadline"))
    );
    Ok(())
}

#[tokio::test]
async fn telemetry_writes_do_not_change_usage_cursor_revision() -> TestResult {
    let (_root, console) = fixture()?;
    let cancelled = AtomicBool::new(false);
    let deadline = Instant::now() + Duration::from_secs(10);
    let first = pages::source_revision(&console.paths, &console.state, None, deadline, &cancelled)
        .await
        .map_err(|_| "first revision")?;
    let connection = rusqlite::Connection::open(console.paths.telemetry_database())?;
    connection.execute("UPDATE recording_evidence SET dropped=dropped+1", [])?;
    connection.execute("INSERT INTO recorded_terminals VALUES('concurrent','session','branch',?1,'codex','5','0','0','0','0','1','1',NULL)", [clock::now_ms()])?;
    let after = pages::source_revision(&console.paths, &console.state, None, deadline, &cancelled)
        .await
        .map_err(|_| "after revision")?;
    assert_eq!(first, after);
    Ok(())
}
