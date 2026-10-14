//! Recover command admission sequences without adding checkpoint authority.

use crate::feed::{FeedChange, FeedFact, FeedPage, FeedSeq};
use crate::runtime::checkpoint::io::{StoreCallError, StorePhase, store_call};
use crate::runtime::checkpoint::owner::DurableBranchFacts;
use crate::{AgentError, CheckpointId, CommandId, RuntimeCheckpoint, SessionStoreError};
use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroUsize;

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
    let mut checkpoint = authority.id();
    let mut upper = None;
    while !wanted.is_empty() {
        let receipt = match store_call(
            StorePhase::RecoverCommit,
            branch.store_timeouts().operation,
            branch.store().recover_commit(checkpoint),
        )
        .await
        {
            Ok(committed) => committed.receipt(),
            Err(error) if error.is(SessionStoreError::NotFound) => {
                // Genesis and fork/rebind publications have no commit receipt.
                let current = store_call(
                    StorePhase::LoadCheckpoint,
                    branch.store_timeouts().operation,
                    branch.store().load_checkpoint(checkpoint),
                )
                .await;
                match current {
                    Ok(current) => {
                        let topology = current.checkpoint().manifest().topology();
                        let Some(parent) = topology.parent_checkpoint_id else {
                            break;
                        };
                        if parent == checkpoint {
                            return Err(AgentError::from_store(SessionStoreError::corrupt()));
                        }
                        if topology
                            .fork
                            .is_some_and(|fork| fork.checkpoint_id == parent)
                        {
                            checkpoint = parent;
                            continue;
                        }
                    }
                    Err(error)
                        if error.is(SessionStoreError::Pruned)
                            || error.is(SessionStoreError::NotFound) => {}
                    Err(error) => return Err(error.into_agent()),
                }
                retained_feed_ancestry(
                    authority,
                    branch,
                    checkpoint,
                    upper,
                    &mut wanted,
                    &mut sequences,
                )
                .await?;
                break;
            }
            Err(error) if error.is(SessionStoreError::Pruned) => {
                retained_feed_ancestry(
                    authority,
                    branch,
                    checkpoint,
                    upper,
                    &mut wanted,
                    &mut sequences,
                )
                .await?;
                break;
            }
            Err(error) => return Err(error.into_agent()),
        };
        let seq = receipt.feed_seq();
        let parent = receipt.previous().checkpoint().checkpoint_id();
        if receipt.checkpoint().checkpoint_id() != checkpoint
            || parent == checkpoint
            || upper.is_some_and(|upper| seq.get() > upper)
        {
            return Err(AgentError::from_store(SessionStoreError::corrupt()));
        }
        let page = store_call(
            StorePhase::ReadFeed,
            branch.store_timeouts().operation,
            branch.store().read_feed(
                authority.manifest().topology().session_id,
                FeedSeq::new(seq.get() - 1),
                NonZeroUsize::MIN,
            ),
        )
        .await
        .map_err(StoreCallError::into_agent)?;
        let FeedPage::Records { records, .. } = page else {
            break;
        };
        let Some(record) = records.first() else {
            return Err(AgentError::from_store(SessionStoreError::corrupt()));
        };
        if record.header().seq != seq
            || !matches!(record.header().change,
            FeedChange::Advanced { checkpoint: published, parent: recorded_parent, .. }
                if published == checkpoint && recorded_parent == parent)
        {
            return Err(AgentError::from_store(SessionStoreError::corrupt()));
        }
        collect_sequences(record.facts(), seq, &mut wanted, &mut sequences);
        checkpoint = parent;
        upper = Some(seq.get() - 1);
    }
    Ok(sequences)
}

async fn retained_feed_ancestry(
    authority: &RuntimeCheckpoint,
    branch: &DurableBranchFacts,
    mut checkpoint: CheckpointId,
    upper: Option<u64>,
    wanted: &mut BTreeSet<String>,
    sequences: &mut BTreeMap<CommandId, FeedSeq>,
) -> Result<(), AgentError> {
    let session = authority.manifest().topology().session_id;
    let timeout = branch.store_timeouts().operation;
    let mut upper = if let Some(upper) = upper {
        upper
    } else {
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
    };
    let mut retained_from = 1;
    while !wanted.is_empty() && upper >= retained_from {
        let lower = upper.saturating_sub(256).max(retained_from - 1);
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
                next_upper = Some(record.header().seq.get() - 1);
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
