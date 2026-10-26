//! Order scripted tree work after the host acknowledges each round's prefix.

use super::super::offline_support::Fixture;
use super::summary;
use plakous::sessions::AgentSessions;
use plakous::sessions::feed::{FeedCursor, FeedFact, FeedPage, FeedPageRequest, FeedSeq};
use rusqlite::{Connection, OpenFlags, OptionalExtension as _};
use serde_json::{Value, json};

pub(super) struct RoundGates<'a> {
    fixture: &'a Fixture,
    original: Value,
    session: plakous::model::SessionId,
    tree: Connection,
    next: usize,
    total: usize,
}

impl<'a> RoundGates<'a> {
    pub(super) fn new(fixture: &'a Fixture, shape: &str) -> Option<Self> {
        if !matches!(
            shape,
            "tree_child_submit_adopt"
                | "tree_conflict_accept"
                | "tree_conflict_resolver"
                | "tree_revert"
                | "tree_refresh"
        ) {
            return None;
        }
        let original: Value =
            serde_json::from_slice(&std::fs::read(fixture.script()).unwrap()).unwrap();
        let mut script = original.clone();
        let steps = script["steps"].as_array_mut().unwrap();
        for (index, step) in steps.iter_mut().enumerate() {
            let stream = step
                .as_object_mut()
                .unwrap()
                .entry("stream")
                .or_insert_with(|| json!([]));
            stream
                .as_array_mut()
                .unwrap()
                .insert(0, json!({"gate": {"file": release(fixture, index)}}));
        }
        let total = steps.len();
        fixture.write_script(&script);
        let head: plakous::sessions::SessionLocator = summary::read(fixture).head.parse().unwrap();
        let tree = Connection::open_with_flags(
            summary::paths(fixture).subagents_database(),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .unwrap();
        Some(Self {
            fixture,
            original,
            session: head.session_id(),
            tree,
            next: 0,
            total,
        })
    }

    pub(super) fn poll(&mut self) {
        if self.next == self.total {
            return;
        }
        let FeedPage::Records { records, .. } = AgentSessions::inspect_feed_sqlite(
            summary::paths(self.fixture).runtime_database(),
            FeedPageRequest::after(FeedCursor::new(self.session, FeedSeq::new(0))),
        )
        .unwrap() else {
            panic!("scripted tree round feed must remain retained")
        };
        let prefixes: Vec<_> = records.iter().filter(|record| record.facts().iter().any(|fact| {
            matches!(fact, FeedFact::CommandAccepted { .. })
                || matches!(fact, FeedFact::HistoryAppended { tool_results, .. } if !tool_results.is_empty())
        })).map(|record| record.header().seq.get()).collect();
        let applied: i64 = self
            .tree
            .query_row(
                "SELECT applied_seq FROM agent_feed_cursors WHERE runtime_session_id=?1",
                [self.session.to_string()],
                |row| row.get(0),
            )
            .optional()
            .unwrap()
            .unwrap_or(0);
        let applied = u64::try_from(applied).unwrap();
        while self.next < self.total && prefixes.get(self.next).is_some_and(|seq| *seq <= applied) {
            std::fs::write(release(self.fixture, self.next), b"projected").unwrap();
            self.next += 1;
        }
    }
}

impl Drop for RoundGates<'_> {
    fn drop(&mut self) {
        self.fixture.write_script(&self.original);
    }
}

fn release(fixture: &Fixture, index: usize) -> std::path::PathBuf {
    fixture
        .root
        .join(format!("tree-round-{}.released", index + 1))
}
