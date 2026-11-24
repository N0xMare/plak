//! Protocol 13 directory projection; no listener selects it.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use plakd_wire_types::v13;
use plakous_host_protocol::wire::{SessionActivity, SessionEnvironment};
use plakous_hosted::host_service::wire::{DirectorySlot, SessionHost};
use plakous_hosted::runtime::operator_attention::AttentionReason as RuntimeReason;
use plakous_hosted::runtime::{ResumeAfterLoss, RootStanding};
use plakous_recursive_subagents_store::SqliteTreeStore;
use tokio::sync::watch;

use crate::home::paths::PlakPaths;
use crate::store::directory_notice::local::TreeFacts;
use crate::store::directory_projection::{
    DirectoryActivity, DirectoryMetadata,
    cost::{Costs, Member},
};
use crate::surface_protocol::PlakExtension;

use super::{Directory, Summary, runner_views};

pub(super) struct Source {
    pub(super) costs: Costs,
    pub(super) previews: BTreeMap<String, Option<v13::SessionPreview>>,
}

pub(super) struct Context<'a> {
    pub(super) tree: Option<&'a TreeFacts>,
    pub(super) head: bool,
    pub(super) remote: bool,
    pub(super) policy: ResumeAfterLoss,
}

#[derive(Clone, Eq, PartialEq)]
pub(super) struct Facts {
    preview: Option<v13::SessionPreview>,
    cost_root: v13::Cost,
    cost_tree: v13::TreeCost,
    attention: Candidates,
    actionable: bool,
    root: Member,
    head: bool,
}

impl Facts {
    pub(super) fn measure(&self, costs: &mut Costs, tree: Option<&TreeFacts>) -> Option<Self> {
        let children = tree
            .filter(|_| self.head)
            .map(|tree| tree.children.as_slice());
        let (cost_root, cost_tree) = costs.read(self.root, children);
        ((cost_root, cost_tree) != (self.cost_root, self.cost_tree)).then(|| Self {
            cost_root,
            cost_tree,
            ..self.clone()
        })
    }
}

impl Source {
    pub(super) fn new(paths: &PlakPaths) -> Self {
        let mut costs = Costs::default();
        costs.poll(paths);
        Self {
            costs,
            previews: BTreeMap::new(),
        }
    }

    pub(super) fn project(
        &mut self,
        id: &str,
        meta: &DirectoryMetadata,
        activity: &DirectoryActivity,
        context: Context<'_>,
    ) -> Facts {
        let Context {
            tree,
            head,
            remote,
            policy,
        } = context;
        let actionable = head && !meta.archived;
        let root = Member {
            locator: meta.locator,
            started_ms: meta.created_at.saturating_mul(1000),
            turns: activity.root_turns,
            remote: remote || activity.retention_gap,
        };
        let children = tree.filter(|_| head).map(|tree| tree.children.as_slice());
        let (cost_root, cost_tree) = self.costs.read(root, children);
        let mut attention = Candidates::default();
        if actionable {
            if let Some(root) = activity.root {
                let since = match root {
                    RuntimeReason::RecoveryRequired | RuntimeReason::ContextCapacity => {
                        activity.pause_since
                    }
                    _ => None,
                };
                attention.add(Kind::runtime(root), since);
            }
            if let (Some(since), Some(account)) = (activity.quota_since, meta.account.as_deref())
                && let Some(account) =
                    crate::telemetry_store::recorded_usage::exact_account(account)
            {
                attention.add(Kind::UsageLimit(account.as_str().to_owned()), Some(since));
            }
            if matches!(
                activity.standing,
                RootStanding::Continuing { handoff: true, .. }
            ) {
                attention.add(Kind::InterruptedByUpgrade, None);
            }
            if let Some(tree) = tree {
                if let Some(child) = tree.child_attention(policy) {
                    attention.add(Kind::ChildNeedsOperator, child.since);
                }
                for (account, since) in &tree.child_limits {
                    attention.add(Kind::UsageLimit(account.clone()), Some(*since));
                }
            }
        }
        Facts {
            preview: self.previews.get(id).cloned().flatten(),
            cost_root,
            cost_tree,
            attention,
            actionable,
            root,
            head,
        }
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Kind {
    ApprovalNeeded,
    CredentialWait,
    ContextCapacity,
    RecoveryRequired,
    RunnerUnavailable,
    ChildNeedsOperator,
    UsageLimit(String),
    InterruptedByUpgrade,
}

impl Kind {
    const fn runtime(reason: RuntimeReason) -> Self {
        match reason {
            RuntimeReason::ContextCapacity => Self::ContextCapacity,
            RuntimeReason::HeldChild | RuntimeReason::ChildRecovery => Self::ChildNeedsOperator,
            _ => Self::RecoveryRequired,
        }
    }

    fn wire(self, since: u64) -> v13::AttentionReason {
        use v13::AttentionReason as A;
        match self {
            Self::ApprovalNeeded => A::ApprovalNeeded { since },
            Self::CredentialWait => A::CredentialWait { since },
            Self::ContextCapacity => A::ContextCapacity { since },
            Self::RecoveryRequired => A::RecoveryRequired { since },
            Self::RunnerUnavailable => A::RunnerUnavailable { since },
            Self::ChildNeedsOperator => A::ChildNeedsOperator { since },
            Self::UsageLimit(account) => A::UsageLimit { account, since },
            Self::InterruptedByUpgrade => A::InterruptedByUpgrade { since },
        }
    }
}

#[derive(Clone, Default, Eq, PartialEq)]
struct Candidates(BTreeMap<Kind, Option<u64>>);

impl Candidates {
    fn add(&mut self, kind: Kind, since: Option<u64>) {
        self.0
            .entry(kind)
            .and_modify(|current| {
                *current = current.iter().copied().chain(since).min();
            })
            .or_insert(since);
    }
}

type Occurrences = BTreeMap<String, BTreeMap<Kind, u64>>;

#[derive(Default)]
pub(super) struct Stamps(Mutex<Occurrences>);

fn publish_attention(
    previous: &mut BTreeMap<Kind, u64>,
    facts: Candidates,
    now: impl Fn() -> u64,
) -> Vec<v13::AttentionReason> {
    let next = facts
        .0
        .into_iter()
        .map(|(kind, since)| {
            let since = since
                .or_else(|| previous.get(&kind).copied())
                .unwrap_or_else(&now);
            (kind, since)
        })
        .collect::<BTreeMap<_, _>>();
    let output = next
        .iter()
        .map(|(kind, since)| kind.clone().wire(*since))
        .collect();
    *previous = next;
    output
}

// Transport activation replaces the v12 call sites with this shared projection.
#[allow(dead_code, reason = "unserved until the coordinated protocol switch")]
impl Directory {
    pub(in crate::host) fn start_draft(
        paths: &PlakPaths,
        tree: Arc<SqliteTreeStore>,
    ) -> plakous_hosted::error::CliResult<Self> {
        Self::start_with_draft(paths, tree, true)
    }

    pub(in crate::host) fn draft_rows(
        &self,
        workspace: Option<&str>,
        runner: Option<&str>,
        include_archived: bool,
        runners: &[runner_views::RunnerRow],
        host: &SessionHost<PlakExtension>,
    ) -> Vec<v13::SessionRow> {
        self.project_draft_rows(
            workspace,
            runner,
            include_archived,
            runners,
            &host.directory_slots(),
        )
    }

    pub(in crate::host) fn watch_draft(&self, host: &SessionHost<PlakExtension>) -> Watch {
        Watch::new(self.changes(), host.directory_slot_changes())
    }

    fn project_draft_rows(
        &self,
        workspace: Option<&str>,
        runner: Option<&str>,
        include_archived: bool,
        runners: &[runner_views::RunnerRow],
        slots: &BTreeMap<String, DirectorySlot>,
    ) -> Vec<v13::SessionRow> {
        let mut stamps = self
            .draft_attention
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let rows = self.select(workspace, runner, include_archived, |summary| {
            let previous = stamps.entry(summary.row.session.clone()).or_default();
            draft_row(summary, runners, slots.get(&summary.row.session), previous)
        });
        let directory = self.rows.read().unwrap_or_else(PoisonError::into_inner);
        stamps.retain(|session, stamp| !stamp.is_empty() && directory.contains_key(session));
        rows
    }
}

/// Compare full draft rows, including changes that leave the old bool untouched.
pub(in crate::host) struct Watch {
    product: watch::Receiver<u64>,
    slots: watch::Receiver<u64>,
    previous: Option<Vec<v13::SessionRow>>,
}

#[allow(dead_code, reason = "unserved until the coordinated protocol switch")]
impl Watch {
    fn new(product: watch::Receiver<u64>, slots: watch::Receiver<u64>) -> Self {
        Self {
            product,
            slots,
            previous: None,
        }
    }

    pub(in crate::host) fn snapshot(
        &mut self,
        directory: &Directory,
        include_archived: bool,
        runners: &[runner_views::RunnerRow],
        host: &SessionHost<PlakExtension>,
    ) -> Option<Vec<v13::SessionRow>> {
        self.refresh(|| directory.draft_rows(None, None, include_archived, runners, host))
    }

    fn refresh(
        &mut self,
        list: impl FnOnce() -> Vec<v13::SessionRow>,
    ) -> Option<Vec<v13::SessionRow>> {
        let dirty = self.product.has_changed().unwrap_or(false)
            || self.slots.has_changed().unwrap_or(false);
        if !dirty && self.previous.is_some() {
            return None;
        }
        let _ = self.product.borrow_and_update();
        let _ = self.slots.borrow_and_update();
        let next = list();
        if self.previous.as_ref() == Some(&next) {
            return None;
        }
        self.previous = Some(next.clone());
        Some(next)
    }
}

fn draft_row(
    summary: &Summary,
    runners: &[runner_views::RunnerRow],
    slot: Option<&DirectorySlot>,
    previous: &mut BTreeMap<Kind, u64>,
) -> Option<v13::SessionRow> {
    let draft = summary.draft.as_ref()?;
    let row = &summary.row;
    let id = &row.runner.runner_id;
    let status = runner_views::v13::status(id, runners);
    let mut facts = draft.attention.clone();
    if draft.actionable {
        if matches!(status, v13::RunnerStatus::Blocked { .. }) {
            let since = runners
                .iter()
                .find(|runner| runner.runner.runner_id == *id)
                .and_then(|runner| runner.blocked_since);
            facts.add(Kind::RunnerUnavailable, since);
        }
        if slot.is_some_and(|slot| slot.open_failed) {
            facts.add(Kind::RecoveryRequired, None);
        }
    }
    let mut hosted_elsewhere = row.hosted_elsewhere.clone();
    let mut environment = row.environment;
    let mut activity = row.activity;
    let resident = slot.and_then(|slot| slot.resident.as_ref());
    if let Some((live, attention)) = resident {
        hosted_elsewhere = None;
        environment = SessionEnvironment::Host;
        if live.busy {
            activity = SessionActivity::Busy;
        }
        if draft.actionable {
            for (kind, since) in [
                (Kind::ApprovalNeeded, attention.approval),
                (Kind::CredentialWait, attention.credential),
                (Kind::ContextCapacity, attention.capacity),
            ] {
                if let Some(since) = since {
                    facts.add(kind, Some(since));
                }
            }
            if let Some((account, since)) = &attention.usage_limit {
                facts.add(Kind::UsageLimit(account.clone()), Some(*since));
            }
            if let Some(reason) = attention.runtime {
                facts.add(Kind::runtime(reason), None);
            }
            if attention.child {
                facts.add(Kind::ChildNeedsOperator, None);
            }
        }
    }
    Some(v13::SessionRow {
        session: row.session.clone(),
        node: row.node.clone(),
        title: row.title.clone(),
        workspace: row.workspace.clone(),
        runner: runner_views::label(id, runners),
        runner_status: status,
        attention: publish_attention(previous, facts, plakous::model::now_unix_secs),
        preview: draft.preview.clone(),
        cost_root: draft.cost_root,
        cost_tree: draft.cost_tree,
        activity_seq: row.activity_seq,
        archived: row.archived,
        profile: row.profile.clone(),
        differences: row.differences.clone(),
        created_at: row.created_at,
        updated_at: row.updated_at,
        residency: slot.map_or(row.residency, |slot| slot.residency),
        activity,
        live_work: super::super::directory_work_v13::live_work(
            resident.map(|(kept, _)| (kept, kept.running_children)),
        ),
        environment,
        hosted_elsewhere,
    })
}

pub(super) fn preview(raw: String) -> Option<v13::SessionPreview> {
    let redacted = plakous::tools::preview_redaction::text(&raw);
    let mut plain = String::new();
    let mut rest: &str = &redacted;
    while let Some(ch) = rest.chars().next() {
        rest = &rest[ch.len_utf8()..];
        match ch {
            '\u{1b}' if rest.starts_with('[') => {
                rest = &rest[1..];
                if let Some(end) = rest.find(|ch: char| ('@'..='~').contains(&ch)) {
                    rest = &rest[end + 1..];
                }
            }
            '<' if rest.starts_with(|ch: char| ch.is_ascii_alphabetic() || ch == '/') => {
                if let Some(end) = rest.find('>') {
                    rest = &rest[end + 1..];
                } else {
                    plain.push(ch);
                }
            }
            '[' if rest
                .split_once("](")
                .is_some_and(|(_, target)| target.contains(')')) =>
            {
                if let Some((label, target)) = rest.split_once("](")
                    && let Some((_, remaining)) = target.split_once(')')
                {
                    plain.push_str(label);
                    rest = remaining;
                }
            }
            '`' | '*' | '_' | '~' | '#' => {}
            ch if ch.is_control() || ch.is_whitespace() => plain.push(' '),
            _ => plain.push(ch),
        }
    }
    let text = plakous::tools::preview_redaction::text(&plain)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let cut = text.floor_char_boundary(v13::SESSION_PREVIEW_BYTES.min(text.len()));
    let text = text[..cut].trim_end();
    (!text.is_empty())
        .then(|| v13::SessionPreview::try_from(text.to_owned()).ok())
        .flatten()
}

#[cfg(test)]
mod tests;
