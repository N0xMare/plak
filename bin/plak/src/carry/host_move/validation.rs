//! Inventory closure is checked independently of target configuration and storage.
use super::Capsule;
use plakous::model::{BranchId, LineId, SessionId};
use plakous::sessions::SessionLocator;
use plakous_hosted::sessions::host_move::MoveGroups;
use std::collections::{BTreeMap, BTreeSet};

type Trees = BTreeMap<String, BTreeSet<SessionId>>;
type Heads = BTreeMap<(SessionId, BranchId), (LineId, u64)>;

impl Capsule {
    pub(super) fn validate_membership(&self) -> Result<(), String> {
        let mut trees: BTreeMap<String, BTreeSet<SessionId>> = BTreeMap::new();
        for product in &self.products {
            let source = product.source();
            let (record, _, _) = product.clone().into_cut()?.into_parts();
            trees
                .entry(record.node.tree_id)
                .or_default()
                .insert(source.session_id());
        }
        let head_lines = self.validate_heads(&trees)?;
        self.validate_group(&mut trees, &head_lines)
    }

    fn validate_heads(&self, trees: &Trees) -> Result<Heads, String> {
        let mut head_lines = BTreeMap::new();
        let mut checkpoints = BTreeSet::new();
        for session in &self.sessions {
            for head in &session.heads {
                let locator: SessionLocator = head
                    .source
                    .parse()
                    .map_err(|_| "invalid move source head")?;
                let line: LineId = head.line.parse().map_err(|_| "invalid move head line")?;
                if !checkpoints.insert(locator.checkpoint_id())
                    || !trees.contains_key(&head.context.tree)
                    || plakous_hosted::sessions::SessionNode::parse(
                        &head.context.tree,
                        &head.context.agent,
                    )
                    .is_err()
                    || !head.context.workspace.is_absolute()
                    || plakous::tools::RunnerId::new(head.context.runner.clone()).is_err()
                    || head.context.plan_authority.is_empty()
                    || head.context.plan_authority.len() > 59
                    || !head.context.plan_authority.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_')
                    })
                {
                    return Err("invalid move head construction context".into());
                }
                if let Some(fork) = &head.fork {
                    fork.decode()?;
                }
                head_lines.insert(
                    (locator.session_id(), locator.branch_id()),
                    (line, head.generation),
                );
            }
        }
        for product in &self.products {
            let source = product.source();
            let head = self
                .sessions
                .iter()
                .flat_map(|session| &session.heads)
                .find(|head| head.source == source.to_string())
                .ok_or("move product has no exact source checkpoint")?;
            if product.plan_authority() != Some(head.context.plan_authority.as_str())
                || !head_lines.contains_key(&(source.session_id(), source.branch_id()))
                || self
                    .sessions
                    .iter()
                    .find(|session| session.id == source.session_id().to_string())
                    .is_none_or(|session| session.feed_floor != product.floor())
            {
                return Err("move product does not cover its exact native cut".into());
            }
        }
        Ok(head_lines)
    }

    fn validate_group(&self, trees: &mut Trees, head_lines: &Heads) -> Result<(), String> {
        for tree in &self.trees {
            if !trees.contains_key(tree.cut.tree_id()) {
                return Err("move tree has no carried product owner".into());
            }
            for node in tree.cut.nodes().values() {
                if let Some(locator) = &node.session_locator {
                    let locator: SessionLocator =
                        locator.parse().map_err(|_| "invalid move tree locator")?;
                    if !head_lines.contains_key(&(locator.session_id(), locator.branch_id())) {
                        return Err("move tree has an unmapped native head".into());
                    }
                    trees
                        .get_mut(tree.cut.tree_id())
                        .ok_or("move tree inventory disappeared")?
                        .insert(locator.session_id());
                }
            }
            for (session, floor) in tree.cut.feed_floors() {
                if self
                    .sessions
                    .iter()
                    .find(|native| native.id == *session)
                    .is_none_or(|native| native.feed_floor != *floor)
                {
                    return Err("move tree projection does not reach its exact native cut".into());
                }
            }
        }
        let mut current_lines = BTreeMap::new();
        for line in &self.lines {
            let id: LineId = line.id.parse().map_err(|_| "invalid move line")?;
            let session: SessionId = line
                .session
                .parse()
                .map_err(|_| "invalid move line session")?;
            let branch: BranchId = line
                .branch
                .parse()
                .map_err(|_| "invalid move line branch")?;
            if head_lines.get(&(session, branch)) != Some(&(id, line.generation)) {
                return Err("move line does not identify its exact current head".into());
            }
            current_lines.insert(id, (session, line.generation));
            if let Some(owner) = &line.source_owner {
                if owner.host != self.source_host
                    || owner.line != line.id
                    || owner.session != line.session
                    || owner.branch != line.branch
                    || owner.generation != line.generation
                    || owner.fence == 0
                    || owner.fence == u64::MAX
                    || plakous::tools::RunnerId::new(owner.runner.clone()).is_err()
                {
                    return Err("move runner owner does not name its source line".into());
                }
            }
        }
        let mut dependencies: BTreeMap<SessionId, BTreeSet<SessionId>> = BTreeMap::new();
        for ((session, _), (line, generation)) in head_lines {
            let (head, current) = current_lines
                .get(line)
                .ok_or("move branch has no current line head")?;
            if *generation > *current {
                return Err("move branch generation exceeds its line head".into());
            }
            dependencies.entry(*session).or_default().insert(*head);
        }
        let groups = MoveGroups::plan(
            &plakous::model::hex_lower(&self.operation),
            trees,
            &dependencies,
        )
        .map_err(|e| e.to_string())?;
        if groups.groups().len() != 1
            || groups.groups()[0].id() != self.group
            || groups.groups()[0].sessions().len() != self.sessions.len()
        {
            return Err("move capsule is not its exact connected group".into());
        }
        Ok(())
    }
}
