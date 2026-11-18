//! Session account choice: fill a profile's kinds, and plan a one-step switch.
//!
//! Templates name a provider kind. The root kind's account is the root line
//! head and is not stored in the choice map. Every other kind the profile uses
//! is a map entry. Creation refuses when a used kind cannot be filled;
//! import and carry fill what is Ready and leave the rest unset.

use std::collections::{BTreeMap, BTreeSet};

use plakous_host_protocol::wire::{AccountChoice, AccountOption, KindAccounts, RequestRefusal};
use plakous_hosted::account::{AccountRef, Vertical};

use crate::product_config::{AgentTemplate, ProductConfig};
use crate::store::sessions::SessionRecord;

/// The prompt submitted when a failed root turn already committed a round
/// after its prompt. Pinned: spec 02 §5.5.
pub const CONTINUATION_PROMPT: &str = "Continue the previous request from where it stopped. The tool results above are what actually ran.";

/// One catalog account the fill rule may select.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReadyAccount {
    /// Full account reference.
    pub account: String,
    /// The credential is ready for a new dispatch.
    pub ready: bool,
    /// Operator-facing label, when the catalog has one.
    pub label: Option<String>,
}

/// Whether an unfilled kind refuses the operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FillMode {
    /// Creation, ApplyProfile and fork.
    Require,
    /// Import and carry. Spawn still fails closed when nothing is Ready.
    BestEffort,
}

/// What a successful fill wrote onto the record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FilledChoices {
    /// The root kind's account, derived, not stored in the map.
    pub root_account: Option<String>,
    /// Non-root kinds only.
    pub choices: BTreeMap<String, String>,
}

/// How a `ChooseAccount` intent is carried out. No new intent kind.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SwitchPlan {
    /// A non-root kind: update the map and stop.
    MapOnly {
        /// The kind being replaced.
        kind: String,
        /// The full account reference.
        account: String,
    },
    /// The root is mid-turn. Nothing is written.
    RootBusy,
    /// The root kind: an existing rebind, then at most one root-start.
    Root {
        /// The full account the line head rebinds to.
        account: String,
        /// `Some` only when `continue_failed` asks for a root-start.
        prompt: Option<SwitchPrompt>,
        /// Stable workflow operation id for this switch.
        operation_id: String,
        /// Stable root-start command id. A repeat is the same command.
        command_id: String,
    },
}

/// The one root-start a continue-on switch may submit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SwitchPrompt {
    /// The failed turn's canonical prompt segment. No round committed after it.
    Exact(String),
    /// A round committed after the failed prompt.
    Continuation,
}

/// What reopen does with a switch that did not finish. Never an automatic prompt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SwitchRecovery {
    /// The rebind has no publication fact. Withdraw it and do not resubmit.
    WithdrawRequest,
    /// The rebind published and the root-start was not written. Notice only.
    NoticeNoPrompt,
    /// The root-start command id is already durable. Do not write another.
    KeepExistingCommand,
}

/// The kinds a profile's root and working set name.
pub fn profile_kinds(config: &ProductConfig) -> BTreeSet<String> {
    let mut kinds = BTreeSet::new();
    if let Some(name) = &config.root_agent
        && let Some(template) = config.templates.get(name)
    {
        insert_kind(&mut kinds, template);
    }
    for name in config.working_set.names() {
        if let Some(template) = config.templates.get(name.as_str()) {
            insert_kind(&mut kinds, template);
        }
    }
    kinds
}

fn insert_kind(kinds: &mut BTreeSet<String>, template: &AgentTemplate) {
    if let Ok(account) = template.account.parse::<AccountRef>()
        && !account.is_named()
    {
        kinds.insert(account.vertical().as_str().to_owned());
    }
}

/// The root template's kind, when it names one.
pub fn root_kind(config: &ProductConfig) -> Option<String> {
    let name = config.root_agent.as_ref()?;
    let template = config.templates.get(name)?;
    let account = template.account.parse::<AccountRef>().ok()?;
    (!account.is_named()).then(|| account.vertical().as_str().to_owned())
}

/// Group catalog sections into picker rows, one per kind.
pub fn kind_accounts(catalog: &[ReadyAccount]) -> Vec<KindAccounts> {
    let mut grouped: BTreeMap<String, Vec<AccountOption>> = BTreeMap::new();
    for row in catalog {
        let Ok(account) = row.account.parse::<AccountRef>() else {
            continue;
        };
        grouped
            .entry(account.vertical().as_str().to_owned())
            .or_default()
            .push(AccountOption {
                account: account.to_string(),
                ready: row.ready,
                label: row.label.clone(),
            });
    }
    grouped
        .into_iter()
        .map(|(kind, rows)| KindAccounts { kind, rows })
        .collect()
}

/// Fill `kinds` from `explicit` choices and `catalog`.
///
/// An explicit choice wins. Otherwise the kind's bare default slot if Ready,
/// otherwise the single Ready account. `Require` refuses the unfilled kinds.
/// The root kind is returned separately and is not inserted into the map.
pub fn fill(
    kinds: &BTreeSet<String>,
    root_kind: &str,
    explicit: &[AccountChoice],
    catalog: &[ReadyAccount],
    mode: FillMode,
) -> Result<FilledChoices, RequestRefusal> {
    let mut seen = BTreeSet::new();
    let mut chosen: BTreeMap<String, String> = BTreeMap::new();
    for choice in explicit {
        if !seen.insert(choice.kind.clone()) {
            return Err(RequestRefusal::AccountChoiceInvalid {
                kind: choice.kind.clone(),
                account: choice.account.clone(),
                reason: "repeated choice".to_owned(),
            });
        }
        let account = admit_choice(&choice.kind, &choice.account, catalog)?;
        chosen.insert(choice.kind.clone(), account);
    }
    let mut missing = Vec::new();
    let mut filled = FilledChoices {
        root_account: None,
        choices: BTreeMap::new(),
    };
    for kind in kinds {
        if let Some(account) = chosen.remove(kind) {
            assign(&mut filled, root_kind, kind, account);
            continue;
        }
        match automatic(kind, catalog) {
            Some(account) => assign(&mut filled, root_kind, kind, account),
            None if mode == FillMode::Require => missing.push(kind.clone()),
            None => {}
        }
    }
    for (kind, account) in chosen {
        return Err(RequestRefusal::AccountChoiceInvalid {
            kind,
            account,
            reason: "kind is not used by this profile".to_owned(),
        });
    }
    if !missing.is_empty() {
        let rows = kind_accounts(catalog);
        let kinds = missing
            .into_iter()
            .map(|kind| {
                rows.iter()
                    .find(|row| row.kind == kind)
                    .cloned()
                    .unwrap_or(KindAccounts {
                        kind,
                        rows: Vec::new(),
                    })
            })
            .collect();
        return Err(RequestRefusal::AccountChoiceRequired { kinds });
    }
    Ok(filled)
}

fn assign(filled: &mut FilledChoices, root_kind: &str, kind: &str, account: String) {
    if kind == root_kind {
        filled.root_account = Some(account);
    } else {
        filled.choices.insert(kind.to_owned(), account);
    }
}

fn admit_choice(
    kind: &str,
    account: &str,
    catalog: &[ReadyAccount],
) -> Result<String, RequestRefusal> {
    let parsed =
        account
            .parse::<AccountRef>()
            .map_err(|_| RequestRefusal::AccountChoiceInvalid {
                kind: kind.to_owned(),
                account: account.to_owned(),
                reason: "not an account reference".to_owned(),
            })?;
    if parsed.vertical().as_str() != kind || !is_kind(kind) {
        return Err(RequestRefusal::AccountChoiceInvalid {
            kind: kind.to_owned(),
            account: account.to_owned(),
            reason: "not an account of this kind".to_owned(),
        });
    }
    let ready = catalog
        .iter()
        .any(|row| row.account == parsed.as_str() && row.ready);
    if !ready {
        return Err(RequestRefusal::AccountChoiceInvalid {
            kind: kind.to_owned(),
            account: parsed.to_string(),
            reason: "not ready".to_owned(),
        });
    }
    Ok(parsed.to_string())
}

fn automatic(kind: &str, catalog: &[ReadyAccount]) -> Option<String> {
    let ready: Vec<&ReadyAccount> = catalog
        .iter()
        .filter(|row| {
            row.ready
                && row
                    .account
                    .parse::<AccountRef>()
                    .is_ok_and(|account| account.vertical().as_str() == kind)
        })
        .collect();
    if ready.iter().any(|row| row.account == kind) {
        return Some(kind.to_owned());
    }
    if ready.len() == 1 {
        return Some(ready[0].account.clone());
    }
    None
}

fn is_kind(kind: &str) -> bool {
    Vertical::ALL
        .iter()
        .any(|vertical| vertical.as_str() == kind)
}

/// Write a filled choice onto `record`. The root account replaces the line
/// head only when the fill named one. A kind change keeps the previous root
/// account in the map when that kind is still used.
pub fn apply_fill(record: &mut SessionRecord, filled: &FilledChoices, kinds: &BTreeSet<String>) {
    let previous = record.account.clone();
    if let Some(root) = &filled.root_account {
        record.account = Some(root.clone());
    }
    record.account_choices.clone_from(&filled.choices);
    if let Some(previous) = previous.as_deref() {
        record.retain_previous_root_kind(previous);
    }
    let root = record
        .account
        .as_deref()
        .and_then(|account| account.parse::<AccountRef>().ok())
        .map(|account| account.vertical().as_str().to_owned());
    record
        .account_choices
        .retain(|kind, _| kinds.contains(kind) && Some(kind.as_str()) != root.as_deref());
}

/// Import and carry: keep the imported root account, fill other kinds without
/// refusing, and do not block on a picker.
pub fn fill_imported(
    record: &mut SessionRecord,
    kinds: &BTreeSet<String>,
    catalog: &[ReadyAccount],
) {
    let root = record
        .account
        .as_deref()
        .and_then(|account| account.parse::<AccountRef>().ok())
        .map(|account| account.vertical().as_str().to_owned())
        .unwrap_or_default();
    let Ok(filled) = fill(kinds, &root, &[], catalog, FillMode::BestEffort) else {
        return;
    };
    let mut choices = filled.choices;
    choices.retain(|kind, _| kind != &root);
    record.account_choices = choices;
    if let Some(account) = record.account.clone() {
        record.retain_previous_root_kind(&account);
    }
}

/// Store explicit choices. A choice of the current root kind replaces the line
/// head. Every other kind is the non-root map.
pub fn store_explicit(record: &mut SessionRecord, choices: &[AccountChoice]) {
    for choice in choices {
        let root_kind = record
            .account
            .as_deref()
            .and_then(|account| account.parse::<AccountRef>().ok())
            .map(|account| account.vertical().as_str().to_owned());
        if Some(choice.kind.as_str()) == root_kind.as_deref() {
            record.account = Some(choice.account.clone());
        } else {
            record
                .account_choices
                .insert(choice.kind.clone(), choice.account.clone());
        }
    }
}

/// Parse one `--account <kind>=<ref>` flag.
pub fn parse_kind_choice(value: &str) -> Result<AccountChoice, String> {
    let Some((kind, account)) = value.split_once('=') else {
        return Err(format!(
            "--account {value} must be <kind>=<ref>, for example codex=codex/work"
        ));
    };
    if kind.is_empty() || account.is_empty() || !is_kind(kind) {
        return Err(format!(
            "--account {value} must name a provider kind and an account of that kind"
        ));
    }
    let parsed = account
        .parse::<AccountRef>()
        .map_err(|error: plakous_hosted::capabilities::AccountAdmissionError| error.to_string())?;
    if parsed.vertical().as_str() != kind {
        return Err(format!(
            "--account {value} names an account of a different kind"
        ));
    }
    Ok(AccountChoice {
        kind: kind.to_owned(),
        account: parsed.to_string(),
    })
}

/// Split repeatable `--account` values into one bare root account and kind choices.
pub fn split_account_flags(
    flags: Vec<String>,
) -> Result<(Option<String>, Vec<AccountChoice>), String> {
    let mut bare = None;
    let mut choices = Vec::new();
    for flag in flags {
        if flag.contains('=') {
            choices.push(parse_kind_choice(&flag)?);
        } else if bare.is_some() {
            return Err(
                "repeat --account as <kind>=<ref>; only one bare account selects the root"
                    .to_owned(),
            );
        } else {
            bare = Some(flag);
        }
    }
    Ok((bare, choices))
}

/// The account a spawn freezes for a card that names `kind_or_frozen`.
///
/// A named reference is already frozen and is kept. A kind resolves through
/// the root line head or the stored choice. Nothing Ready is an error.
pub fn resolve_spawn_account(
    kind_or_frozen: &str,
    session: &SessionRecord,
) -> Result<String, String> {
    session.resolve_spawn_account(kind_or_frozen)
}

/// Plan one `ChooseAccount`. `root_busy` refuses a root switch before any write.
pub fn plan_switch(
    kind: &str,
    account: &str,
    root_kind: &str,
    root_busy: bool,
    continue_failed: bool,
    failed_prompt: Option<&str>,
    round_committed_after_prompt: bool,
    operation_id: String,
    command_id: String,
) -> Result<SwitchPlan, RequestRefusal> {
    let account = admit_choice(kind, account, &[]).or_else(|refusal| match refusal {
        RequestRefusal::AccountChoiceInvalid { reason, .. } if reason == "not ready" => {
            // Readiness of the selected account is checked by the caller
            // against the live catalog. This admission checks shape only
            // when the catalog is empty, so a unit plan can name a ref.
            let parsed = account.parse::<AccountRef>().map_err(|_| {
                RequestRefusal::AccountChoiceInvalid {
                    kind: kind.to_owned(),
                    account: account.to_owned(),
                    reason: "not an account reference".to_owned(),
                }
            })?;
            if parsed.vertical().as_str() != kind {
                return Err(RequestRefusal::AccountChoiceInvalid {
                    kind: kind.to_owned(),
                    account: account.to_owned(),
                    reason: "not an account of this kind".to_owned(),
                });
            }
            Ok(parsed.to_string())
        }
        other => Err(other),
    })?;
    if kind != root_kind {
        return Ok(SwitchPlan::MapOnly {
            kind: kind.to_owned(),
            account,
        });
    }
    if root_busy {
        return Ok(SwitchPlan::RootBusy);
    }
    let prompt = continue_failed.then(|| match (round_committed_after_prompt, failed_prompt) {
        (false, Some(prompt)) if !prompt.is_empty() => SwitchPrompt::Exact(prompt.to_owned()),
        _ => SwitchPrompt::Continuation,
    });
    Ok(SwitchPlan::Root {
        account,
        prompt,
        operation_id,
        command_id,
    })
}

/// The prompt bytes a root-start submits, when the plan has one.
pub fn switch_prompt_text(prompt: &SwitchPrompt) -> &str {
    match prompt {
        SwitchPrompt::Exact(text) => text,
        SwitchPrompt::Continuation => CONTINUATION_PROMPT,
    }
}

/// Reopen never resubmits a switch. An unpublished rebind is withdrawn.
/// A published rebind without its command gets a notice. A written command
/// stays the one command of its id.
pub const fn recover_switch(request_published: bool, command_written: bool) -> SwitchRecovery {
    if !request_published {
        SwitchRecovery::WithdrawRequest
    } else if !command_written {
        SwitchRecovery::NoticeNoPrompt
    } else {
        SwitchRecovery::KeepExistingCommand
    }
}

/// A crashed switch is never submitted again by reopen.
pub const fn auto_resubmit_after_reopen() -> bool {
    false
}

/// Stable ids for one continue-on action, derived from the session and account
/// so a repeat of the same action is the same command.
pub fn switch_ids(session_id: &str, kind: &str, account: &str) -> (String, String) {
    let digest =
        crate::store::sessions::digest_parts(&["plak:account-switch", session_id, kind, account]);
    (
        format!("plak:rebind:account:{digest}"),
        format!("plak:root-start:account:{digest}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready(account: &str) -> ReadyAccount {
        ReadyAccount {
            account: account.to_owned(),
            ready: true,
            label: None,
        }
    }

    fn unready(account: &str) -> ReadyAccount {
        ReadyAccount {
            account: account.to_owned(),
            ready: false,
            label: None,
        }
    }

    fn kinds(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn fill_refuses_zero_ready_accounts_and_accepts_one_or_an_explicit_choice() {
        let used = kinds(&["codex", "claude"]);
        let none = fill(&used, "codex", &[], &[], FillMode::Require);
        match none {
            Err(RequestRefusal::AccountChoiceRequired { kinds }) => {
                assert_eq!(
                    kinds
                        .iter()
                        .map(|row| row.kind.as_str())
                        .collect::<Vec<_>>(),
                    ["claude", "codex"]
                );
            }
            other => panic!("expected required, got {other:?}"),
        }
        let one = fill(
            &used,
            "codex",
            &[],
            &[ready("codex/work"), ready("claude")],
            FillMode::Require,
        )
        .unwrap();
        assert_eq!(one.root_account.as_deref(), Some("codex/work"));
        assert_eq!(
            one.choices.get("claude").map(String::as_str),
            Some("claude")
        );
        let many = fill(
            &kinds(&["codex"]),
            "codex",
            &[],
            &[ready("codex"), ready("codex/work")],
            FillMode::Require,
        );
        assert!(matches!(
            many,
            Err(RequestRefusal::AccountChoiceRequired { .. })
        ));
        let explicit = fill(
            &kinds(&["codex"]),
            "codex",
            &[AccountChoice {
                kind: "codex".into(),
                account: "codex/work".into(),
            }],
            &[ready("codex"), ready("codex/work")],
            FillMode::Require,
        )
        .unwrap();
        assert_eq!(explicit.root_account.as_deref(), Some("codex/work"));
        assert!(explicit.choices.is_empty());
    }

    #[test]
    fn account_choice_required_lists_the_unfilled_kind() {
        let error = fill(
            &kinds(&["grok"]),
            "codex",
            &[],
            &[ready("grok/a"), ready("grok/b"), unready("grok")],
            FillMode::Require,
        )
        .unwrap_err();
        let RequestRefusal::AccountChoiceRequired { kinds } = error else {
            panic!("expected required");
        };
        assert_eq!(kinds.len(), 1);
        assert_eq!(kinds[0].kind, "grok");
        assert_eq!(kinds[0].rows.len(), 3);
    }

    #[test]
    fn import_fill_does_not_refuse_when_nothing_is_ready() {
        let filled = fill(
            &kinds(&["codex", "claude"]),
            "codex",
            &[],
            &[unready("claude"), ready("codex/ops")],
            FillMode::BestEffort,
        )
        .unwrap();
        assert_eq!(filled.root_account.as_deref(), Some("codex/ops"));
        assert!(filled.choices.is_empty());
    }

    #[test]
    fn a_named_template_account_is_not_a_kind_flag() {
        let error = parse_kind_choice("codex").unwrap_err();
        assert!(error.contains("<kind>=<ref>"), "{error}");
        let choice = parse_kind_choice("codex=codex/work").unwrap();
        assert_eq!(choice.kind, "codex");
        assert_eq!(choice.account, "codex/work");
    }

    #[test]
    fn spawn_keeps_a_frozen_named_ref_and_resolves_a_kind_through_the_choice() {
        let mut session = SessionRecord::new(std::path::PathBuf::from("/tmp/plak-choice"));
        session.account = Some("codex/work".into());
        session
            .account_choices
            .insert("claude".into(), "claude/alt".into());
        assert_eq!(
            resolve_spawn_account("claude/old", &session).unwrap(),
            "claude/old"
        );
        assert_eq!(
            resolve_spawn_account("claude", &session).unwrap(),
            "claude/alt"
        );
        assert_eq!(
            resolve_spawn_account("codex", &session).unwrap(),
            "codex/work"
        );
        let error = resolve_spawn_account("grok", &session).unwrap_err();
        assert!(error.contains("no ready account"), "{error}");
    }

    #[test]
    fn a_non_root_switch_does_not_rebind_and_a_busy_root_is_root_busy() {
        let map = plan_switch(
            "claude",
            "claude/alt",
            "codex",
            true,
            true,
            Some("do the thing"),
            false,
            "op".into(),
            "cmd".into(),
        )
        .unwrap();
        assert!(matches!(map, SwitchPlan::MapOnly { .. }));
        let busy = plan_switch(
            "codex",
            "codex/work",
            "codex",
            true,
            true,
            Some("do the thing"),
            false,
            "op".into(),
            "cmd".into(),
        )
        .unwrap();
        assert_eq!(busy, SwitchPlan::RootBusy);
    }

    #[test]
    fn a_root_switch_uses_the_exact_prompt_or_the_pinned_continuation() {
        let exact = plan_switch(
            "codex",
            "codex/work",
            "codex",
            false,
            true,
            Some("do the thing"),
            false,
            "op".into(),
            "cmd".into(),
        )
        .unwrap();
        let SwitchPlan::Root {
            prompt, command_id, ..
        } = exact
        else {
            panic!("root plan");
        };
        assert_eq!(command_id, "cmd");
        assert_eq!(switch_prompt_text(prompt.as_ref().unwrap()), "do the thing");
        let continued = plan_switch(
            "codex",
            "codex/work",
            "codex",
            false,
            true,
            Some("do the thing"),
            true,
            "op".into(),
            "cmd".into(),
        )
        .unwrap();
        let SwitchPlan::Root { prompt, .. } = continued else {
            panic!("root plan");
        };
        assert_eq!(
            switch_prompt_text(prompt.as_ref().unwrap()),
            CONTINUATION_PROMPT
        );
    }

    #[test]
    fn the_same_switch_ids_repeat_and_reopen_never_auto_resubmits() {
        let first = switch_ids("session", "codex", "codex/work");
        assert_eq!(first, switch_ids("session", "codex", "codex/work"));
        assert_ne!(first, switch_ids("session", "codex", "codex/other"));
        assert!(!auto_resubmit_after_reopen());
        assert_eq!(
            recover_switch(false, false),
            SwitchRecovery::WithdrawRequest
        );
        assert_eq!(recover_switch(true, false), SwitchRecovery::NoticeNoPrompt);
        assert_eq!(
            recover_switch(true, true),
            SwitchRecovery::KeepExistingCommand
        );
    }
}

#[cfg(test)]
mod durability {
    use super::*;
    use crate::home::paths::PlakPaths;
    use crate::store::ProductStore;
    use plakous_hosted::sessions::projector::{Submission, SubmissionKind, WorkflowRequest};
    use plakous_hosted::sessions::SessionWorkflowKind;

    fn locator() -> plakous::sessions::SessionLocator {
        plakous::sessions::SessionLocator::new(
            uuid::Uuid::nil(),
            uuid::Uuid::from_u128(1),
            uuid::Uuid::from_u128(2),
        )
    }

    #[test]
    fn a_switch_crash_withdraws_an_unpublished_rebind_and_never_writes_a_second_command() {
        let root = tempfile::tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("home"));
        let mut store = ProductStore::open(&paths).unwrap();
        let mut session = SessionRecord::new(root.path().join("workspace"));
        session.account = Some("codex".into());
        session
            .account_choices
            .insert("claude".into(), "claude/alt".into());
        store.commit(&mut session).unwrap();
        let (operation_id, command_id) = switch_ids(&session.id, "codex", "codex/work");
        let plan = plan_switch(
            "codex",
            "codex/work",
            "codex",
            false,
            true,
            Some("do the thing"),
            false,
            operation_id.clone(),
            command_id.clone(),
        )
        .unwrap();
        let SwitchPlan::Root {
            account,
            prompt,
            operation_id,
            command_id,
        } = plan
        else {
            panic!("root plan");
        };
        let request = WorkflowRequest {
            operation_id: operation_id.clone(),
            kind: SessionWorkflowKind::Rebind(Box::new(
                plakous_hosted::sessions::RebindSelection {
                    provider: "codex".into(),
                    account: account.clone(),
                    model: "gpt".into(),
                    effort: None,
                    composition: session.composition,
                    target_selection: session.target_selection.clone(),
                    protocol: "native".into(),
                    route: "native".into(),
                    requested_model: "gpt".into(),
                    selected_configuration: "{}".into(),
                    effective_configuration: "{}".into(),
                    account_binding: "\"binding\"".into(),
                    own_tree: None,
                },
            )),
            source_locator: locator(),
        };
        store.write_workflow_request(&session.id, &request).unwrap();
        drop(store);
        let mut reopened = ProductStore::open(&paths).unwrap();
        assert_eq!(
            recover_switch(false, false),
            SwitchRecovery::WithdrawRequest
        );
        assert!(!auto_resubmit_after_reopen());
        reopened
            .withdraw_unadmitted_workflows(&mut session)
            .unwrap();
        let intents = reopened.load_intents(&session.id).unwrap();
        assert!(intents.workflow_request(&operation_id).is_none());
        assert!(intents.submissions.is_empty());
        let submission = Submission {
            command_id: command_id.clone(),
            kind: SubmissionKind::RootStart,
            queue_item_id: command_id.clone(),
            queue_item_revision: 1,
            target_turn_id: None,
            canonical_command: switch_prompt_text(prompt.as_ref().unwrap()).to_owned(),
            submitted_at_ms: 1,
        };
        reopened.write_submission(&session.id, &submission).unwrap();
        assert_eq!(
            recover_switch(true, true),
            SwitchRecovery::KeepExistingCommand
        );
        let again = reopened.write_submission(&session.id, &submission);
        assert!(again.is_ok());
        let loaded = reopened.load_intents(&session.id).unwrap();
        assert_eq!(loaded.submissions.len(), 1);
        assert_eq!(loaded.submissions[0].command_id, command_id);
        assert_eq!(loaded.submissions[0].canonical_command, "do the thing");
        let persisted = reopened.load(&session.id).unwrap();
        assert_eq!(
            persisted.account_choices.get("claude").map(String::as_str),
            Some("claude/alt")
        );
        assert!(persisted.account_choices.get("codex").is_none());
    }
}
