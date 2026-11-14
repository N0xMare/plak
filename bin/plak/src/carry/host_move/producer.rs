//! Source-owned projection; native checkpoints never enter the document codec.
use super::{
    BTreeMap, BTreeSet, Binding, Capsule, Compaction, DormantSessionDocument, Durability, Fork,
    Head, HeadContext, Jobs, Line, Origin, Presentation, RunnerOwner, Session, Terminal, Tree,
};
use plakous::sessions::SessionLocator;
use plakous::sessions::host_move::{MoveDurability, MoveHeadFacts, MoveInstall, MoveLineOrigin};
use plakous_hosted::sessions::host_move::MoveGroups;

pub(super) struct SourceCut {
    pub(super) operation: [u8; 32],
    pub(super) key: String,
    pub(super) binary: String,
    pub(super) host: String,
    pub(super) instance: String,
    pub(super) products: Vec<DormantSessionDocument>,
    pub(super) trees: Vec<Tree>,
    pub(super) contexts:
        BTreeMap<(plakous::model::SessionId, plakous::model::BranchId), HeadContext>,
    pub(super) owners: BTreeMap<plakous::model::LineId, RunnerOwner>,
}

impl SourceCut {
    pub(super) fn capsule(mut self, snapshot: &MoveInstall) -> Result<Capsule, String> {
        let mut sessions = Vec::new();
        let mut dependencies = BTreeMap::<_, BTreeSet<_>>::new();
        let mut trees = BTreeMap::<_, BTreeSet<_>>::new();
        for product in &self.products {
            let source = product.source();
            let (record, _, _) = product.clone().into_cut()?.into_parts();
            trees
                .entry(record.node.tree_id)
                .or_default()
                .insert(source.session_id());
        }
        self.attach_plan_authorities()?;
        for session in snapshot.sessions() {
            let mut heads = Vec::new();
            for branch in &session.branches {
                let context = self
                    .contexts
                    .remove(&(branch.session.session_id(), branch.session.branch_id()))
                    .ok_or("source move head has no exact configuration context")?;
                let (head, current) = capture_head(
                    session,
                    branch,
                    snapshot.lines(),
                    context,
                    snapshot.job_cut(),
                )?;
                dependencies
                    .entry(session.session.session_id())
                    .or_default()
                    .insert(current);
                heads.push(head);
            }
            sessions.push(Session {
                id: session.session.session_id().to_string(),
                initial_branch: session.session.initial_branch().to_string(),
                revision: session.session.revision().get(),
                created_seconds: session.session.created_at().seconds(),
                created_nanos: session.session.created_at().nanoseconds(),
                durability: match session.session.durability() {
                    MoveDurability::Disabled => Durability::Disabled,
                    MoveDurability::BestEffort => Durability::BestEffort,
                    MoveDurability::Required => Durability::Required,
                },
                feed_floor: session.feed_tail_floor,
                heads,
            });
        }
        for tree in &self.trees {
            for node in tree.cut.nodes().values() {
                if let Some(locator) = &node.session_locator {
                    let source: SessionLocator =
                        locator.parse().map_err(|_| "invalid source node locator")?;
                    trees
                        .entry(tree.cut.tree_id().into())
                        .or_default()
                        .insert(source.session_id());
                }
            }
        }
        let groups = MoveGroups::plan(
            &plakous::model::hex_lower(&self.operation),
            &trees,
            &dependencies,
        )
        .map_err(|e| e.to_string())?;
        if groups.groups().len() != 1 {
            return Err("source cut is not one connected group".into());
        }
        let lines = snapshot
            .lines()
            .iter()
            .map(|line| capture_line(line, self.owners.remove(&line.line.line())))
            .collect::<Result<_, String>>()?;
        if !self.owners.is_empty() || !self.contexts.is_empty() {
            return Err("source move context exceeds its native inventory".into());
        }
        let capsule = Capsule {
            job_cut: super::JobCut::from_native(snapshot.job_cut()),
            format: super::super::document::FORMAT.into(),
            version: super::super::VERSION,
            kind: "host_upgrade".into(),
            operation: self.operation,
            group: groups.groups()[0].id().into(),
            source_key: self.key,
            source_binary: self.binary,
            source_host: self.host,
            source_instance: self.instance,
            products: self.products,
            trees: self.trees,
            sessions,
            lines,
        };
        capsule.validate()?;
        Ok(capsule)
    }

    fn attach_plan_authorities(&mut self) -> Result<(), String> {
        self.products = std::mem::take(&mut self.products)
            .into_iter()
            .map(|product| {
                let source = product.source();
                let context = self
                    .contexts
                    .get(&(source.session_id(), source.branch_id()))
                    .ok_or("source product has no move head context")?;
                product
                    .into_cut()?
                    .with_plan_authority(context.plan_authority.clone())?
                    .document()
            })
            .collect::<Result<_, String>>()?;
        Ok(())
    }
}

fn capture_head(
    session: &plakous::sessions::host_move::MoveSession,
    branch: &plakous::sessions::host_move::MoveBranch,
    lines: &[plakous::sessions::host_move::MoveLine],
    context: HeadContext,
    job_cut: plakous::sessions::host_move::MoveJobCut,
) -> Result<(Head, plakous::model::SessionId), String> {
    let checkpoint = session
        .checkpoints
        .iter()
        .find(|checkpoint| checkpoint.id() == branch.checkpoint)
        .ok_or("source move head is missing")?;
    let topology = checkpoint.manifest().topology();
    let line = lines
        .iter()
        .find(|line| line.line.line() == topology.line)
        .ok_or("source move line is missing")?;
    let facts =
        MoveHeadFacts::capture(checkpoint, &line.terminals, job_cut).map_err(|e| e.to_string())?;
    let source = SessionLocator::new(topology.session_id, topology.branch_id, checkpoint.id());
    let named_state = facts.history.named_state().map_err(|e| e.to_string())?;
    let history =
        super::super::history::History::export(&facts.history).map_err(|e| e.to_string())?;
    Ok((
        Head {
            source: source.to_string(),
            revision: branch.revision.get(),
            fence_floor: branch.fence_floor,
            line: topology.line.to_string(),
            generation: topology.line_generation,
            fork: topology.fork.map(|fork| Fork {
                session: fork.session_id.to_string(),
                branch: fork.branch_id.to_string(),
                checkpoint: fork.checkpoint_id.to_string(),
                revision: fork.branch_revision,
            }),
            context,
            binding: Binding {
                protocol: facts.binding.protocol.as_str().into(),
                route: facts.binding.route.as_str().into(),
                model: facts.binding.model.as_str().into(),
                account: facts.binding.account,
                selected: facts.binding.selected,
                effective: facts.binding.effective,
            },
            history,
            named_state,
            presentation: Presentation::capture(facts.presentation)?,
            jobs: Jobs::capture(&facts.jobs)?,
            compaction: facts.compaction.map(Compaction::capture),
            interpreter: facts.interpreter,
        },
        line.line.head().session_id(),
    ))
}

fn capture_line(
    line: &plakous::sessions::host_move::MoveLine,
    owner: Option<RunnerOwner>,
) -> Result<Line, String> {
    let origin = match line.line.origin() {
        MoveLineOrigin::Created => Origin::Created,
        MoveLineOrigin::Imported { provenance } => Origin::Imported {
            provenance: provenance.clone(),
        },
        MoveLineOrigin::Forked {
            line,
            source,
            checkpoint,
        } => Origin::Forked {
            line: line.to_string(),
            source: SessionLocator::new(source.session_id(), source.branch_id(), *checkpoint)
                .to_string(),
        },
    };
    Ok(Line {
        id: line.line.line().to_string(),
        session: line.line.head().session_id().to_string(),
        branch: line.line.head().branch_id().to_string(),
        generation: line.line.generation(),
        origin,
        source_owner: owner,
        terminals: line
            .terminals
            .iter()
            .map(Terminal::capture)
            .collect::<Result<_, _>>()?,
    })
}
