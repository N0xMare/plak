//! Frozen terminal facts and recording evidence, never admission authority.

use plakous_hosted::product_observation::{BoundedText64, ProductObservation};
use rusqlite::{Connection, params};

pub(crate) fn exact_account(value: &str) -> Option<BoundedText64> {
    let account = value.parse::<plakous_hosted::account::AccountRef>().ok()?;
    (account.as_str() == value)
        .then(|| BoundedText64::new(value))
        .flatten()
}

pub(crate) struct TurnAttribution {
    pub locator: Option<plakous::sessions::SessionLocator>,
    pub account: Option<String>,
}

pub(super) fn account_for(
    record: &TurnAttribution,
    terminal: &plakous_hosted::sessions::projector::ProjectedTurnUsage,
) -> Option<BoundedText64> {
    let locator = record.locator?;
    if locator.session_id() != terminal.session_id || locator.branch_id() != terminal.branch_id {
        return None;
    }
    exact_account(record.account.as_deref()?)
}

pub(super) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

pub(super) fn write_terminal(
    connection: &Connection,
    observation: &ProductObservation,
) -> rusqlite::Result<()> {
    let ProductObservation::TurnUsage {
        at,
        session_id,
        branch_id,
        turn_id,
        usage,
        account,
    } = observation
    else {
        return Ok(());
    };
    let committed_at_ms = at
        .seconds()
        .checked_mul(1_000)
        .and_then(|seconds| seconds.checked_add(u64::from(at.nanoseconds() / 1_000_000)))
        .and_then(|value| i64::try_from(value).ok())
        .ok_or(rusqlite::Error::InvalidQuery)?;
    let model = usage.model();
    let account = account
        .as_ref()
        .and_then(|value| exact_account(value.as_str()));
    connection.execute(
        "INSERT OR IGNORE INTO recorded_terminals(turn_id,session_id,branch_id,committed_at_ms,account,input_tokens,output_tokens,reasoning_tokens,cached_input_tokens,cache_write_tokens,model_operations,reporting_operations,cost_micros) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
        params![turn_id.to_string(), session_id.to_string(), branch_id.to_string(), committed_at_ms,
            account.as_ref().map(BoundedText64::as_str), model.input_tokens().to_string(), model.output_tokens().to_string(),
            model.reasoning_tokens().to_string(), model.cached_input_tokens().to_string(), model.cache_write_tokens().to_string(),
            usage.model_operations().to_string(), usage.model_operations_reporting_usage().to_string(),
            model.cost_micros().map(|cost| cost.to_string())],
    )?;
    Ok(())
}

impl super::TelemetryStore {
    pub(crate) fn record_writer_losses(&self, dropped: u64, errors: u64) -> rusqlite::Result<()> {
        if dropped == 0 && errors == 0 {
            return Ok(());
        }
        self.connection.execute(
            "UPDATE recording_evidence SET dropped = dropped + ?1, write_errors = write_errors + ?2 WHERE singleton = 1",
            params![i64::try_from(dropped).map_err(|_| rusqlite::Error::InvalidQuery)?, i64::try_from(errors).map_err(|_| rusqlite::Error::InvalidQuery)?],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::home::paths::PlakPaths;
    use crate::telemetry_store::tests::{TestResult, fixture_context, fixture_timestamp};
    use crate::telemetry_store::{TelemetryItem, TelemetryStore};
    use plakous::{agent::TurnUsage, model::Usage};

    #[test]
    fn terminal_attribution_and_commit_time_survive_replay_and_u64_counters() -> TestResult {
        let (_guard, paths) = PlakPaths::isolated_test_home();
        let mut store = TelemetryStore::open(&paths)?;
        let context = fixture_context()?;
        let first = ProductObservation::TurnUsage {
            at: fixture_timestamp()?,
            session_id: context.session_id(),
            branch_id: context.branch_id(),
            turn_id: context.turn_id().ok_or("turn")?,
            account: exact_account("openai/work"),
            usage: TurnUsage::new(
                Usage::new(u64::MAX, 2, 3, 4, 5).with_cost_micros(Some(u64::MAX)),
                7,
                6,
                1,
            )?,
        };
        store.write_batch(&[TelemetryItem::Product(first)])?;
        let ProductObservation::TurnUsage {
            at,
            session_id,
            branch_id,
            turn_id,
            usage,
            ..
        } = first
        else {
            return Err("terminal".into());
        };
        let replay = ProductObservation::TurnUsage {
            at,
            session_id,
            branch_id,
            turn_id,
            usage,
            account: exact_account("codex/other"),
        };
        store.write_batch(&[TelemetryItem::Product(replay)])?;
        assert_eq!(
            store.query_i64("SELECT count(*) FROM recorded_terminals"),
            Some(1)
        );
        assert_eq!(
            store.query_text("SELECT account FROM recorded_terminals"),
            Some("openai/work".into())
        );
        assert_eq!(
            store.query_text("SELECT input_tokens FROM recorded_terminals"),
            Some(u64::MAX.to_string())
        );
        assert_eq!(
            store.query_text("SELECT cost_micros FROM recorded_terminals"),
            Some(u64::MAX.to_string())
        );
        assert_eq!(
            store.query_text("SELECT model_operations FROM recorded_terminals"),
            Some("7".into())
        );
        assert_eq!(
            store.query_i64("SELECT committed_at_ms FROM recorded_terminals"),
            i64::try_from(at.seconds() * 1_000 + u64::from(at.nanoseconds() / 1_000_000)).ok()
        );
        Ok(())
    }

    #[test]
    fn loss_and_retention_evidence_outlive_deleted_sessions() -> TestResult {
        let (_guard, paths) = PlakPaths::isolated_test_home();
        let store = TelemetryStore::open(&paths)?;
        store.connection.execute("INSERT INTO recorded_terminals VALUES('turn','session','branch',1000,NULL,'0','0','0','0','0','0','0',NULL)",[])?;
        store.record_writer_losses(2, 3)?;
        super::super::delete_session_rows(&store.connection, "session")?;
        assert_eq!(
            store.query_i64("SELECT count(*) FROM recorded_terminals"),
            Some(0)
        );
        assert_eq!(
            store.query_i64("SELECT retained_from_ms FROM recording_evidence"),
            Some(1001)
        );
        assert_eq!(
            store.query_i64("SELECT dropped FROM recording_evidence"),
            Some(2)
        );
        assert_eq!(
            store.query_i64("SELECT write_errors FROM recording_evidence"),
            Some(3)
        );
        assert!(exact_account("chatgpt/work").is_none());
        assert!(exact_account(" openai/work").is_none());
        assert!(exact_account("openai/default").is_none());
        assert!(exact_account("openai/work").is_some());
        assert_eq!(store.query_text("SELECT type || ':' || \"notnull\" FROM pragma_table_info('credential_refreshes') WHERE name = 'account'"),Some("TEXT:0".into()));
        assert_eq!(store.query_i64("SELECT count(*) FROM pragma_index_list('credential_refreshes') WHERE name = 'credential_refreshes_account_at'"),Some(1));
        Ok(())
    }

    #[test]
    fn failed_retention_rolls_back_its_watermark_and_terminal_deletion() -> TestResult {
        let (_guard, paths) = PlakPaths::isolated_test_home();
        let store = TelemetryStore::open(&paths)?;
        store.connection.execute_batch("INSERT INTO recorded_terminals VALUES('turn','session','branch',1000,NULL,'0','0','0','0','0','0','0',NULL); INSERT INTO turns(session_id,branch_id,run_id,agent_id,turn_id) VALUES('session','branch','','','turn'); CREATE TRIGGER refuse_retention BEFORE DELETE ON turns BEGIN SELECT RAISE(ABORT,'fixture'); END;")?;
        assert!(super::super::delete_session_rows(&store.connection, "session").is_err());
        assert_eq!(
            store.query_i64("SELECT count(*) FROM recorded_terminals"),
            Some(1)
        );
        assert_eq!(
            store.query_i64("SELECT retained_from_ms FROM recording_evidence"),
            None
        );
        Ok(())
    }

    #[test]
    fn attribution_never_borrows_a_different_branch_or_default_account() -> TestResult {
        let context = fixture_context()?;
        let checkpoint = plakous::model::CheckpointId::try_from(uuid::Uuid::from_u128(20))?;
        let mut record = TurnAttribution {
            locator: Some(plakous::sessions::SessionLocator::new(
                context.session_id(),
                context.branch_id(),
                checkpoint,
            )),
            account: Some("openai/work".into()),
        };
        let mut terminal = plakous_hosted::sessions::projector::ProjectedTurnUsage {
            session_id: context.session_id(),
            branch_id: context.branch_id(),
            turn_id: context.turn_id().ok_or("turn")?,
            committed_at_ms: 1000,
            usage: TurnUsage::default(),
        };
        assert_eq!(
            account_for(&record, &terminal).map(|value| value.as_str().to_owned()),
            Some("openai/work".into())
        );
        terminal.branch_id = plakous::model::BranchId::try_from(uuid::Uuid::from_u128(21))?;
        assert_eq!(account_for(&record, &terminal), None);
        terminal.branch_id = context.branch_id();
        record.account = None;
        assert_eq!(account_for(&record, &terminal), None);
        Ok(())
    }
}
