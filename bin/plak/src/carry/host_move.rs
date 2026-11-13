//! Export-v13 ownership-move data. Ordinary import never accepts this kind.
use plakous::sessions::host_move::{MoveBinding, MoveHeadRequest, MoveOperation};
use plakous_hosted::sessions::host_move::DormantSessionDocument;
use plakous_recursive_subagents::tree::host_move::DormantTree;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

mod admission;
mod configuration;
mod context;
mod jobs;
mod presentation;
#[cfg(test)]
mod producer;
mod reconstruction;
#[cfg(test)]
mod tests;
mod validation;
pub(crate) use admission::{probe, read_if_move};
pub(crate) use context::Compaction;
pub(crate) use jobs::{Jobs, Terminal};
pub(crate) use presentation::Presentation;

/// Exactly one connected group; no checkpoint/component bytes are present.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Capsule {
    pub(crate) format: String,
    pub(crate) version: u32,
    pub(crate) kind: String,
    pub(crate) operation: [u8; 32],
    pub(crate) group: String,
    pub(crate) source_key: String,
    pub(crate) source_binary: String,
    pub(crate) source_host: String,
    pub(crate) source_instance: String,
    pub(crate) products: Vec<DormantSessionDocument>,
    pub(crate) trees: Vec<Tree>,
    pub(crate) sessions: Vec<Session>,
    pub(crate) lines: Vec<Line>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Session {
    pub(crate) id: String,
    pub(crate) initial_branch: String,
    pub(crate) revision: u64,
    pub(crate) created_seconds: u64,
    pub(crate) created_nanos: u32,
    pub(crate) durability: Durability,
    pub(crate) feed_floor: u64,
    pub(crate) heads: Vec<Head>,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Durability {
    Disabled,
    BestEffort,
    Required,
}

impl Durability {
    pub(crate) const fn decode(self) -> plakous::sessions::host_move::MoveDurability {
        use plakous::sessions::host_move::MoveDurability;
        match self {
            Self::Disabled => MoveDurability::Disabled,
            Self::BestEffort => MoveDurability::BestEffort,
            Self::Required => MoveDurability::Required,
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Head {
    pub(crate) source: String,
    pub(crate) revision: u64,
    pub(crate) fence_floor: u64,
    pub(crate) line: String,
    pub(crate) generation: u64,
    pub(crate) fork: Option<Fork>,
    pub(crate) context: HeadContext,
    pub(crate) binding: Binding,
    pub(super) history: super::history::History,
    pub(crate) named_state: BTreeMap<String, serde_json::Value>,
    pub(crate) presentation: Presentation,
    pub(crate) jobs: Jobs,
    pub(crate) compaction: Option<Compaction>,
    pub(crate) interpreter: Option<plakous::sessions::host_move::MoveInterpreter>,
}

/// Configuration provenance needed to rebuild a root or child, not an executable grant.
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HeadContext {
    pub(crate) product: String,
    pub(crate) tree: String,
    pub(crate) agent: String,
    pub(crate) workspace: std::path::PathBuf,
    pub(crate) runner: String,
    pub(crate) plan_authority: String,
    pub(crate) configuration: configuration::Configuration,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Fork {
    pub(crate) session: String,
    pub(crate) branch: String,
    pub(crate) checkpoint: String,
    pub(crate) revision: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Binding {
    pub(crate) protocol: String,
    pub(crate) route: String,
    pub(crate) model: String,
    pub(crate) account: String,
    pub(crate) selected: plakous::model::SelectedProviderConfigurationId,
    pub(crate) effective: plakous::model::EffectiveProviderConfigurationId,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Tree {
    pub(crate) cut: DormantTree,
    pub(crate) custody: Custody,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Custody {
    pub(crate) fence: u64,
    pub(crate) host: String,
    pub(crate) instance: String,
    pub(crate) binary: Option<String>,
    pub(crate) machine: String,
    pub(crate) claimed_ms: u64,
    pub(crate) renewed_ms: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Line {
    pub(crate) id: String,
    pub(crate) session: String,
    pub(crate) branch: String,
    pub(crate) generation: u64,
    pub(crate) origin: Origin,
    pub(crate) source_owner: Option<RunnerOwner>,
    pub(crate) terminals: Vec<Terminal>,
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Origin {
    Created,
    Imported { provenance: String },
    Forked { line: String, source: String },
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RunnerOwner {
    pub(crate) runner: String,
    pub(crate) host: String,
    pub(crate) line: String,
    pub(crate) generation: u64,
    pub(crate) session: String,
    pub(crate) branch: String,
    pub(crate) fence: u64,
}

impl Binding {
    pub(crate) fn decode(self) -> Result<MoveBinding, String> {
        Ok(MoveBinding {
            protocol: plakous::model::ProtocolId::new(self.protocol).map_err(|e| e.to_string())?,
            route: plakous::model::RouteId::new(self.route).map_err(|e| e.to_string())?,
            model: plakous::model::ModelId::new(self.model).map_err(|e| e.to_string())?,
            selected: self.selected,
            effective: self.effective,
            account: self.account,
        })
    }
}

impl Fork {
    pub(crate) fn decode(&self) -> Result<plakous::sessions::host_move::MoveFork, String> {
        Ok(plakous::sessions::host_move::MoveFork {
            session_id: self.session.parse().map_err(|_| "invalid fork session")?,
            branch_id: self.branch.parse().map_err(|_| "invalid fork branch")?,
            checkpoint_id: self
                .checkpoint
                .parse()
                .map_err(|_| "invalid fork checkpoint")?,
            branch_revision: self.revision,
        })
    }
}

impl Head {
    pub(crate) fn into_request(self, operation: MoveOperation) -> Result<MoveHeadRequest, String> {
        let source: plakous::sessions::SessionLocator = self
            .source
            .parse()
            .map_err(|_| "invalid move source locator")?;
        Ok(MoveHeadRequest {
            operation,
            source,
            source_revision: self.revision,
            source_fork: self.fork.map(|fork| fork.decode()).transpose()?,
            fence_floor: self
                .fence_floor
                .checked_add(1)
                .filter(|floor| *floor != u64::MAX)
                .ok_or("move fence overflow")?,
            line: self.line.parse().map_err(|_| "invalid move line")?,
            generation: self.generation,
            binding: self.binding.decode()?,
            history: self
                .history
                .import()
                .map_err(|e| e.to_string())?
                .with_named_state(self.named_state)
                .map_err(|e| e.to_string())?,
            jobs: self.jobs.decode()?,
            compaction: self.compaction.map(Compaction::decode).transpose()?,
            interpreter: self.interpreter,
        })
    }
}

impl Capsule {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.format != super::document::FORMAT
            || self.version != super::VERSION
            || self.kind != "host_upgrade"
            || self.group.is_empty()
            || !super::upgrade::state_key_valid(&self.source_key)
            || !super::upgrade::binary_id_valid(&self.source_binary)
            || self.products.is_empty()
            || self.products.len() > 4096
            || self.sessions.is_empty()
            || self.sessions.len() > 1024
            || self.lines.is_empty()
            || self.lines.len() > 16384
        {
            return Err("invalid ownership-move capsule header or inventory".into());
        }
        MoveOperation::new(self.operation).map_err(|e| e.to_string())?;
        if self.source_host.is_empty()
            || self.source_host.len() > 128
            || !self.source_host.bytes().all(|byte| byte.is_ascii_graphic())
            || plakous_recursive_subagents::tree::HostInstance::parse(&self.source_instance)
                .is_err()
        {
            return Err("invalid move source owner".into());
        }
        let mut products = BTreeSet::new();
        for product in &self.products {
            if !products.insert(product.id()) {
                return Err("duplicate move product identity".into());
            }
            product.clone().into_cut()?;
        }
        let mut sessions = BTreeSet::new();
        let mut heads = BTreeSet::new();
        for session in &self.sessions {
            if !sessions.insert(&session.id)
                || session.feed_floor >= i64::MAX.cast_unsigned()
                || session.heads.is_empty()
                || session.heads.len() > 1024
            {
                return Err("invalid or duplicate move runtime session".into());
            }
            let id: plakous::model::SessionId =
                session.id.parse().map_err(|_| "invalid move session")?;
            plakous::model::Timestamp::new(session.created_seconds, session.created_nanos)
                .map_err(|e| e.to_string())?;
            let mut initial = false;
            for head in &session.heads {
                let source: plakous::sessions::SessionLocator =
                    head.source.parse().map_err(|_| "invalid move head")?;
                if source.session_id() != id
                    || !heads.insert((source.session_id(), source.branch_id()))
                    || !products.contains(head.context.product.as_str())
                    || head.fence_floor >= u64::MAX - 1
                    || head.generation == 0
                    || head.generation == u64::MAX
                    || head.revision == u64::MAX
                {
                    return Err("invalid or duplicate move head identity".into());
                }
                initial |= source.branch_id().to_string() == session.initial_branch;
                head.history.validate_size().map_err(|e| e.to_string())?;
                head.presentation.validate()?;
            }
            if !initial {
                return Err("move runtime initial branch is missing".into());
            }
        }
        let mut trees = BTreeSet::new();
        for tree in &self.trees {
            tree.cut.validate()?;
            if !trees.insert(tree.cut.tree_id())
                || tree.custody.host != self.source_host
                || tree.custody.fence >= i64::MAX.cast_unsigned()
            {
                return Err("invalid move tree custody".into());
            }
        }
        let mut lines = BTreeSet::new();
        for line in &self.lines {
            let id: plakous::model::LineId = line.id.parse().map_err(|_| "invalid move line")?;
            let session: plakous::model::SessionId =
                line.session.parse().map_err(|_| "invalid line session")?;
            let branch: plakous::model::BranchId =
                line.branch.parse().map_err(|_| "invalid line branch")?;
            if !lines.insert(id)
                || !heads.contains(&(session, branch))
                || line.generation == 0
                || line.generation == u64::MAX
            {
                return Err("invalid move line membership".into());
            }
        }
        self.validate_membership()?;
        Ok(())
    }
}
