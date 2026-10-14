//! Facts a topology tool call leaves for its host's transcript rows (spec 02
//! §5.4), keyed by the call id.
//!
//! The topology tools run in the host's process. What a `wait_agent` or
//! `adopt_candidate` call means for the transcript (what woke the wait, how
//! many files an adopt landed, every conflicting path) is read here, at the
//! tool boundary, from the whole result. The host takes it when the call's
//! terminal event arrives, so no row is derived from the bounded result
//! preview the model-facing observation carries. Process-local and never
//! persisted: the rows built from these facts are what persists.

use std::collections::VecDeque;
use std::sync::Mutex;

/// Facts no host has taken are dropped oldest first past this many entries
/// or bytes. Every in-process agent's topology calls leave facts and only
/// the hosts that project those calls take them, so the bytes bound keeps
/// untaken whole wait results from pinning memory.
const MAX_UNTAKEN: usize = 256;
const MAX_UNTAKEN_BYTES: usize = 4 * 1024 * 1024;

struct Board {
    entries: VecDeque<(String, TopologyCallFacts, usize)>,
    bytes: usize,
}

static BOARD: Mutex<Board> = Mutex::new(Board {
    entries: VecDeque::new(),
    bytes: 0,
});

impl Board {
    fn pop_front(&mut self) {
        if let Some((_, _, size)) = self.entries.pop_front() {
            self.bytes = self.bytes.saturating_sub(size);
        }
    }

    fn remove(&mut self, index: usize) -> Option<TopologyCallFacts> {
        let (_, facts, size) = self.entries.remove(index)?;
        self.bytes = self.bytes.saturating_sub(size);
        Some(facts)
    }
}

/// What one topology tool call tells its host beyond its model-facing result.
#[derive(Clone, Debug, PartialEq)]
pub enum TopologyCallFacts {
    /// A `wait_agent` result, whole.
    Wait(serde_json::Value),
    /// An `adopt_candidate` landed this many changed files.
    Adopted {
        /// Files the landing changed.
        files_changed: u32,
    },
    /// An `adopt_candidate` refused with `merge_conflict` on these paths.
    AdoptConflict {
        /// Every conflicting path, as the VCS named it.
        paths: Vec<String>,
    },
}

impl TopologyCallFacts {
    /// The bytes these facts hold, for the board's bound.
    fn size(&self) -> usize {
        match self {
            Self::Wait(result) => result.to_string().len(),
            Self::Adopted { .. } => size_of::<u32>(),
            Self::AdoptConflict { paths } => paths.iter().map(String::len).sum(),
        }
    }
}

/// Leaves `facts` under the call that produced them, replacing any earlier
/// facts of the same call (a replayed operation). The newest facts always
/// stay, even alone past the bytes bound.
pub(crate) fn leave(call_id: &str, facts: TopologyCallFacts) {
    let size = call_id.len().saturating_add(facts.size());
    let Ok(mut board) = BOARD.lock() else {
        return;
    };
    if let Some(index) = board.entries.iter().position(|(id, _, _)| id == call_id) {
        let _ = board.remove(index);
    }
    while !board.entries.is_empty()
        && (board.entries.len() >= MAX_UNTAKEN
            || board.bytes.saturating_add(size) > MAX_UNTAKEN_BYTES)
    {
        board.pop_front();
    }
    board.bytes = board.bytes.saturating_add(size);
    board.entries.push_back((call_id.to_owned(), facts, size));
}

/// Takes the facts `call_id` left, once.
#[must_use]
pub fn take(call_id: &str) -> Option<TopologyCallFacts> {
    let mut board = BOARD.lock().ok()?;
    let index = board.entries.iter().position(|(id, _, _)| id == call_id)?;
    board.remove(index)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use super::*;

    #[test]
    fn facts_are_taken_once_and_a_replay_replaces_them() {
        leave("call-a", TopologyCallFacts::Adopted { files_changed: 1 });
        leave("call-a", TopologyCallFacts::Adopted { files_changed: 2 });
        assert_eq!(
            take("call-a"),
            Some(TopologyCallFacts::Adopted { files_changed: 2 })
        );
        assert_eq!(take("call-a"), None);
    }

    /// Facts nobody takes (another agent's calls) are bounded by bytes as
    /// well as entries: the oldest drop, the newest stay takeable.
    #[test]
    fn untaken_facts_are_bounded_by_bytes() {
        let large = serde_json::Value::String("x".repeat(MAX_UNTAKEN_BYTES / 8));
        for index in 0..32 {
            leave(
                &format!("untaken-{index}"),
                TopologyCallFacts::Wait(large.clone()),
            );
        }
        let held = BOARD.lock().unwrap().bytes;
        assert!(held <= MAX_UNTAKEN_BYTES, "{held} bytes held");
        assert_eq!(take("untaken-0"), None);
        assert_eq!(take("untaken-31"), Some(TopologyCallFacts::Wait(large)));
    }
}
