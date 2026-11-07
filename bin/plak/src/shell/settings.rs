//! Settings snapshot, save, and resume-picker projections.

use crate::home::paths::PlakPaths;
use crate::product_config::ProductConfig;
use crate::surface_protocol::{
    AgentAcknowledgement, ContentEvent, PlakExtension, ProfileOutcome, SurfaceIntent,
    SurfaceObservation,
};
use plakous_host_protocol::status::HostPhase;
use plakous_hosted::binding::ProtocolSelection;

use crate::shell::host::{commit_root_template, settings_save_changes_root_binding};
use plakous_hosted::runtime::{
    ProfileForkOutcome, Sender, SurfaceEffect, SurfaceFacts, SurfaceHandler, SurfacePhase,
    SurfaceProfileFork, SurfaceSaved, UsageReads, oauth_provider_for_account,
};
use plakous_hosted::sessions::{NoticeScope, TranscriptRow};

fn send_profiles(paths: &PlakPaths, events: &Sender<PlakExtension>) {
    let view = match ProductConfig::load(paths) {
        Ok(config) => crate::profiles::list(&config, paths.home()),
        Err(error) => crate::surface_protocol::ProfilesView {
            diagnostics: vec![config_load_diagnostic(&error)],
            ..crate::surface_protocol::ProfilesView::default()
        },
    };
    let _ = events.send(ContentEvent::Extension(SurfaceObservation::Profiles(view)));
}

/// One line when `config.toml` cannot be loaded. Listing profiles against the
/// default product would report each named template as missing.
fn config_load_diagnostic(error: &str) -> String {
    format!("templates could not be loaded because config.toml failed: {error}")
}

fn profile_differences(
    row: &crate::surface_protocol::ProfileRow,
    config: &ProductConfig,
    session: &plakous_hosted::runtime::SurfaceSession,
) -> Vec<crate::surface_protocol::ProfileDifference> {
    use crate::surface_protocol::ProfileDifference as Difference;
    let mut differences = Vec::new();
    let account = session.account.clone().unwrap_or_default();
    if account != row.root.account {
        differences.push(Difference::RootAccount {
            session: account,
            profile: row.root.account.clone(),
        });
    }
    let model = session.model.clone().unwrap_or_default();
    if model != row.root.model {
        differences.push(Difference::RootModel {
            session: model,
            profile: row.root.model.clone(),
        });
    }
    let effort = session.effort.as_deref().unwrap_or("default");
    if effort != row.root.effort {
        differences.push(Difference::RootEffort {
            session: effort.to_owned(),
            profile: row.root.effort.clone(),
        });
    }
    if session.fast != row.root.fast {
        differences.push(Difference::RootFast {
            session: session.fast,
        });
    }
    let roster = crate::roster_input::roster_input_from_config(
        config,
        row.root.account.clone(),
        row.root.model.clone(),
        session.effort.clone(),
    );
    if let Some(frozen) = &session.roster {
        if frozen.working_set != roster.working_set {
            differences.push(Difference::WorkingSet);
        }
        let names = frozen
            .roster
            .keys()
            .chain(roster.templates.keys())
            .collect::<std::collections::BTreeSet<_>>();
        for name in names {
            if frozen.roster.get(name) != roster.templates.get(name) {
                differences.push(Difference::Card(name.to_string()));
            }
        }
        if differences.is_empty() && frozen.templates_digest != roster.templates_digest {
            differences.push(Difference::Roster);
        }
    } else {
        differences.push(Difference::Roster);
    }
    differences
}

fn session_profile(
    config: &ProductConfig,
    home: &std::path::Path,
    session: &plakous_hosted::runtime::SurfaceSession,
) -> crate::surface_protocol::ProfileRow {
    let mut matched_order = None;
    let mut nearest: Option<(String, Vec<crate::surface_protocol::ProfileDifference>)> = None;
    let matched = crate::profiles::list(config, home)
        .rows
        .into_iter()
        .find_map(|row| {
            let Ok(prepared) = crate::profiles::prepare(config, home, &row.id) else {
                return None;
            };
            let differences = profile_differences(&row, &prepared.config, session);
            if differences.is_empty() {
                matched_order = Some(prepared.config.working_set.names().to_vec());
                return Some(row.id);
            }
            if nearest
                .as_ref()
                .is_none_or(|(_, previous)| differences.len() < previous.len())
            {
                nearest = Some((row.id, differences));
            }
            None
        });
    // An exact match names its profile; otherwise the nearest profile names
    // the session, with the values that differ from it.
    let (id, differences) = match matched {
        Some(id) => (id, Vec::new()),
        None => nearest.unwrap_or_else(|| {
            (
                "custom".to_owned(),
                vec![crate::surface_protocol::ProfileDifference::Roster],
            )
        }),
    };
    session_profile_row(config, session, id, differences, matched_order.as_deref())
}

/// Config failed to load: the chip is not a nearest-profile match, and it is
/// not labeled `custom`.
fn unavailable_session_profile(
    session: &plakous_hosted::runtime::SurfaceSession,
) -> crate::surface_protocol::ProfileRow {
    session_profile_row(
        &ProductConfig::default_product(),
        session,
        "unavailable".to_owned(),
        Vec::new(),
        None,
    )
}

fn session_profile_for(
    paths: &PlakPaths,
    session: &plakous_hosted::runtime::SurfaceSession,
) -> crate::surface_protocol::ProfileRow {
    match ProductConfig::load(paths) {
        Ok(config) => session_profile(&config, paths.home(), session),
        Err(_) => unavailable_session_profile(session),
    }
}

fn session_profile_row(
    config: &ProductConfig,
    session: &plakous_hosted::runtime::SurfaceSession,
    id: String,
    differences: Vec<crate::surface_protocol::ProfileDifference>,
    matched_order: Option<&[plakous_hosted::agent_tree_protocol::TemplateName]>,
) -> crate::surface_protocol::ProfileRow {
    use crate::surface_protocol::{ProfileRole, ProfileRow};
    let effort = session.effort.as_deref().unwrap_or("default");
    let cards = session.roster.as_ref().map_or_else(Vec::new, |snapshot| {
        let working_set = ordered_working_set(config, snapshot, matched_order);
        crate::roster_input::nested_spawns(&working_set, |name| {
            snapshot
                .binding(name)
                .map(|card| card.spawns.clone())
                .unwrap_or_default()
        })
        .into_iter()
        .filter_map(|(depth, name)| snapshot.binding(&name).map(|card| (depth, card)))
        .map(|(depth, card)| {
            let fast = match &card.processing_tier {
                plakous::model::ProcessingTierDisposition::ProviderNamed(tier) => {
                    tier.as_str() == "priority"
                }
                plakous::model::ProcessingTierDisposition::Standard => false,
            };
            ProfileRole {
                label: card
                    .template
                    .strip_prefix(&format!("{id}-"))
                    .unwrap_or(&card.template)
                    .to_owned(),
                account: card.account.clone(),
                model: card.tuple_id.clone(),
                effort: card.effort.clone().unwrap_or_else(|| "default".to_owned()),
                fast,
                depth,
            }
        })
        .collect()
    });
    ProfileRow {
        id,
        differences,
        aliases: Vec::new(),
        root: ProfileRole {
            label: "root".to_owned(),
            account: session.account.clone().unwrap_or_default(),
            model: session
                .model
                .clone()
                .unwrap_or_else(|| "not attached".to_owned()),
            effort: effort.to_owned(),
            fast: session.fast,
            depth: 0,
        },
        cards,
    }
}

/// The frozen working set is a set, so it has no config order. Prefer the
/// matched profile's order, then the loaded config's: both are what Profiles
/// already shows.
fn ordered_working_set(
    config: &ProductConfig,
    snapshot: &plakous_hosted::orchestration::OrchestrationSnapshot,
    matched_order: Option<&[plakous_hosted::agent_tree_protocol::TemplateName]>,
) -> Vec<plakous_hosted::agent_tree_protocol::TemplateName> {
    let same_members = |order: &[plakous_hosted::agent_tree_protocol::TemplateName]| {
        order
            .iter()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>()
            == snapshot.working_set
    };
    if let Some(order) = matched_order.filter(|order| same_members(order)) {
        order.to_vec()
    } else if same_members(config.working_set.names()) {
        config.working_set.names().to_vec()
    } else {
        snapshot.working_set.iter().cloned().collect()
    }
}

fn send_session_profile(
    paths: &PlakPaths,
    session: &plakous_hosted::runtime::SurfaceSession,
    events: &Sender<PlakExtension>,
) {
    let _ = events.send(ContentEvent::Extension(SurfaceObservation::SessionProfile(
        session_profile_for(paths, session),
    )));
}

fn publish_composer_chrome(
    generation: &std::sync::Mutex<u64>,
    expected: u64,
    row: crate::surface_protocol::ProfileRow,
    label: crate::surface_protocol::WorkspaceChrome,
    events: &Sender<PlakExtension>,
) {
    let Ok(current) = generation.lock() else {
        return;
    };
    if *current != expected {
        return;
    }
    let _ = events.send(ContentEvent::Extension(SurfaceObservation::SessionProfile(
        row,
    )));
    let _ = events.send(ContentEvent::Extension(
        SurfaceObservation::WorkspaceChrome(label),
    ));
}

fn refuse_profile(name: &str, reason: String, events: &Sender<PlakExtension>) {
    let _ = events.send(ContentEvent::Extension(
        SurfaceObservation::ProfileRefused {
            name: name.to_owned(),
            reason,
        },
    ));
}

/// Reports how a profile landed. Profile choice is per session: only the
/// explicit default action writes the home's pointers, and Use now (offered
/// only while no model is bound) when the home has no loadable default, so a
/// fresh home's next `plak` has one. A fork never writes them.
fn commit_profile(
    paths: &PlakPaths,
    id: String,
    config: &ProductConfig,
    outcome: ProfileOutcome,
    home_has_default: bool,
    events: &Sender<PlakExtension>,
) {
    let saves = match outcome {
        ProfileOutcome::Saved => true,
        ProfileOutcome::Bound => !home_has_default,
        ProfileOutcome::Forked | ProfileOutcome::Switched | ProfileOutcome::AlreadyActive => false,
    };
    if saves
        && let Err(error) = config.save_scoped(paths, crate::product_config::ConfigWriteScope::User)
    {
        let reason = error.into_message();
        if outcome == ProfileOutcome::Saved {
            refuse_profile(&id, reason, events);
            return;
        }
        // The session already moved; only the default for the next launch
        // could not be written.
        let _ = events.send(ContentEvent::Row(TranscriptRow::error(NoticeScope::Settings, format!(
            "profile {id} is active in this session, but it could not be saved as the default: {reason}"
        ))));
    }
    let _ = events.send(ContentEvent::Extension(
        SurfaceObservation::ProfileApplied { name: id, outcome },
    ));
    send_profiles(paths, events);
}

/// Validates a profile and either saves it for the next session or asks the
/// host to use it now. Nothing is saved before a requested fork lands.
fn apply_profile(
    paths: &PlakPaths,
    name: &str,
    apply: crate::surface_protocol::ProfileApply,
    facts: SurfaceFacts<'_>,
    events: &Sender<PlakExtension>,
) -> Option<SurfaceProfileFork> {
    let loaded = match ProductConfig::load(paths) {
        Ok(config) => config,
        Err(error) => {
            refuse_profile(name, error, events);
            return None;
        }
    };
    if loaded.origin(crate::product_config::ConfigField::RootAgent)
        == crate::product_config::ConfigValueOrigin::Project
        || loaded.origin(crate::product_config::ConfigField::WorkingSet)
            == crate::product_config::ConfigValueOrigin::Project
    {
        refuse_profile(
            name,
            "this project's .plak/config.toml owns the root or the working set".to_owned(),
            events,
        );
        return None;
    }
    let home_has_default = loaded.root_template().is_some();
    let prepared = match crate::profiles::prepare(&loaded, paths.home(), name) {
        Ok(prepared) => prepared,
        Err(error) => {
            refuse_profile(name, error, events);
            return None;
        }
    };
    let root = &prepared.root;
    let ready = facts
        .catalog
        .iter()
        .any(|section| section.ready && section.account == root.account);
    if !ready {
        refuse_profile(
            &prepared.id,
            format!("{} is not ready; log in before applying it", root.account),
            events,
        );
        return None;
    }
    if apply == crate::surface_protocol::ProfileApply::Default {
        commit_profile(
            paths,
            prepared.id,
            &prepared.config,
            ProfileOutcome::Saved,
            home_has_default,
            events,
        );
        return None;
    }
    let switch = apply == crate::surface_protocol::ProfileApply::Switch;
    if let Some(reason) = now_refusal(switch, &facts) {
        refuse_profile(&prepared.id, reason, events);
        return None;
    }
    let roster = crate::roster_input::roster_input_from_config(
        &prepared.config,
        root.account.clone(),
        root.model.clone(),
        root.effort.clone(),
    );
    let fork = SurfaceProfileFork {
        account: root.account.clone(),
        model: root.model.clone(),
        effort: root.effort.clone(),
        extra: root.is_fast().then(|| "priority".to_owned()),
        composition: root.composition.map(|composition| {
            plakous_host_protocol::CompositionSelection::from(composition).into()
        }),
        protocol: root.protocol,
        roster,
        switch: switch.then(|| prepared.id.clone()),
        finish: Box::new({
            let paths = paths.clone();
            let events = events.answering();
            move |outcome| {
                let outcome = match outcome {
                    ProfileForkOutcome::Forked => ProfileOutcome::Forked,
                    ProfileForkOutcome::Switched => ProfileOutcome::Switched,
                    ProfileForkOutcome::Bound => ProfileOutcome::Bound,
                    ProfileForkOutcome::AlreadyActive => ProfileOutcome::AlreadyActive,
                    ProfileForkOutcome::Refused(reason) => {
                        refuse_profile_failure(&prepared.id, &reason, &events);
                        return;
                    }
                };
                commit_profile(
                    &paths,
                    prepared.id,
                    &prepared.config,
                    outcome,
                    home_has_default,
                    &events,
                );
            }
        }),
    };
    Some(fork)
}

/// Why the profile cannot apply in this session now: a running turn, or for
/// Switch now, queued prompts (the switch continues this root, so nothing
/// may wait on the predecessor).
fn now_refusal(switch: bool, facts: &SurfaceFacts<'_>) -> Option<String> {
    if facts.phase == SurfacePhase::Busy {
        let verb = if switch { "switch to" } else { "fork onto" };
        return Some(format!(
            "a turn is running; {verb} a profile when the root is idle"
        ));
    }
    (switch && facts.queue_len != 0)
        .then(|| "prompts are queued; switch profile once the queue is empty".to_owned())
}

fn refuse_profile_failure(
    name: &str,
    reason: &plakous_host_protocol::status::HostFailure,
    events: &Sender<PlakExtension>,
) {
    let text = crate::refusal_text::host_failure_text(
        reason,
        crate::notice_text::RemedyContext {
            frontend: crate::notice_text::Frontend::Tui,
            session_id: None,
        },
    );
    refuse_profile(name, text, events);
}

/// Publishes the template readiness projection to the TUI. Emitted
/// at session open and whenever the TUI asks (spec 8.2, 8.7). A load failure
/// publishes an empty roster rather than a stale one: readiness is fail-closed.
pub(super) fn emit_template_readiness(
    paths: &PlakPaths,
    ready_catalog: &[plakous_host_protocol::views::ProviderSection],
    events: &Sender<PlakExtension>,
) {
    let rows = crate::agents_projection::template_rows(paths, ready_catalog);
    let _ = events.send(ContentEvent::TemplateReadiness(rows));
}

pub fn settings_view(config: &ProductConfig) -> crate::surface_protocol::SettingsView {
    crate::surface_protocol::SettingsView {
        product: crate::surface_protocol::SettingsSnapshot {
            default_account: config.default_account().map(str::to_owned),
            default_model: config.default_model().map(str::to_owned),
            default_effort: config.default_effort().map(str::to_owned),
            default_extra: config.default_extra().map(str::to_owned),
            composition: config.default_composition().into(),
            composition_options: [
                plakous_hosted::binding::CompositionSelection::Direct,
                plakous_hosted::binding::CompositionSelection::FollowCatalog,
                plakous_hosted::binding::CompositionSelection::NativePtc,
                plakous_hosted::binding::CompositionSelection::JsPtc,
                plakous_hosted::binding::CompositionSelection::PyPtc,
                plakous_hosted::binding::CompositionSelection::PySdk,
            ]
            .into_iter()
            .filter(|composition| {
                !composition.requires_python_worker()
                    || crate::build_features::require_feature("python").is_ok()
            })
            .map(
                |composition| crate::surface_protocol::SettingsCompositionOption {
                    composition: composition.into(),
                    effective_presentation: plakous_hosted::agent::effective_presentation_label(
                        composition,
                        config
                            .default_account()
                            .and_then(oauth_provider_for_account),
                        None,
                    )
                    .to_owned(),
                    unavailable_reason: composition_conflict(
                        composition,
                        config.default_account(),
                        config.default_model(),
                        config.default_effort(),
                    ),
                },
            )
            .collect::<Vec<_>>()
            .into_boxed_slice(),
            protocol: match config.default_protocol() {
                ProtocolSelection::Auto => plakous_host_protocol::ProtocolSelection::Auto,
                ProtocolSelection::Chat => plakous_host_protocol::ProtocolSelection::Chat,
                ProtocolSelection::Responses => plakous_host_protocol::ProtocolSelection::Responses,
                ProtocolSelection::Messages => plakous_host_protocol::ProtocolSelection::Messages,
            },
            compaction: config.compaction_enabled,
        },
        tui: crate::tui_preferences::TuiPreferences {
            mouse: config.tui_mouse,
            scroll_lines: config.scroll_lines,
            show_thinking: config.show_thinking,
            group_tool_calls: config.group_tool_calls,
            reduced_motion: config.reduced_motion,
            theme: match config.theme {
                crate::product_config::ProductTheme::DefaultDark => {
                    crate::surface_protocol::SettingsTheme::DefaultDark
                }
                crate::product_config::ProductTheme::DefaultLight => {
                    crate::surface_protocol::SettingsTheme::DefaultLight
                }
                crate::product_config::ProductTheme::CatppuccinLatte => {
                    crate::surface_protocol::SettingsTheme::CatppuccinLatte
                }
                crate::product_config::ProductTheme::CatppuccinFrappe => {
                    crate::surface_protocol::SettingsTheme::CatppuccinFrappe
                }
                crate::product_config::ProductTheme::CatppuccinMacchiato => {
                    crate::surface_protocol::SettingsTheme::CatppuccinMacchiato
                }
                crate::product_config::ProductTheme::CatppuccinMocha => {
                    crate::surface_protocol::SettingsTheme::CatppuccinMocha
                }
            },
        },
    }
}

pub(super) fn composition_conflict(
    composition: plakous_hosted::binding::CompositionSelection,
    account: Option<&str>,
    model: Option<&str>,
    _effort: Option<&str>,
) -> Option<String> {
    // Settings persistence has no authenticated catalog authority. Exact
    // conflicts are projected by the Ready catalog and rechecked by headless
    // admission; never infer CodeModeOnly from account/model spelling here.
    if composition != plakous_hosted::binding::CompositionSelection::FollowCatalog
        && (account.is_none_or(str::is_empty) || model.is_none_or(str::is_empty))
    {
        return Some("requires an exact default account/model tuple".to_owned());
    }
    if composition == plakous_hosted::binding::CompositionSelection::NativePtc {
        return native_ptc_conflict(account?, model?).map(str::to_owned);
    }
    None
}

/// `native-ptc` is the vertical's own PTC system, so the account's own
/// enrollment decides it: a subscription vertical with no enrolled
/// `CodeModeOnly` row, an API model the bundled table does not enroll as
/// `CodeModeOnly`, and OpenRouter refuse it. Which Codex row is Code Mode is
/// the Ready catalog's to say, not this check's.
fn native_ptc_conflict(account: &str, model: &str) -> Option<&'static str> {
    use plakous::subscriptions::NativeToolMode;
    use plakous_hosted::binding::{CompositionSelection, PlatformProvider};
    if let Some(provider) = oauth_provider_for_account(account) {
        let enrolled = provider
            .enrolled_models()
            .into_iter()
            .any(|row| provider.enrolled_tool_mode(row) == Some(NativeToolMode::CodeModeOnly));
        return (!enrolled).then(|| plakous_hosted::binding::native_ptc_refusal(provider));
    }
    if let Some(provider) = PlatformProvider::for_account(account) {
        return plakous_hosted::platform::api_composition(
            provider,
            model,
            CompositionSelection::NativePtc,
        )
        .err();
    }
    (account == "openrouter").then_some(plakous_hosted::binding::OPENROUTER_NATIVE_PTC_REFUSAL)
}

pub(super) fn emit_settings(paths: &PlakPaths, events: &Sender<PlakExtension>) {
    match ProductConfig::load(paths) {
        Ok(config) => {
            let _ = events.send(ContentEvent::Extension(SurfaceObservation::SettingsView(
                settings_view(&config),
            )));
        }
        Err(error) => {
            let message = format!("settings could not be loaded: {error}");
            let _ = events.send(ContentEvent::Extension(
                SurfaceObservation::SettingsLoadFailed(message.clone()),
            ));
            let _ = events.send(ContentEvent::Row(TranscriptRow::error(
                NoticeScope::Settings,
                message,
            )));
        }
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "Settings save validates catalog authority, writes configuration, and stages a binding handoff in the required order."
)]
pub(super) fn save_settings(
    paths: &PlakPaths,
    update: crate::surface_protocol::SettingsUpdate,
    ready_catalog: &[plakous_host_protocol::views::ProviderSection],
    events: &Sender<PlakExtension>,
) -> Option<ProductConfig> {
    if update.product.default_account.is_some() != update.product.default_model.is_some() {
        let message = "settings: default account and model must be selected together".to_owned();
        let _ = events.send(ContentEvent::Extension(
            SurfaceObservation::SettingsSaveFailed(message.clone()),
        ));
        let _ = events.send(ContentEvent::Row(TranscriptRow::error(
            NoticeScope::Settings,
            message,
        )));
        return None;
    }
    let current = match ProductConfig::load(paths) {
        Ok(current) => current,
        Err(error) => {
            let message = format!("settings: config load failed: {error}");
            let _ = events.send(ContentEvent::Extension(
                SurfaceObservation::SettingsSaveFailed(message.clone()),
            ));
            let _ = events.send(ContentEvent::Row(TranscriptRow::error(
                NoticeScope::Settings,
                message,
            )));
            return None;
        }
    };
    let selected_entry = update
        .product
        .default_account
        .as_ref()
        .zip(update.product.default_model.as_ref())
        .and_then(|(account, model)| {
            ready_catalog
                .iter()
                .find(|section| section.ready && section.account == *account)
                .and_then(|section| section.entries.iter().find(|entry| entry.id == *model))
        });
    let selected_composition: plakous_hosted::binding::CompositionSelection =
        update.product.composition.into();
    let exact_selection = match selected_entry {
        None => {
            update.product.default_account.is_none()
                && update.product.default_model.is_none()
                && update.product.default_effort.is_none()
                && update.product.default_extra.is_none()
        }
        Some(entry) => {
            let effort = if entry.effort_options.is_empty() {
                update.product.default_effort.is_none()
            } else {
                update
                    .product
                    .default_effort
                    .as_ref()
                    .is_some_and(|value| entry.effort_options.contains(value))
            };
            let extra =
                update.product.default_extra.as_ref().is_none_or(|value| {
                    entry.extras.iter().any(|candidate| candidate.id == *value)
                });
            entry.unavailable_reason.is_none() && effort && extra
        }
    };
    // The Settings OpenRouter-protocol row is identity-bearing: it writes the
    // file for new sessions and never repaints a live binding (spec 8.1).
    let default_protocol = match update.product.protocol {
        plakous_host_protocol::ProtocolSelection::Auto => ProtocolSelection::Auto,
        plakous_host_protocol::ProtocolSelection::Chat => ProtocolSelection::Chat,
        plakous_host_protocol::ProtocolSelection::Responses => ProtocolSelection::Responses,
        plakous_host_protocol::ProtocolSelection::Messages => ProtocolSelection::Messages,
    };
    // Wire settings edit the root template body and compaction only.
    // Roster and orchestration policy move through the successor-epoch apply.
    let mut config = current.clone();
    // Settings edits the tuple only; the template's authority fields
    // (description, tiers, delegation, hooks) stay as the file authored them.
    let requested_root = crate::product_config::AgentTemplate {
        account: update.product.default_account.clone().unwrap_or_default(),
        model: update.product.default_model.clone().unwrap_or_default(),
        effort: update.product.default_effort.clone(),
        extra: update.product.default_extra.clone(),
        composition: Some(selected_composition),
        protocol: (update.product.default_account.as_deref() == Some("openrouter"))
            .then_some(default_protocol),
        ..current.root_template().cloned().unwrap_or_default()
    };
    // The host validates exactly what the save changes (readiness
    // is an affordance, not a gate on unrelated writes). The TUI never
    // validates, and `settings_snapshot` always echoes the file's root tuple
    // back into the draft, so a theme, mouse, or reading-width save carries the
    // root binding unchanged. Admitting it against the exact Ready catalog then
    // refused every view-only save under a root whose account is not logged in
    // — the seeded config's own state. Only an actual identity difference is
    // validated, and only then does it reach the template writer.
    let changes_root_binding = settings_save_changes_root_binding(&current, &requested_root);
    if changes_root_binding {
        if !exact_selection {
            let message =
                "settings: defaults are not admitted by the exact Ready catalog".to_owned();
            let _ = events.send(ContentEvent::Extension(
                SurfaceObservation::SettingsSaveFailed(message.clone()),
            ));
            let _ = events.send(ContentEvent::Row(TranscriptRow::error(
                NoticeScope::Settings,
                message,
            )));
            return None;
        }
        if let Some(conflict) = composition_conflict(
            selected_composition,
            update.product.default_account.as_deref(),
            update.product.default_model.as_deref(),
            update.product.default_effort.as_deref(),
        ) {
            let message = format!("settings: composition conflict: {conflict}");
            let _ = events.send(ContentEvent::Extension(
                SurfaceObservation::SettingsSaveFailed(message.clone()),
            ));
            let _ = events.send(ContentEvent::Row(TranscriptRow::error(
                NoticeScope::Settings,
                message,
            )));
            return None;
        }
        // Commit the complete requested binding to one fresh template, then
        // publish its root pointer together with the other settings below.
        if let Err(message) = commit_root_template(&mut config, requested_root) {
            let _ = events.send(ContentEvent::Extension(
                SurfaceObservation::SettingsSaveFailed(message),
            ));
            return None;
        }
    }
    config.compaction_enabled = update.product.compaction;
    // Inside a trusted project, a settings save publishes the project
    // override — the active context owns the defaults. Outside one it writes
    // the global home. Neither path silently flattens the other's values.
    let scope = if config.project.as_ref().is_some_and(|source| source.trusted) {
        crate::product_config::ConfigWriteScope::Project
    } else {
        crate::product_config::ConfigWriteScope::User
    };
    match config
        .save_scoped(paths, scope)
        .map_err(crate::product_config::ConfigSaveError::into_message)
    {
        Ok(()) => {
            let note = settings_saved_note(&current, &config, changes_root_binding);
            let _ = events.send(ContentEvent::Extension(
                SurfaceObservation::SettingsViewApplied(settings_view(&config)),
            ));
            let _ = events.send(ContentEvent::Row(TranscriptRow::info(
                NoticeScope::Settings,
                note,
            )));
            Some(config)
        }
        Err(error) => {
            let message = format!("settings could not be saved: {error}");
            let _ = events.send(ContentEvent::Extension(
                SurfaceObservation::SettingsSaveFailed(message.clone()),
            ));
            let _ = events.send(ContentEvent::Row(TranscriptRow::error(
                NoticeScope::Settings,
                message,
            )));
            None
        }
    }
}

/// The transcript line one settings save prints, naming what actually changed.
///
/// The fixed text said `theme applied` after every toggle, so flipping
/// `Show timestamps` reported a theme change the operator had not made — and
/// had in fact just reverted. Each row names itself, a save that
/// changed nothing says so, and only a theme change mentions the theme.
pub(super) fn settings_saved_note(
    before: &ProductConfig,
    after: &ProductConfig,
    changed_root_binding: bool,
) -> String {
    let mut changed: Vec<&str> = Vec::new();
    if before.theme != after.theme {
        changed.push("theme applied");
    }
    if before.tui_mouse != after.tui_mouse {
        changed.push("mouse capture");
    }
    if before.compaction_enabled != after.compaction_enabled {
        changed.push("auto compaction");
    }
    if before.scroll_lines != after.scroll_lines {
        changed.push("scroll lines");
    }
    if before.show_thinking != after.show_thinking {
        changed.push("show thinking blocks");
    }
    if before.group_tool_calls != after.group_tool_calls {
        changed.push("group tool calls");
    }
    if before.reduced_motion != after.reduced_motion {
        changed.push("reduced motion");
    }
    if changed_root_binding {
        changed.push("defaults (next launch)");
    }
    if changed.is_empty() {
        return "settings saved \u{b7} nothing changed".to_owned();
    }
    format!("settings saved \u{b7} {}", changed.join(", "))
}

/// Longest id an acknowledgement accepts; tree, agent and attempt ids are
/// far shorter, so anything longer is not one.
const MAX_ACK_ID_BYTES: usize = 256;

/// Longest attention label an acknowledgement accepts. The labels the list
/// paints are far shorter.
const MAX_ATTENTION_BYTES: usize = 64;

/// Persists one acknowledgement, then answers with the tree's complete set so
/// the TUI never paints an acknowledgement the store did not keep.
fn acknowledge_agent(
    paths: &PlakPaths,
    tree_id: &str,
    acknowledgement: &AgentAcknowledgement,
    events: &Sender<PlakExtension>,
) {
    if [
        tree_id,
        acknowledgement.agent_id.as_str(),
        acknowledgement.attempt_id.as_str(),
    ]
    .iter()
    .any(|id| id.is_empty() || id.len() > MAX_ACK_ID_BYTES)
        || acknowledgement.attention.is_empty()
        || acknowledgement.attention.len() > MAX_ATTENTION_BYTES
    {
        return;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let recorded = crate::store::ProductStore::open(paths)
        .map_err(|error| error.to_string())
        .and_then(|mut store| {
            store
                .acknowledge_agent_attempt(tree_id, acknowledgement, now)
                .map_err(String::from)
        });
    if let Err(error) = recorded {
        let _ = events.send(ContentEvent::Row(TranscriptRow::warning(
            NoticeScope::Tree,
            format!("agent acknowledgement was not saved: {error}"),
        )));
        return;
    }
    send_agent_acknowledgements(paths, tree_id, events);
}

fn send_agent_acknowledgements(paths: &PlakPaths, tree_id: &str, events: &Sender<PlakExtension>) {
    if tree_id.is_empty() || tree_id.len() > MAX_ACK_ID_BYTES {
        return;
    }
    let attempts = crate::store::ProductStore::open(paths)
        .map_err(|error| error.to_string())
        .and_then(|mut store| store.agent_acknowledgements(tree_id));
    match attempts {
        Ok(attempts) => {
            let _ = events.send(ContentEvent::Extension(
                SurfaceObservation::AgentAcknowledgements {
                    tree_id: tree_id.to_owned(),
                    attempts,
                },
            ));
        }
        Err(error) => {
            let _ = events.send(ContentEvent::Row(TranscriptRow::warning(
                NoticeScope::Tree,
                format!("agent acknowledgements are unavailable: {error}"),
            )));
            let _ = events.send(ContentEvent::Extension(
                SurfaceObservation::AgentAcknowledgementsUnavailable {
                    tree_id: tree_id.to_owned(),
                },
            ));
        }
    }
}

/// Plak's own answers to the surface questions (`runtime::surface`).
///
/// Everything above is what this is made of: the settings schema, the roster
/// projection, and where `config.toml` lives. It is constructed from the
/// product home, and it is the only thing in a hosted session that knows the
/// product has a configuration file at all.
pub struct ProductSurface {
    paths: PlakPaths,
    chrome_refresh: crate::shell::chrome_refresh::ChromeRefresh,
    chrome_generation: std::sync::Arc<std::sync::Mutex<u64>>,
    usage_accounts_generation: std::sync::Arc<std::sync::Mutex<u64>>,
    usage_refreshing: std::sync::Arc<std::sync::Mutex<bool>>,
    /// True only while the TUI's Usage tab is the open control tab.
    usage_watch: std::sync::atomic::AtomicBool,
    state_home: Result<std::path::PathBuf, String>,
    reload: crate::shell::reload_surface::ReloadChannel,
}

/// How long a Usage refresh-account read waits for the host's usage read to
/// settle before it reads posture anyway. The read itself is bounded far
/// below this; the limit only keeps a wedged read from parking the thread.
const USAGE_READ_SETTLE_LIMIT: std::time::Duration = std::time::Duration::from_secs(30);

/// Publishes a refresh-account list unless a newer Usage intent asked for one.
fn publish_usage_refresh_accounts(
    generation: &std::sync::Mutex<u64>,
    expected: u64,
    accounts: Vec<String>,
    events: &Sender<PlakExtension>,
) {
    let Ok(current) = generation.lock() else {
        return;
    };
    if *current == expected {
        let _ = events.send(ContentEvent::Extension(
            SurfaceObservation::UsageRefreshAccounts(accounts),
        ));
    }
}

impl ProductSurface {
    /// The surfaces of one product home.
    #[must_use]
    pub fn new(paths: PlakPaths) -> Self {
        Self {
            state_home: crate::home::paths::application_state_home(),
            usage_refreshing: std::sync::Arc::new(std::sync::Mutex::new(false)),
            chrome_refresh: crate::shell::chrome_refresh::ChromeRefresh::default(),
            chrome_generation: std::sync::Arc::new(std::sync::Mutex::new(0)),
            usage_accounts_generation: std::sync::Arc::new(std::sync::Mutex::new(0)),
            usage_watch: std::sync::atomic::AtomicBool::new(false),
            reload: crate::shell::reload_surface::ReloadChannel::new(paths.clone()),
            paths,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_test_state_home(mut self, state_home: std::path::PathBuf) -> Self {
        self.state_home = Ok(state_home);
        self
    }

    /// Reads the offered `[refresh]` accounts on a thread of its own: the
    /// posture read takes each provider's compatibility lock and waits behind
    /// any holder, so it never runs on the host's action loop. It first waits
    /// for the host's usage read, which holds that lock across its network
    /// exchange and only try-locks it.
    fn send_usage_refresh_accounts(&self, reads: UsageReads, events: &Sender<PlakExtension>) {
        let expected = {
            let Ok(mut generation) = self.usage_accounts_generation.lock() else {
                return;
            };
            *generation = generation.wrapping_add(1);
            *generation
        };
        let state_home = self.state_home.clone();
        let generation = std::sync::Arc::clone(&self.usage_accounts_generation);
        let events = events.answering();
        std::thread::spawn(move || {
            let _ = reads.wait_settled(USAGE_READ_SETTLE_LIMIT);
            let accounts = state_home
                .map_err(plakous_hosted::error::CliFailure::new)
                .and_then(crate::commands::application::PlakousApplication::for_accounts)
                .map(|mut backend| crate::commands::usage::usage_refresh_accounts(&mut backend))
                .unwrap_or_default();
            publish_usage_refresh_accounts(&generation, expected, accounts, &events);
        });
    }

    fn refresh_usage(&self, account: String, events: &Sender<PlakExtension>) {
        let Some(provider) = oauth_provider_for_account(&account) else {
            let _ = events.send(ContentEvent::Extension(
                SurfaceObservation::UsageCredentialRefreshed {
                    account,
                    error: Some("Usage refresh requires a subscription account".to_owned()),
                },
            ));
            return;
        };
        let Ok(mut running) = self.usage_refreshing.lock() else {
            return;
        };
        if *running {
            return;
        }
        *running = true;
        drop(running);
        let paths = self.paths.clone();
        let state_home = self.state_home.clone();
        let running = std::sync::Arc::clone(&self.usage_refreshing);
        let events = events.answering();
        std::thread::spawn(move || {
            let _telemetry = crate::telemetry_sink::install_product_credential_telemetry(&paths);
            let result = state_home
                .map_err(plakous_hosted::error::CliFailure::new)
                .and_then(crate::commands::application::PlakousApplication::for_accounts)
                .and_then(|mut backend| {
                    crate::commands::usage::refresh_usage_credential(&mut backend, provider)
                });
            let error = result.err().and_then(|error| {
                crate::product_doctor::last_failed_refresh(
                    &paths,
                    &account,
                    crate::local_time_zone(),
                )
                .is_none()
                .then(|| error.message().to_owned())
            });
            if let Ok(mut running) = running.lock() {
                *running = false;
            }
            let _ = events.send(ContentEvent::Extension(
                SurfaceObservation::UsageCredentialRefreshed { account, error },
            ));
        });
    }

    /// A boxed handler for a [`HostConfig`](plakous_hosted::runtime::HostConfig)'s `surface` field.
    #[must_use]
    pub fn boxed(paths: &PlakPaths) -> Box<dyn SurfaceHandler<PlakExtension>> {
        Box::new(Self::new(paths.clone()))
    }
}

impl SurfaceHandler<PlakExtension> for ProductSurface {
    fn leaving_session(&self, session_id: &str) {
        self.reload.leave(session_id);
    }

    fn child_wake_enabled(&self) -> Option<bool> {
        let text = match std::fs::read_to_string(self.paths.config_file()) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Some(ProductConfig::default_product().auto_continue);
            }
            Err(_) => return None,
        };
        let config = text.parse::<toml::Table>().ok()?;
        match config
            .get("turn")
            .and_then(|table| table.get("auto_continue"))
        {
            Some(value) => value.as_bool(),
            None => Some(ProductConfig::default_product().auto_continue),
        }
    }

    fn needs_usage(&self, intent: &SurfaceIntent) -> bool {
        matches!(
            intent,
            SurfaceIntent::OpenUsage | SurfaceIntent::RereadUsage
        )
    }

    fn fresh_usage(&self, intent: &SurfaceIntent) -> bool {
        matches!(intent, SurfaceIntent::RereadUsage)
    }

    fn usage_watch_armed(&self) -> bool {
        self.usage_watch.load(std::sync::atomic::Ordering::Acquire)
    }

    fn needs_session_tools(&self, intent: &SurfaceIntent) -> bool {
        matches!(intent, SurfaceIntent::ShowMcp)
    }

    #[allow(
        clippy::too_many_lines,
        reason = "The product intent dispatcher handles each settings, session, topology, and account action against one immutable host snapshot."
    )]
    fn dispatch(
        &self,
        intent: SurfaceIntent,
        facts: SurfaceFacts<'_>,
        events: &Sender<PlakExtension>,
    ) -> SurfaceEffect {
        match intent {
            SurfaceIntent::OpenConfigFile if facts.phase == SurfacePhase::Busy => {
                plakous_hosted::runtime::note_idle_only(events, "config");
            }
            SurfaceIntent::OpenConfigFile => {
                let path = self.paths.config_file().display().to_string();
                let _ = events.send(ContentEvent::Row(TranscriptRow::info(
                    NoticeScope::Settings,
                    format!("config: {path}"),
                )));
                let _ = events.send(ContentEvent::Status {
                    phase: HostPhase::Idle,
                    queue_len: facts.queue_len,
                    detail: Some(
                        plakous_host_protocol::status::HostStatusDetail::ConfigurationPath {
                            path: path.clone(),
                        },
                    ),
                });
            }
            SurfaceIntent::OpenSettings => emit_settings(&self.paths, events),
            SurfaceIntent::ShowMcp => {
                let body =
                    crate::commands::mcp::session_overview(&self.paths, facts.session.as_ref());
                let _ = events.send(ContentEvent::Extension(SurfaceObservation::McpOverview(
                    body,
                )));
            }
            SurfaceIntent::SaveSettings(update) => {
                return SurfaceEffect {
                    saved: save_settings(&self.paths, update, facts.catalog, events).map(|saved| {
                        SurfaceSaved {
                            root_template: saved.root_template_fact(),
                        }
                    }),
                    ..SurfaceEffect::default()
                };
            }
            SurfaceIntent::OpenUsage
                if facts.phase == SurfacePhase::Idle && facts.attached_one_shot => {}
            SurfaceIntent::RefreshUsageCredential { account } => {
                self.refresh_usage(account, events);
            }
            SurfaceIntent::RereadUsage => {
                self.send_usage_refresh_accounts(facts.usage_reads, events);
            }
            SurfaceIntent::WatchUsage(armed) => {
                self.usage_watch
                    .store(armed, std::sync::atomic::Ordering::Release);
            }
            SurfaceIntent::OpenUsage => {
                self.send_usage_refresh_accounts(facts.usage_reads, events);
                emit_settings(&self.paths, events);
                let _ = events.send(ContentEvent::Extension(
                    SurfaceObservation::OpenUsageOverlay,
                ));
            }
            SurfaceIntent::ListProfiles => {
                send_profiles(&self.paths, events);
                if let Some(session) = &facts.session {
                    send_session_profile(&self.paths, session, events);
                }
            }
            SurfaceIntent::RefreshComposerChrome => {
                let expected = {
                    let Ok(mut generation) = self.chrome_generation.lock() else {
                        return SurfaceEffect::default();
                    };
                    *generation = generation.wrapping_add(1);
                    *generation
                };
                if let Some(session) = facts.session {
                    let paths = self.paths.clone();
                    let generation = std::sync::Arc::clone(&self.chrome_generation);
                    let events = events.clone();
                    self.chrome_refresh.request(move || {
                        let row = session_profile_for(&paths, &session);
                        let label = crate::shell::workspace::read(&session.workspace);
                        publish_composer_chrome(&generation, expected, row, label, &events);
                    });
                }
            }
            SurfaceIntent::AcknowledgeAgent {
                tree_id,
                agent_id,
                attempt_id,
                attention,
                ended_at,
            } => acknowledge_agent(
                &self.paths,
                &tree_id,
                &AgentAcknowledgement {
                    agent_id,
                    attempt_id,
                    attention,
                    ended_at,
                },
                events,
            ),
            SurfaceIntent::ListAgentAcknowledgements { tree_id } => {
                send_agent_acknowledgements(&self.paths, &tree_id, events);
            }
            SurfaceIntent::CheckBuild => {
                let session = facts.session.as_ref().map(|session| session.id.as_str());
                self.reload.check(session, events);
            }
            SurfaceIntent::ProbeReload => {
                crate::shell::reload_surface::probe(
                    &self.paths,
                    facts.session,
                    facts.phase,
                    events,
                );
            }
            SurfaceIntent::AcceptReloadRequest { request_id } => {
                self.reload.accept(&request_id);
            }
            SurfaceIntent::RefuseReloadRequest { request_id, reason } => {
                self.reload.refuse(&request_id, reason);
            }
            SurfaceIntent::ApplyProfile { name, apply } => {
                return SurfaceEffect {
                    profile_fork: apply_profile(&self.paths, &name, apply, facts, events),
                    ..SurfaceEffect::default()
                };
            }
        }
        SurfaceEffect::default()
    }

    fn publish_template_readiness(
        &self,
        catalog: &[plakous_host_protocol::views::ProviderSection],
        events: &Sender<PlakExtension>,
    ) {
        emit_template_readiness(&self.paths, catalog, events);
    }

    fn readiness_read(&self) -> plakous_hosted::runtime::ReadinessRead {
        let paths = self.paths.clone();
        Box::new(move || {
            crate::presentation::tui_presentation_values(&paths)
                .map_err(|error| error.message().to_owned())
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
