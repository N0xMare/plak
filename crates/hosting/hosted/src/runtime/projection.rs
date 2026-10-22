//! The host's one projector driver (spec 02 §2.6, §4.2): it applies a
//! session's feed to its tail, page by page, one product transaction per
//! page under the cursor CAS, and runs the tree projector over the same feed.
//! Open (lease-free before the runtime open and again after it) and the live
//! loop call the same entry points.

use std::sync::Arc;

use plakous::model::SessionId;
use plakous::sessions::AgentSessions;
use plakous::sessions::feed::{FeedCursor, FeedLocator, FeedPage, FeedPageRequest, FeedSeq};

use crate::error::ReadFailure;
use crate::session_store::SessionRecordStore;
use crate::sessions::SessionRecord;
use crate::sessions::projector::{
    LineageStep, ProjectionCommit, ReadItem, SessionViews, ViewWrites, read_item,
    rebuild_behind_retention,
};

use super::topology_state::TopologyRows;

#[cfg(test)]
pub(crate) mod probe;

/// Most pages one pass applies. A concurrent applier that keeps moving the
/// cursor costs a page each; the bound only stops a runaway loop, and the next
/// pass continues from the stored cursor.
const MAX_PASS_PAGES: usize = 4_096;

pub(in crate::runtime) async fn queue_join(
    store: &Arc<dyn SessionRecordStore>,
    product_id: &str,
) -> Result<crate::sessions::QueueCommands, String> {
    blocking(store, product_id, |store, id| {
        let intents = store.load_intents(id)?;
        let views = store.load_views(id)?;
        Ok(crate::sessions::QueueCommands::from_join(
            &intents,
            views.as_ref(),
        ))
    })
    .await
}

pub(in crate::runtime) async fn queue_after_ack(
    sessions: &AgentSessions,
    store: &Arc<dyn SessionRecordStore>,
    product_id: &str,
    followed: plakous::sessions::SessionLocator,
) -> Result<crate::sessions::QueueCommands, String> {
    tokio::time::timeout(
        super::host_loop::FINAL_PROJECTION_BOUND,
        after_ack_pass(sessions, store, product_id, followed),
    )
    .await
    .unwrap_or_else(|_| Err("the after-ack projector pass exceeded its bound".to_owned()))
}

/// The pass `queue_after_ack` bounds: the feed to its tail, then the queue
/// those views and the stored intents name.
async fn after_ack_pass(
    sessions: &AgentSessions,
    store: &Arc<dyn SessionRecordStore>,
    product_id: &str,
    followed: plakous::sessions::SessionLocator,
) -> Result<crate::sessions::QueueCommands, String> {
    let views = project_to_tail(sessions, store, product_id, followed).await?;
    let intents = blocking(store, product_id, |store, id| store.load_intents(id)).await?;
    Ok(crate::sessions::QueueCommands::from_join(
        &intents,
        Some(&views),
    ))
}

/// Step 0 of the reopen (spec 02 §4.2): a lease-free projector pass to the
/// feed's tail. Returns the head the views now name on the record's branch,
/// which is what the runtime opens; `None` for a session with no runtime yet
/// or no stored record.
///
/// # Errors
///
/// The pass's refusal; nothing past the last committed page is applied.
pub(crate) async fn project_before_open(
    sessions: &AgentSessions,
    store: &Arc<dyn SessionRecordStore>,
    session: &SessionRecord,
) -> Result<Option<plakous::sessions::SessionLocator>, ReadFailure> {
    let Some(recorded) = session.plakous_locator else {
        return Ok(None);
    };
    if session.store_revision == 0 {
        return Ok(None);
    }
    let pass = project_to_tail(sessions, store, &session.id, recorded);
    #[cfg(test)]
    let pass = probe::CALLER.scope(probe::Caller::Open, pass);
    let views = pass.await?;
    Ok(Some(projected_head(recorded, &views)))
}

/// A product session after a lease-free projector pass to its feed tail.
#[derive(Clone, Debug)]
pub struct ProjectedSession {
    /// The head the views name on the record's branch.
    pub head: plakous::sessions::SessionLocator,
    /// Command submissions the feed holds no fact for, by command id.
    pub pending_commands: Vec<String>,
    /// Workflow requests the feed holds no fact for, by operation id.
    pub pending_workflows: Vec<String>,
}

/// The lease-free projector pass `plak session export` and `plak session gc`
/// run first (spec 02 §2.5.1, §2.5.3): applies `session`'s feed to its tail
/// and names the head the views now hold and the intents no fact settled.
/// `None` for a session with no runtime yet or no stored record. Safe beside
/// a live owner: every write is a product transaction under the cursor CAS.
///
/// # Errors
///
/// The pass's refusal; nothing past the last committed page is applied.
pub async fn project_session(
    sessions: &AgentSessions,
    store: &Arc<dyn SessionRecordStore>,
    session: &SessionRecord,
) -> Result<Option<ProjectedSession>, String> {
    let Some(recorded) = session.plakous_locator else {
        return Ok(None);
    };
    if session.store_revision == 0 {
        return Ok(None);
    }
    let views = project_to_tail(sessions, store, &session.id, recorded).await?;
    let intents = blocking(store, &session.id, |store, id| store.load_intents(id)).await?;
    let head = projected_head(recorded, &views);
    Ok(Some(ProjectedSession {
        head,
        pending_commands: crate::sessions::projector::pending_command_ids(&views, &intents),
        pending_workflows: crate::sessions::projector::pending_workflow_operation_ids(
            &views, &intents,
        ),
    }))
}

/// Selects the exact head open follows after its projector pass. Views of
/// another branch never replace the session's construction binding.
#[doc(hidden)]
#[must_use]
pub fn projected_head(
    recorded: plakous::sessions::SessionLocator,
    views: &SessionViews,
) -> plakous::sessions::SessionLocator {
    views.projected_locator(Some(recorded)).unwrap_or(recorded)
}

/// The head the views name.
pub(crate) fn head_locator(views: &SessionViews) -> Option<plakous::sessions::SessionLocator> {
    views.state.head.map(|head| {
        plakous::sessions::SessionLocator::new(head.session_id, head.branch_id, head.checkpoint_id)
    })
}

/// Applies `runtime_session`'s feed to product session `product_id`'s views
/// through the tail observed in the pass. Needs no lease: every write is a
/// product transaction under the cursor CAS, so a concurrent or late applier
/// changes nothing and both converge.
///
/// # Errors
///
/// A refused feed read, an unappliable page or a store failure; the stored
/// views and cursor stay where the last committed page left them.
pub(crate) async fn project_to_tail(
    sessions: &AgentSessions,
    store: &Arc<dyn SessionRecordStore>,
    product_id: &str,
    followed: plakous::sessions::SessionLocator,
) -> Result<SessionViews, ReadFailure> {
    project_to_tail_cached(sessions, store, product_id, followed, None)
        .await
        .map(|projected| projected.views)
}

pub(super) async fn project_to_tail_interruptible(
    sessions: &AgentSessions,
    store: &Arc<dyn SessionRecordStore>,
    product_id: &str,
    followed: plakous::sessions::SessionLocator,
    interrupted: impl Fn() -> bool,
    progress: impl Fn(FeedSeq, FeedSeq),
) -> Result<SessionViews, ReadFailure> {
    project_to_tail_controlled(
        sessions,
        store,
        product_id,
        followed,
        None,
        interrupted,
        progress,
    )
    .await
    .map(|projected| projected.views)
}

/// Owned projector state. Same-cursor workflow joins in the store may be newer;
/// this is not a fresh snapshot of the store's joined workflow bookkeeping.
pub(super) struct ProjectedViews {
    pub(super) views: SessionViews,
    /// None after a store reload: reconcile all retained settled rows once.
    changed: Option<std::collections::BTreeSet<crate::sessions::projector::RowKey>>,
}

async fn project_to_tail_cached(
    sessions: &AgentSessions,
    store: &Arc<dyn SessionRecordStore>,
    product_id: &str,
    followed: plakous::sessions::SessionLocator,
    cached: Option<SessionViews>,
) -> Result<ProjectedViews, ReadFailure> {
    project_to_tail_controlled(
        sessions,
        store,
        product_id,
        followed,
        cached,
        || false,
        |_, _| {},
    )
    .await
}

async fn project_to_tail_controlled(
    sessions: &AgentSessions,
    store: &Arc<dyn SessionRecordStore>,
    product_id: &str,
    followed: plakous::sessions::SessionLocator,
    cached: Option<SessionViews>,
    interrupted: impl Fn() -> bool,
    progress: impl Fn(FeedSeq, FeedSeq),
) -> Result<ProjectedViews, ReadFailure> {
    #[cfg(test)]
    let _entry = probe::enter(product_id);
    let mut changed = cached.as_ref().map(|_| std::collections::BTreeSet::new());
    let runtime_session = followed.session_id();
    let mut views = match cached {
        Some(views) => views,
        None => load_views(store, product_id).await?,
    };
    let inherited = blocking(store, product_id, |store, id| {
        store.load_inherited_submissions(id)
    })
    .await?;
    for _ in 0..MAX_PASS_PAGES {
        if interrupted() {
            return Err("projection interrupted between pages".to_owned().into());
        }
        // A target's atomic join can seed only a source's completion at zero.
        // Its branch lineage still comes from the source's publishing records.
        if views.state.lineage.is_empty() {
            views.state.lineage =
                seed_lineage(sessions, runtime_session, followed.branch_id()).await?;
        }
        if views
            .state
            .runtime_session_id
            .is_some_and(|followed| followed != runtime_session)
        {
            return Err("the product session's views follow another runtime session"
                .to_owned()
                .into());
        }
        let (page, intents) = page_with_intents(store, product_id, &inherited, async {
            sessions
                .feed_page(FeedPageRequest::after(FeedCursor::new(
                    runtime_session,
                    views.cursor,
                )))
                .await
                .map_err(|error| {
                    ReadFailure::store("the projector could not read the feed", &error)
                })
        })
        .await?;
        let tail = match &page {
            FeedPage::Records { tail, .. } | FeedPage::BehindRetention { tail, .. } => *tail,
            _ => return Err("the projector read an unknown feed page".to_owned().into()),
        };
        progress(views.cursor, tail);
        let (mut next, writes, terminal_usages) = match page {
            FeedPage::Records {
                records,
                retained_from,
                ..
            } if records.is_empty() => {
                let previous = views.state.clone();
                if retained_from.get() > 1 {
                    views.state.retained_from = Some(retained_from.get());
                }
                views.consume_before_retention(&intents);
                if views.state != previous {
                    let mut writes =
                        ViewWrites::between(&SessionViews::empty(), &SessionViews::empty());
                    writes.expected_cursor = views.cursor;
                    writes.cursor = views.cursor;
                    let (outcome, next) = blocking(store, product_id, move |store, id| {
                        let outcome = store.commit_projection_views(id, &writes, &views.state)?;
                        Ok((outcome, views))
                    })
                    .await?;
                    views = next;
                    if outcome == ProjectionCommit::CursorMoved {
                        changed = None;
                        views = load_views(store, product_id).await?;
                        continue;
                    }
                }
                let (outcome, next) = blocking(store, product_id, move |store, id| {
                    let outcome = rejoin_clear_source(store, id, &views)?;
                    Ok((outcome, views))
                })
                .await?;
                views = next;
                if outcome == ProjectionCommit::CursorMoved {
                    changed = None;
                    views = load_views(store, product_id).await?;
                    continue;
                }
                for request in &intents.workflow_requests {
                    if request.kind != crate::sessions::SessionWorkflowKind::Clear
                        || intents
                            .workflow_consumed(&request.operation_id, views.state.retained_from)
                        || views.state.workflows.contains_key(&request.operation_id)
                    {
                        continue;
                    }
                    let target_id = request.successor_product_id();
                    let target = match blocking(store, &target_id, |store, id| store.load(id)).await
                    {
                        Ok(target) => target,
                        Err(error) if error == "session not found" => continue,
                        Err(error) => return Err(error.into()),
                    };
                    let operation =
                        plakous::sessions::SessionOperationId::new(&target.create_operation_id)
                            .map_err(|error| error.to_string())?;
                    let locator = AgentSessions::genesis_locator_for(&operation)
                        .map_err(|error| error.to_string())?;
                    let page = match sessions
                        .feed_page(FeedPageRequest::after(FeedCursor::new(
                            locator.session_id(),
                            FeedSeq::ZERO,
                        )))
                        .await
                    {
                        Ok(page) => page,
                        Err(error)
                            if error.kind()
                                == plakous::sessions::AgentSessionsErrorKind::NotFound =>
                        {
                            continue;
                        }
                        Err(error) => return Err(error.to_string().into()),
                    };
                    // An absent genesis has no admission fact and is withdrawn
                    // by open; never retry create from an unresolved intent.
                    if matches!(page, FeedPage::Records { ref records, .. } if records.is_empty()) {
                        continue;
                    }
                    let target_views =
                        Box::pin(project_to_tail(sessions, store, &target_id, locator)).await?;
                    if let Some(completion) = crate::sessions::projector::clear_completion(
                        request,
                        &target,
                        &target_views.state,
                    ) {
                        views
                            .state
                            .workflows
                            .insert(request.operation_id.clone(), completion);
                    }
                }
                return Ok(ProjectedViews { views, changed });
            }
            FeedPage::Records {
                records,
                retained_from,
                ..
            } => {
                let mut projection =
                    crate::sessions::projector::run(sessions, views, &intents, &records)
                        .await
                        .map_err(|error| error.to_string())?;
                if retained_from.get() > 1 {
                    projection.views.state.retained_from = Some(retained_from.get());
                }
                (
                    projection.views,
                    projection.writes,
                    projection.terminal_usages,
                )
            }
            FeedPage::BehindRetention {
                retained_from,
                tail,
                heads,
            } => {
                let rebuilt =
                    behind_retention(sessions, &views, followed, retained_from, tail, &heads)
                        .await?;
                let mut rebuilt = rebuilt;
                let session = blocking(store, product_id, |store, id| store.load(id)).await?;
                let operation =
                    plakous::sessions::SessionOperationId::new(&session.create_operation_id)
                        .map_err(|error| error.to_string())?;
                let genesis = AgentSessions::genesis_locator_for(&operation)
                    .map_err(|error| error.to_string())?;
                // The retained runtime session proves this keyed create committed,
                // even when its SessionCreated fact has been pruned.
                if genesis.session_id() == runtime_session {
                    rebuilt.state.created_operation_id = Some(session.create_operation_id);
                }
                let writes = full_writes(&views, &rebuilt);
                (rebuilt, writes, Vec::new())
            }
            _ => return Err("the projector read an unknown feed page".to_owned().into()),
        };
        if let Some(changed) = &mut changed {
            changed.extend(writes.upserts.iter().map(|(key, _)| *key));
            changed.extend(writes.deletes.iter().copied());
        }
        next.consume_before_retention(&intents);
        let observations = terminal_usages;
        let (outcome, next) = blocking(store, product_id, move |store, id| {
            let outcome = store.commit_projection_views(id, &writes, &next.state)?;
            if outcome == ProjectionCommit::Applied {
                store.emit_projected_turn_usages(&observations);
            }
            Ok((outcome, next))
        })
        .await?;
        match outcome {
            ProjectionCommit::Applied => views = next,
            ProjectionCommit::CursorMoved => {
                changed = None;
                views = load_views(store, product_id).await?;
            }
        }
        progress(views.cursor, tail);
    }
    Ok(ProjectedViews { views, changed })
}

async fn load_views(
    store: &Arc<dyn SessionRecordStore>,
    product_id: &str,
) -> Result<SessionViews, String> {
    Ok(
        blocking(store, product_id, |store, id| store.load_views(id))
            .await?
            .unwrap_or_else(SessionViews::empty),
    )
}

async fn page_with_intents(
    store: &Arc<dyn SessionRecordStore>,
    product_id: &str,
    inherited: &[crate::sessions::projector::Submission],
    page: impl Future<Output = Result<FeedPage, ReadFailure>>,
) -> Result<(FeedPage, crate::sessions::projector::ProductIntents), ReadFailure> {
    let page = page.await?;
    // Every intent precedes its submission, so a page read before this
    // snapshot cannot name an intent the snapshot missed.
    let mut intents = blocking(store, product_id, |store, id| store.load_intents(id)).await?;
    // Ancestor submissions are immutable; only our own intent snapshot must
    // follow each page read to observe concurrent submissions.
    intents.submissions.extend(inherited.iter().cloned());
    Ok((page, intents))
}

/// An idle target only writes when its creation fact repairs a missing clear
/// settlement (for example after source view deletion). Ordinary idle passes
/// and already-joined clears must not consume a product commit.
fn rejoin_clear_source(
    store: &dyn SessionRecordStore,
    product_id: &str,
    views: &SessionViews,
) -> Result<ProjectionCommit, String> {
    if views
        .state
        .created_operation_id
        .as_deref()
        .is_none_or(|operation| !operation.ends_with(":create"))
    {
        return Ok(ProjectionCommit::Applied);
    }
    let target = store.load(product_id)?;
    let (Some(source_id), Some(operation)) = (
        target.source_product_session_id.as_deref(),
        target.source_workflow_operation_id.as_deref(),
    ) else {
        return Ok(ProjectionCommit::Applied);
    };
    let intents = store.load_intents(source_id)?;
    let Some(completion) = intents.workflow_request(operation).and_then(|request| {
        crate::sessions::projector::clear_completion(request, &target, &views.state)
    }) else {
        return Ok(ProjectionCommit::Applied);
    };
    let source_views = store
        .load_views(source_id)?
        .unwrap_or_else(SessionViews::empty);
    if !intents.workflow_consumed(operation, source_views.state.retained_from)
        && source_views.state.workflows.get(operation) != Some(&completion)
    {
        return store.commit_projection_views(
            product_id,
            &ViewWrites::between(views, views),
            &views.state,
        );
    }
    Ok(ProjectionCommit::Applied)
}

/// Runs one store call on the blocking pool: a slow SQLite commit never holds
/// a runtime worker, so the background task cannot stall the host's executor.
async fn blocking<T: Send + 'static>(
    store: &Arc<dyn SessionRecordStore>,
    product_id: &str,
    call: impl FnOnce(&dyn SessionRecordStore, &str) -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let store = Arc::clone(store);
    let product_id = product_id.to_owned();
    tokio::task::spawn_blocking(move || {
        #[cfg(test)]
        return probe::store_call(&product_id, || call(&*store, &product_id));
        #[cfg(not(test))]
        call(&*store, &product_id)
    })
    .await
    .map_err(|_| "the projector's store call did not complete".to_owned())?
}

/// The branch lineage a product session's views start from when it has none
/// stored: empty for the session's first branch (its `Created` record seeds
/// it), else the chain of `Branched` records that published `branch`, so a
/// successor rebuilds from its source's records below each of them.
async fn seed_lineage(
    sessions: &AgentSessions,
    runtime_session: SessionId,
    branch: plakous::model::BranchId,
) -> Result<Vec<LineageStep>, ReadFailure> {
    let mut created = None;
    let mut published = std::collections::BTreeMap::new();
    let mut behind = false;
    let mut cursor = FeedCursor::start(runtime_session);
    loop {
        let page = sessions
            .feed_page(FeedPageRequest::after(cursor))
            .await
            .map_err(|error| ReadFailure::store("the projector could not read the feed", &error))?;
        let records = match page {
            FeedPage::Records { records, .. } => records,
            FeedPage::BehindRetention { retained_from, .. } => {
                behind = true;
                cursor = FeedCursor::new(
                    runtime_session,
                    FeedSeq::new(retained_from.get().saturating_sub(1)),
                );
                continue;
            }
            _ => return Err("the projector read an unknown feed page".to_owned().into()),
        };
        let Some(last) = records.last() else {
            break;
        };
        cursor = cursor.advanced_past(last);
        for record in &records {
            let header = record.header();
            match header.change {
                plakous::sessions::feed::FeedChange::Created { .. } => {
                    created.get_or_insert(header.branch_id);
                }
                plakous::sessions::feed::FeedChange::Branched { source, .. } => {
                    published.insert(header.branch_id, (source.branch_id, header.seq.get()));
                }
                _ => {}
            }
        }
    }
    let mut lineage = Vec::new();
    let mut current = branch;
    while Some(current) != created {
        let Some((source, seq)) = published.get(&current).copied() else {
            if behind {
                break;
            }
            return Err("the product session's branch has no publishing record"
                .to_owned()
                .into());
        };
        lineage.push(LineageStep {
            branch_id: current,
            from_seq: seq,
        });
        if lineage.len() > published.len() {
            return Err("the feed's branch lineage is cyclic".to_owned().into());
        }
        current = source;
    }
    if lineage.is_empty() && !behind {
        return Ok(lineage);
    }
    lineage.push(LineageStep {
        branch_id: current,
        from_seq: 0,
    });
    lineage.reverse();
    Ok(lineage)
}

/// The views of a session whose cursor fell behind the feed's retention,
/// seeded from its branch's current head (spec 02 §2.6).
async fn behind_retention(
    sessions: &AgentSessions,
    views: &SessionViews,
    followed: plakous::sessions::SessionLocator,
    retained_from: FeedSeq,
    tail: FeedSeq,
    heads: &[plakous::sessions::SessionLocator],
) -> Result<SessionViews, String> {
    let head = heads
        .iter()
        .find(|head| head.branch_id() == followed.branch_id())
        .copied()
        .ok_or_else(|| {
            "the feed is behind retention and names no followed branch head".to_owned()
        })?;
    let source = sessions
        .transcript_source(head)
        .await
        .map_err(|error| format!("the projector could not read the head's history: {error}"))?;
    let history: Vec<ReadItem> = source
        .tail(
            crate::sessions::MAX_TRANSCRIPT_LINES,
            crate::sessions::MAX_TRANSCRIPT_BYTES,
        )
        .items()
        .iter()
        .map(read_item)
        .collect();
    let lineage = if views.state.lineage.is_empty() {
        vec![LineageStep {
            branch_id: head.branch_id(),
            from_seq: 0,
        }]
    } else {
        views.state.lineage.clone()
    };
    Ok(rebuild_behind_retention(
        lineage,
        followed.session_id(),
        retained_from,
        tail,
        FeedLocator {
            session_id: head.session_id(),
            branch_id: head.branch_id(),
            checkpoint_id: head.checkpoint_id(),
        },
        &history,
    ))
}

/// The writes that replace `from`'s rows with `to`'s.
fn full_writes(from: &SessionViews, to: &SessionViews) -> ViewWrites {
    ViewWrites::between(from, to)
}

/// Runs the tree projector over the root session's feed past the tree's
/// cursor for it (spec 03 §14.3), and saves the tree. A session without a
/// tree has nothing to apply.
///
/// # Errors
///
/// A refused feed read, a refused page or a refused tree save; the tree's
/// cursor stays where it was and the next pass applies the same records.
pub(crate) async fn project_tree(
    sessions: &AgentSessions,
    rows: &(dyn TopologyRows + Sync),
    tree_id: &str,
    runtime_session: SessionId,
) -> Result<(), ReadFailure> {
    let session_key = runtime_session.to_string();
    let runtime = crate::topology_runtime::lookup(tree_id);
    let read = |tree: &crate::agent_tree::ProductAgentTree| {
        (tree.root_agent_id.clone(), tree.feed_cursor(&session_key))
    };
    let (root, cursor) = match &runtime {
        Some(runtime) => runtime.read(read).map_err(|error| error.message)?,
        None => match rows.tree(tree_id)? {
            Some(tree) => read(&tree),
            None => return Ok(()),
        },
    };
    let feed = crate::agent::read_tree_feed(sessions, runtime_session, cursor).await?;
    if let Some(runtime) = runtime {
        crate::agent::feed_projection::retry_tree_feed(sessions, feed, |feed| {
            crate::agent::feed_projection::apply_on_runtime(&runtime, feed, &root)
        })
        .await
        .map_err(|error| error.message)?;
        return Ok(());
    }
    let Some(mut tree) = rows.tree(tree_id)? else {
        return Ok(());
    };
    let before = tree.feed_cursor(&session_key);
    crate::agent::feed_projection::retry_tree_feed(sessions, feed, |feed| {
        feed.apply_checked(&mut tree, &root)
    })
    .await
    .map_err(|error| error.message)?;
    if tree.feed_cursor(&session_key) == before {
        return Ok(());
    }
    Ok(rows.save_tree(&mut tree)?)
}

pub(super) mod live;
pub(super) mod presentation;

#[cfg(test)]
mod tests;
