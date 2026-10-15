//! Recover command admission sequences without adding checkpoint authority.

use crate::feed::{FeedChange, FeedFact, FeedPage, FeedSeq};
use crate::runtime::checkpoint::io::{StoreCallError, StorePhase, store_call};
use crate::runtime::checkpoint::owner::DurableBranchFacts;
use crate::{AgentError, CommandId, RuntimeCheckpoint, SessionStoreError};
use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroUsize;

/// Walks the session feed backwards in bounded pages, following exact
/// checkpoint links from `authority`, until every command the ledger retains
/// has its admission sequence or the retained feed ends.
pub(in crate::runtime) async fn command_feed_sequences(
    authority: &RuntimeCheckpoint,
    branch: &DurableBranchFacts,
) -> Result<BTreeMap<CommandId, FeedSeq>, AgentError> {
    let mut wanted = authority
        .manifest()
        .ledger()
        .commands()
        .into_iter()
        .map(|command| command.command_id().to_owned())
        .collect::<BTreeSet<_>>();
    let mut sequences = BTreeMap::new();
    if !wanted.is_empty() {
        feed_ancestry(authority, branch, &mut wanted, &mut sequences).await?;
    }
    Ok(sequences)
}

async fn feed_ancestry(
    authority: &RuntimeCheckpoint,
    branch: &DurableBranchFacts,
    wanted: &mut BTreeSet<String>,
    sequences: &mut BTreeMap<CommandId, FeedSeq>,
) -> Result<(), AgentError> {
    let session = authority.manifest().topology().session_id;
    let timeout = branch.store_timeouts().operation;
    let mut checkpoint = authority.id();
    // The head's receipt bounds the walk below a sibling's later records;
    // genesis, fork and pruned heads start from the feed tail instead.
    let mut upper = match store_call(
        StorePhase::RecoverCommit,
        timeout,
        branch.store().recover_commit(checkpoint),
    )
    .await
    {
        Ok(committed) => {
            let receipt = committed.receipt();
            if receipt.checkpoint().checkpoint_id() != checkpoint {
                return Err(AgentError::from_store(SessionStoreError::corrupt()));
            }
            receipt.feed_seq().get()
        }
        Err(error)
            if error.is(SessionStoreError::NotFound) || error.is(SessionStoreError::Pruned) =>
        {
            let page = store_call(
                StorePhase::ReadFeed,
                timeout,
                branch
                    .store()
                    .read_feed(session, FeedSeq::new(0), NonZeroUsize::MIN),
            )
            .await
            .map_err(StoreCallError::into_agent)?;
            match page {
                FeedPage::Records { window, .. } | FeedPage::BehindRetention { window, .. } => {
                    window.tail().get()
                }
            }
        }
        Err(error) => return Err(error.into_agent()),
    };
    let mut retained_from = 1;
    while !wanted.is_empty() && upper >= retained_from {
        let lower = upper.saturating_sub(256).max(
            retained_from
                .checked_sub(1)
                .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?,
        );
        let mut after = lower;
        loop {
            let limit = NonZeroUsize::new(usize::try_from(upper - after).unwrap_or(256).min(256))
                .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
            let page = store_call(
                StorePhase::ReadFeed,
                timeout,
                branch
                    .store()
                    .read_feed(session, FeedSeq::new(after), limit),
            )
            .await
            .map_err(StoreCallError::into_agent)?;
            let records = match page {
                FeedPage::BehindRetention { window, .. } => {
                    retained_from = window.retained_from().get();
                    break;
                }
                FeedPage::Records { records, window } => {
                    retained_from = window.retained_from().get();
                    records
                }
            };
            let Some(last) = records.last() else {
                return Ok(());
            };
            let last_seq = last.header().seq.get();
            if last_seq <= after {
                return Err(AgentError::from_store(SessionStoreError::corrupt()));
            }
            let mut next_upper = None;
            // Keep only this bounded page. Exact checkpoint links select the
            // ancestry; a sibling's matching command identity is irrelevant.
            for record in records
                .iter()
                .rev()
                .filter(|record| record.header().seq.get() <= upper)
            {
                let parent = match record.header().change {
                    FeedChange::Advanced {
                        checkpoint: published,
                        parent,
                        ..
                    } if published == checkpoint => parent,
                    FeedChange::Branched { target, source, .. } if target == checkpoint => {
                        source.checkpoint_id
                    }
                    FeedChange::Created {
                        genesis: published, ..
                    } if published == checkpoint => return Ok(()),
                    _ => continue,
                };
                collect_sequences(record.facts(), record.header().seq, wanted, sequences);
                checkpoint = parent;
                next_upper = Some(
                    record
                        .header()
                        .seq
                        .get()
                        .checked_sub(1)
                        .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?,
                );
                if wanted.is_empty() {
                    return Ok(());
                }
            }
            if let Some(next) = next_upper {
                upper = next;
                break;
            }
            if last_seq >= upper {
                upper = lower;
                break;
            }
            // Byte-bounded pages can end before the requested sequence window.
            after = last_seq;
        }
    }
    Ok(())
}

fn collect_sequences(
    facts: &[FeedFact],
    seq: FeedSeq,
    wanted: &mut BTreeSet<String>,
    sequences: &mut BTreeMap<CommandId, FeedSeq>,
) {
    for fact in facts {
        let (FeedFact::CommandAccepted { command_id, .. }
        | FeedFact::CommandRejected { command_id, .. }) = fact
        else {
            continue;
        };
        if wanted.remove(command_id.as_str()) {
            sequences.insert(command_id.clone(), seq);
        }
    }
}
