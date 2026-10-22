//! Memory and SQLite semantic-session construction through the facade.

use plakous::sessions::{
    AgentSessions, AgentSessionsError, SessionBackend, SessionLocator, SessionTranscriptPart,
    SessionTranscriptTail,
};
use std::path::Path;

pub(crate) fn surface_is_nameable() {
    let _ = std::any::type_name::<AgentSessions>();
    let _ = |binding: &plakous::sessions::SessionRuntimeBinding| -> plakous::sessions::SessionInputWindow {
        binding.input_window()
    };
    let _ = |history: plakous::sessions::PortableSessionHistory| {
        history.without_replay().map(|history| history.with_replay_validation())
    };
    let _ = |session: &plakous::sessions::OpenedSession| session.replay_stripped();
    let _ = SessionBackend::memory().and_then(AgentSessions::new);
    let _ = |path: &Path, locator: SessionLocator| {
        AgentSessions::inspect_recovery_sqlite(path, locator)
    };
    let _ = |tail: &SessionTranscriptTail, call_id| tail.composition_owner(call_id);
    let _ = |part: &SessionTranscriptPart| match part {
        SessionTranscriptPart::ToolCall {
            call_id,
            canonical_name,
            exposed_name,
            observation,
            lineage,
        } => {
            let _: &plakous::agent::ToolCallerLineageView = lineage;
            let _ = (
                call_id,
                canonical_name,
                exposed_name,
                observation,
                lineage.parent_call_id(),
                lineage.cell_id(),
            );
        }
        SessionTranscriptPart::ToolResult {
            call_id,
            observation,
            lineage,
        } => {
            let _: &plakous::agent::ToolCallerLineageView = lineage;
            let _ = (
                call_id,
                observation,
                lineage.parent_call_id(),
                lineage.cell_id(),
            );
        }
        _ => {}
    };
    feed_surface_is_nameable();
}

/// The change feed is supported inventory: its types, reads and the
/// `feed_seq` accessors on acknowledgements, results, events and outcomes.
fn feed_surface_is_nameable() {
    use plakous::sessions::feed::{
        FeedCall, FeedCallOutcome, FeedCallRequest, FeedCallTarget, FeedChange, FeedCursor,
        FeedFact, FeedItemsOutcome, FeedItemsRequest, FeedJobTerminal, FeedPage, FeedPageRequest,
        FeedRecord, FeedSeq, FeedSubscription, FeedTerminal, FeedTerminalOutcome,
        FeedTerminalRequest, MAX_FEED_ITEMS, MAX_FEED_PAGE_RECORDS,
    };
    let _ = (
        std::any::type_name::<FeedCall>(),
        std::any::type_name::<FeedCallOutcome>(),
        std::any::type_name::<FeedChange>(),
        std::any::type_name::<FeedFact>(),
        std::any::type_name::<FeedItemsOutcome>(),
        std::any::type_name::<FeedJobTerminal>(),
        std::any::type_name::<FeedPage>(),
        std::any::type_name::<FeedTerminal>(),
        std::any::type_name::<FeedTerminalOutcome>(),
        MAX_FEED_ITEMS,
        MAX_FEED_PAGE_RECORDS,
    );
    let _ = |record: &FeedRecord| (record.header().seq, record.facts().len());
    async fn reads(
        sessions: &AgentSessions,
        session: plakous::model::SessionId,
        items: FeedItemsRequest,
        call: FeedCallRequest,
        terminal: FeedTerminalRequest,
    ) -> Result<FeedSeq, AgentSessionsError> {
        let _ = sessions
            .feed_page(FeedPageRequest::after(FeedCursor::start(session)))
            .await;
        let _ = sessions.feed_items(items).await;
        let _ = sessions.feed_call(call).await;
        let _ = sessions.feed_terminal(terminal).await;
        let _ = FeedCallTarget::Operation(1);
        let mut subscription: FeedSubscription = sessions.subscribe_feed(session).await?;
        subscription.changed().await
    }
    let _ = reads;
    let _ = |path: &Path,
             page: FeedPageRequest,
             items: &FeedItemsRequest,
             call: &FeedCallRequest,
             terminal: &FeedTerminalRequest| {
        let _ = AgentSessions::inspect_feed_sqlite(path, page);
        let _ = AgentSessions::inspect_feed_items_sqlite(path, items);
        let _ = AgentSessions::inspect_feed_call_sqlite(path, call);
        let _ = AgentSessions::inspect_feed_terminal_sqlite(path, terminal);
    };
    let _ = |ack: &plakous::agent::CommandAck,
             result: &plakous::agent::TurnResult,
             event: &plakous::agent::AgentEvent,
             compacted: &plakous::sessions::CompactSessionOutcome,
             forked: &plakous::sessions::ForkSessionOutcome,
             rebound: &plakous::sessions::RebindSessionOutcome| {
        let _: [Option<FeedSeq>; 6] = [
            ack.feed_seq(),
            result.feed_seq(),
            event.feed_seq(),
            compacted.feed_seq(),
            forked.feed_seq(),
            rebound.feed_seq(),
        ];
    };
}

#[allow(dead_code)]
pub(crate) fn memory_sessions() -> Result<AgentSessions, AgentSessionsError> {
    AgentSessions::new(SessionBackend::memory()?)
}

#[allow(dead_code)]
pub(crate) fn sqlite_sessions(path: &Path) -> Result<AgentSessions, AgentSessionsError> {
    AgentSessions::new(SessionBackend::sqlite(path)?)
}
