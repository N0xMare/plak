//! The root rebind entry points: a profile fork or switch, and the account
//! continue (spec 02 §4.4.1), over one shared rebind body.

use super::{
    AttachedCodingSession, CliFailure, CliResult, HostConfig, HostLifecyclePhase, Instant,
    InterruptWatch, ProfileBranchInput, ProfileTargetInput, Rebound, Sender, TargetSelection,
    attach_profile_branch, frozen_tools_rebind_refusal, instruction_sources,
    interrupted_before_dispatch, note_reread_instructions, observe_phase, profile_cards_active,
    profile_rebind_selection, refuse_unready_rebind, release_profile_build_lease,
    resolve_profile_target, sessions, source_skills_on, stage_rebind,
};

/// What a rebind does with the root node.
#[derive(Clone, Copy)]
pub(super) enum RebindMode<'a> {
    /// A profile fork: a new line with a tree of its own frozen on `roster`.
    Fork(&'a crate::orchestration::RosterInput, AccountChoices<'a>),
    /// Switch now: the root continues its line as the named profile.
    Switch(
        &'a str,
        &'a crate::orchestration::RosterInput,
        AccountChoices<'a>,
    ),
    /// The root continues its line on another account of its kind with the
    /// same model, effort, composition, target, epoch and profile (spec 02
    /// §4.4.1). The operation is the switch's derived workflow id.
    Account(&'a str),
}

/// The non-root account choices a profile's fill gives the successor;
/// `None` keeps the source's.
pub(super) type AccountChoices<'a> = Option<&'a std::collections::BTreeMap<String, String>>;

impl RebindMode<'_> {
    /// The root node keeps its line, its running processes and its children.
    pub(super) const fn continues_line(self) -> bool {
        !matches!(self, Self::Fork(..))
    }

    /// Whether the cards this rebind would freeze are the tree's own already.
    fn cards_active<P: plakous_host_protocol::ProtocolExtension>(
        self,
        config: &HostConfig<P>,
    ) -> CliResult<bool> {
        match self {
            Self::Fork(roster, _) | Self::Switch(_, roster, _) => {
                profile_cards_active(config, roster)
            }
            Self::Account(_) => Ok(true),
        }
    }

    /// The profile a successor that opens a tree of its own freezes; an
    /// account continue keeps the line head's.
    fn root_profile<P: plakous_host_protocol::ProtocolExtension>(
        self,
        config: &HostConfig<P>,
    ) -> sessions::RootProfile {
        match self {
            Self::Fork(roster, _) | Self::Switch(_, roster, _) => {
                sessions::RootProfile::from_roster(roster)
            }
            Self::Account(_) => config.session.root_profile.clone().unwrap_or_default(),
        }
    }

    pub(super) fn account_choices(self) -> Option<std::collections::BTreeMap<String, String>> {
        match self {
            Self::Fork(_, choices) | Self::Switch(_, _, choices) => choices.cloned(),
            Self::Account(_) => None,
        }
    }
}

/// Forks the attached session onto a profile's root: the branch opens a
/// tree of its own frozen on `roster`, so the source keeps its tree and cards.
#[cfg(test)]
#[allow(
    clippy::too_many_arguments,
    reason = "Profile rebinding receives the attached source, interrupt watch, target tuple, and frozen roster as distinct authorities."
)]
pub(in crate::runtime) async fn handle_rebind<P: plakous_host_protocol::ProtocolExtension>(
    watch: InterruptWatch<'_, P>,
    config: &mut HostConfig<P>,
    opened: &mut AttachedCodingSession,
    stderr_buf: &mut Vec<u8>,
    events: &Sender<P>,
    account: &str,
    model: &str,
    effort: Option<&str>,
    extra: Option<&str>,
    composition: Option<crate::binding::CompositionSelection>,
    protocol: Option<crate::binding::ProtocolSelection>,
    roster: &crate::orchestration::RosterInput,
    switch: Option<&str>,
) -> CliResult<Rebound> {
    handle_profile_rebind(
        watch,
        config,
        opened,
        stderr_buf,
        events,
        ProfileRebind {
            account,
            model,
            effort,
            extra,
            composition,
            protocol,
            roster,
            switch,
            account_choices: None,
        },
    )
    .await
}

/// One profile's root binding for [`handle_profile_rebind`].
pub(in crate::runtime) struct ProfileRebind<'a> {
    pub(in crate::runtime) account: &'a str,
    pub(in crate::runtime) model: &'a str,
    pub(in crate::runtime) effort: Option<&'a str>,
    pub(in crate::runtime) extra: Option<&'a str>,
    pub(in crate::runtime) composition: Option<crate::binding::CompositionSelection>,
    pub(in crate::runtime) protocol: Option<crate::binding::ProtocolSelection>,
    pub(in crate::runtime) roster: &'a crate::orchestration::RosterInput,
    /// `Some(profile)` continues the root's line as that profile.
    pub(in crate::runtime) switch: Option<&'a str>,
    /// The profile's filled non-root account choices for the successor.
    pub(in crate::runtime) account_choices: AccountChoices<'a>,
}

/// [`handle_rebind`] with the profile's account fill.
pub(in crate::runtime) async fn handle_profile_rebind<
    P: plakous_host_protocol::ProtocolExtension,
>(
    watch: InterruptWatch<'_, P>,
    config: &mut HostConfig<P>,
    opened: &mut AttachedCodingSession,
    stderr_buf: &mut Vec<u8>,
    events: &Sender<P>,
    profile: ProfileRebind<'_>,
) -> CliResult<Rebound> {
    let ProfileRebind {
        account,
        model,
        effort,
        extra,
        composition,
        protocol,
        roster,
        switch,
        account_choices,
    } = profile;
    let mode = switch.map_or(RebindMode::Fork(roster, account_choices), |profile| {
        RebindMode::Switch(profile, roster, account_choices)
    });
    let selection =
        profile_rebind_selection(config, account, model, effort, extra, composition, protocol);
    rebind_root(
        watch,
        config,
        opened,
        stderr_buf,
        events,
        RootRebind {
            account,
            model,
            selection,
            mode,
        },
    )
    .await
}

/// Continues the root on `account` (`ChooseAccount` for the root kind): an
/// ordinary rebind workflow that keeps every other binding fact and the
/// tree's epoch, published under `operation`.
pub(in crate::runtime) async fn handle_account_rebind<
    P: plakous_host_protocol::ProtocolExtension,
>(
    watch: InterruptWatch<'_, P>,
    config: &mut HostConfig<P>,
    opened: &mut AttachedCodingSession,
    stderr_buf: &mut Vec<u8>,
    events: &Sender<P>,
    account: &str,
    operation: &str,
) -> CliResult<Rebound> {
    let Some(model) = config.session.model.clone() else {
        return Err(CliFailure::new(
            "the root has no model to continue on another account",
        ));
    };
    // The model, effort, composition and target are the line head's own;
    // the target account's resolution admits them against its catalog.
    let target_selection = config.session.target_selection.clone();
    let api_root = target_selection.is_api();
    let selection = Ok((
        target_selection,
        api_root,
        config.session.effort.clone(),
        config.session.composition,
    ));
    rebind_root(
        watch,
        config,
        opened,
        stderr_buf,
        events,
        RootRebind {
            account,
            model: &model,
            selection,
            mode: RebindMode::Account(operation),
        },
    )
    .await
}

/// The root binding a rebind moves to and what it does with the root node.
struct RootRebind<'a> {
    account: &'a str,
    model: &'a str,
    selection: CliResult<(
        TargetSelection,
        bool,
        Option<String>,
        crate::binding::CompositionSelection,
    )>,
    mode: RebindMode<'a>,
}

async fn rebind_root<P: plakous_host_protocol::ProtocolExtension>(
    watch: InterruptWatch<'_, P>,
    config: &mut HostConfig<P>,
    opened: &mut AttachedCodingSession,
    stderr_buf: &mut Vec<u8>,
    events: &Sender<P>,
    rebind: RootRebind<'_>,
) -> CliResult<Rebound> {
    let RootRebind {
        account,
        model,
        selection,
        mode,
    } = rebind;
    let rebound: CliResult<Rebound> = Box::pin(async {
        // A switch hands running processes to the successor on the same
        // line; a fork leaves them with the source.
        if !mode.continues_line() {
            crate::runtime::jobs_transition_guard(config, opened)?;
        }
        watch.begin();
        let started = Instant::now();
        refuse_unready_rebind(config, mode.continues_line())?;
        let account = account.to_owned();
        // A Code Mode (native-ptc) prefix is replayable onto every presentation:
        // each kernel re-projects `exec`/`wait` cells truthfully at dispatch
        // (Responses/Chat carry the original string input, Messages wraps it as
        // `{"input": ...}`), so the rebind branches at the canonical checkpoint
        // like any other and nothing is refused up front.
        let (target_selection, api_root, effort_label, composition) = selection?;
        // refuse before the first resolution round-trip,
        // while no durable rebind work exists to roll back.
        if watch.observe_now() {
            return Err(interrupted_before_dispatch("rebind"));
        }
        let same_binding = config.session.account.as_deref() == Some(account.as_str())
            && config.session.model.as_deref() == Some(model)
            && config.session.effort.as_deref() == effort_label.as_deref()
            && config.session.composition == composition
            && config.session.target_selection == target_selection;
        if same_binding && mode.cards_active(config)? {
            return Ok(Rebound::Unchanged);
        }
        if let Some(refusal) = frozen_tools_rebind_refusal(config, &account, &target_selection) {
            return Err(CliFailure::new(refusal)
                .with_remedy(plakous_host_protocol::notice_remedy::NoticeRemedy::StartFresh {}));
        }
        // A rebind or switch keeps canonical tool authority: its successor
        // inherits whether the source's skills are on, whatever tuple it
        // moves to, and never takes the live setting (spec 02 §6.1.1).
        let inherited = source_skills_on(opened).await?;
        config.state_roots.set_inherited_skills(inherited);
        let (operation, target_product_id, own_tree, topology_tree_id, succession) =
            stage_rebind(config, opened, mode, &account, model, composition).await?;
        let profile = mode.root_profile(config);
        let target = resolve_profile_target(
            watch,
            config,
            ProfileTargetInput {
                api_root,
                account: &account,
                model,
                effort_label: &effort_label,
                composition,
                target_selection: &target_selection,
                target_product_id: &target_product_id,
                operation: &operation,
                topology_tree_id: &topology_tree_id,
            },
        )
        .await?;
        observe_phase(
            events,
            HostLifecyclePhase::RebindResolveTarget,
            started,
            config.session.pending_queue.len(),
        );
        let instructions_before = instruction_sources(opened);
        let mut attached_tree = None;
        let branched = Box::pin(attach_profile_branch(
            watch,
            config,
            opened,
            stderr_buf,
            events,
            ProfileBranchInput {
                target,
                own_tree,
                profile,
                topology_tree_id: &topology_tree_id,
                effort_label: &effort_label,
                attached_tree: &mut attached_tree,
                started,
                account: &account,
                model,
                composition,
                target_selection,
                operation: &operation,
                succession: succession.as_ref(),
                account_choices: mode.account_choices(),
            },
        ))
        .await;
        if let Some(tree_id) = attached_tree {
            release_profile_build_lease(events, &tree_id).await?;
        }
        if matches!(branched, Ok(Rebound::Switched | Rebound::Succeeded)) {
            note_reread_instructions(events, instructions_before, opened);
        }
        branched
    })
    .await;
    finish_root_rebind(config, events, &rebound);
    rebound
}

/// What every rebind settles once it ends, published or not.
fn finish_root_rebind<P: plakous_host_protocol::ProtocolExtension>(
    config: &mut HostConfig<P>,
    events: &Sender<P>,
    rebound: &CliResult<Rebound>,
) {
    config.state_roots.set_inherited_skills(None);
    config.state_roots.set_pending_epoch(None);
    // A switch onto another tree takes it for this root, so it runs the
    // custody-attach scan as a host's open does.
    if matches!(rebound, Ok(Rebound::Switched | Rebound::Succeeded)) {
        crate::runtime::tree::finish_custody_attach_scan(
            config,
            events,
            crate::runtime::tree::HeldNotice::CHILDREN,
        );
    }
}
