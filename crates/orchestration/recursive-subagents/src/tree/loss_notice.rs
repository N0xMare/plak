//! A parent's loss notice (spec 03 section 14.1, what the parent learns):
//! the children a lost host interrupted, named once per host generation to
//! the parent's next turn, in the process-notice encoding. Nothing is
//! stored: which children this process already named is held by the tree's
//! driver, so the next host generation names them again.

use super::{ContinuationCause, LossContinuationState, ProductAgentTree};
use std::collections::BTreeSet;

/// One child a parent's loss notice named: `(agent id, attempt id)`.
pub type LossNoticeKey = (String, String);

/// How many children one notice names before it counts the rest.
const NOTICE_NAMED: usize = 16;

/// Opens a loss notice after the command's text and mail.
const MARKER: &str = "\n\n[harness]\n";
const HELD: &str = "held";
const CONTINUING: &str = "continuing";

fn notice_element(agent_id: &str, continuation: &str) -> String {
    let next = if continuation == HELD {
        "It waits for the operator to resume it."
    } else {
        "It continues by itself from its last completed step."
    };
    format!(
        "<agent_notice agent_id=\"{agent_id}\" cause=\"owner_lost\" continuation=\"{continuation}\">\n\
         The host running this sub-agent was lost, which interrupted it. Its attempt and its \
         work are kept, and no result was sent for the interrupted work. {next}\n\
         </agent_notice>"
    )
}

fn more_line(more: usize) -> String {
    format!("{more} more sub-agents were interrupted the same way; wait_agent lists each.")
}

/// What a command's trailing loss notice named, for a transcript row.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LossNoticeSummary {
    /// Each named child with `held` or `continuing`, in order.
    pub named: Vec<(String, String)>,
    /// How many more the notice counted without naming.
    pub more: usize,
}

impl LossNoticeSummary {
    /// One line for a transcript notice row.
    #[must_use]
    pub fn row(&self) -> String {
        let mut named = self
            .named
            .iter()
            .map(|(agent_id, continuation)| format!("{agent_id} ({continuation})"))
            .collect::<Vec<_>>();
        if self.more > 0 {
            named.push(format!("{} more", self.more));
        }
        format!("a lost host interrupted {}", named.join(", "))
    }
}

/// Splits a command whose text ends with a loss notice into the text before
/// it and what it named. Only a block that is exactly what
/// [`ProductAgentTree::loss_notice`] writes, through to the end, counts.
/// `None` for text that carries none.
#[must_use]
pub fn split_loss_notice(text: &str) -> Option<(&str, LossNoticeSummary)> {
    text.rmatch_indices(MARKER).find_map(|(start, _)| {
        let summary = parse_notice(&text[start + MARKER.len()..])?;
        Some((&text[..start], summary))
    })
}

fn parse_notice(mut rest: &str) -> Option<LossNoticeSummary> {
    let mut summary = LossNoticeSummary::default();
    while let Some(element) = rest.strip_prefix("<agent_notice agent_id=\"") {
        let (agent_id, element) = element.split_once("\" cause=\"owner_lost\" continuation=\"")?;
        let (continuation, _) = element.split_once('"')?;
        if continuation != HELD && continuation != CONTINUING {
            return None;
        }
        rest = rest.strip_prefix(notice_element(agent_id, continuation).as_str())?;
        summary
            .named
            .push((agent_id.to_owned(), continuation.to_owned()));
        if rest.is_empty() {
            return Some(summary);
        }
        rest = rest.strip_prefix('\n')?;
    }
    let more = rest.split_once(' ')?.0.parse::<usize>().ok()?;
    (!summary.named.is_empty() && rest == more_line(more))
        .then_some(LossNoticeSummary { more, ..summary })
}

impl ProductAgentTree {
    /// `parent`'s loss notice: each child of `parent` a lost host interrupted
    /// (cause `owner_lost`) whose continuation still stands and which
    /// `noticed` does not hold yet, with the keys to hold once it is
    /// delivered. A handed-off child continues by itself and is not named.
    /// `None` when there is nothing new to say.
    #[must_use]
    pub fn loss_notice(
        &self,
        parent: &str,
        noticed: &BTreeSet<LossNoticeKey>,
    ) -> Option<(String, Vec<LossNoticeKey>)> {
        let lost = self
            .nodes
            .values()
            .filter(|node| node.parent_agent_id.as_deref() == Some(parent))
            .filter_map(|node| {
                let continuation = self.loss_continuation(&node.agent_id)?;
                let key = (node.agent_id.clone(), continuation.attempt_id.to_string());
                (continuation.cause == ContinuationCause::OwnerLost && !noticed.contains(&key))
                    .then_some((key, continuation.state))
            })
            .collect::<Vec<_>>();
        if lost.is_empty() {
            return None;
        }
        let mut elements = lost
            .iter()
            .take(NOTICE_NAMED)
            .map(|((agent_id, _), state)| {
                notice_element(
                    agent_id,
                    match state {
                        LossContinuationState::Held => HELD,
                        LossContinuationState::Pending | LossContinuationState::Running => {
                            CONTINUING
                        }
                    },
                )
            })
            .collect::<Vec<_>>();
        if lost.len() > NOTICE_NAMED {
            elements.push(more_line(lost.len() - NOTICE_NAMED));
        }
        let text = format!("{MARKER}{}", elements.join("\n"));
        Some((text, lost.into_iter().map(|(key, _)| key).collect()))
    }
}

#[cfg(test)]
mod tests;
