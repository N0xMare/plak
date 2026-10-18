//! Settings snapshot, save, and resume-picker projections.

use crate::home::paths::PlakPaths;
use crate::product_config::ProductConfig;
use crate::surface_protocol::{
    AgentAcknowledgement, HostEvent, PlakExtension, ProfileOutcome, SurfaceIntent,
    SurfaceObservation,
};
use plakous_hosted::binding::ProtocolSelection;

use crate::shell::host::{commit_root_template, settings_save_changes_root_binding};
use plakous_hosted::runtime::{
    ProfileForkOutcome, Sender, SurfaceEffect, SurfaceFacts, SurfaceHandler, SurfacePhase,
    SurfaceProfileFork, SurfaceSaved, oauth_provider_for_account,
};
use plakous_hosted::sessions::{NoticeScope, TranscriptRow};

fn send_profiles(paths: &PlakPaths, events: &Sender<PlakExtension>) {
    let view = match ProductConfig::load(paths) {
        Ok(config) => crate::profiles::list(&config, paths.home()),
        Err(error) => {
            let mut view = crate::profiles::list(&ProductConfig::default_product(), paths.home());
            view.diagnostics
                .insert(0, format!("config.toml could not be loaded: {error}"));
            view
        }
    };
    let _ = events.send(HostEvent::Extension(SurfaceObservation::Profiles(view)));
}

fn session_profile(
    config: &ProductConfig,
    home: &std::path::Path,
    session: &plakous_hosted::runtime::SurfaceSession,
) -> crate::surface_protocol::ProfileRow {
    use crate::surface_protocol::{ProfileRole, ProfileRow};
    let effort = session.effort.as_deref().unwrap_or("default");
    let mut matched_order = None;
    let id = crate::profiles::list(config, home)
        .rows
        .into_iter()
        .find(|row| {
            if session.account.as_deref() != Some(row.root.account.as_str())
                || session.model.as_deref() != Some(row.root.model.as_str())
                || effort != row.root.effort
                || session.fast != row.root.fast
            {
                return false;
            }
            let Ok(prepared) = crate::profiles::prepare(config, home, &row.id) else {
                return false;
            };
            let roster = crate::roster_input::roster_input_from_config(
                &prepared.config,
                row.root.account.clone(),
                row.root.model.clone(),
                session.effort.clone(),
            );
            let matches = session.roster.as_ref().is_some_and(|frozen| {
                frozen.templates_digest == roster.templates_digest
                    && frozen.working_set == roster.working_set
                    && frozen.roster == roster.templates
            });
            if matches {
                matched_order = Some(prepared.config.working_set.names().to_vec());
            }
            matches
        })
        .map_or_else(|| "custom".to_owned(), |row| row.id);
    let cards = session.roster.as_ref().map_or_else(Vec::new, |snapshot| {
        let working_set = ordered_working_set(config, snapshot, matched_order.as_deref());
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
    let config = ProductConfig::load(paths).unwrap_or_else(|_| ProductConfig::default_product());
    let row = session_profile(&config, paths.home(), session);
    let _ = events.send(HostEvent::Extension(SurfaceObservation::SessionProfile(
        row,
    )));
}

fn publish_composer_chrome(
    generation: &std::sync::Mutex<u64>,
    expected: u64,
    row: crate::surface_protocol::ProfileRow,
    label: String,
    events: &Sender<PlakExtension>,
) {
    let Ok(current) = generation.lock() else {
        return;
    };
    if *current != expected {
        return;
    }
    let _ = events.send(HostEvent::Extension(SurfaceObservation::SessionProfile(
        row,
    )));
    let _ = events.send(HostEvent::Extension(SurfaceObservation::WorkspaceChrome(
        label,
    )));
}

fn refuse_profile(name: &str, reason: String, events: &Sender<PlakExtension>) {
    let _ = events.send(HostEvent::Extension(SurfaceObservation::ProfileRefused {
        name: name.to_owned(),
        reason,
    }));
}

/// Saves the profile's pointers once it has landed, and reports it.
fn commit_profile(
    paths: &PlakPaths,
    id: String,
    config: &ProductConfig,
    outcome: ProfileOutcome,
    events: &Sender<PlakExtension>,
) {
    if let Err(error) = config.save_scoped(paths, crate::product_config::ConfigWriteScope::User) {
        let reason = error.into_message();
        if outcome == ProfileOutcome::Saved {
            refuse_profile(&id, reason, events);
            return;
        }
        // The session already moved; only the default for the next launch
        // could not be written.
        let _ = events.send(HostEvent::Row(TranscriptRow::error(NoticeScope::Settings, format!(
            "profile {id} is active in this session, but it could not be saved as the default: {reason}"
        ))));
    }
    let _ = events.send(HostEvent::Extension(SurfaceObservation::ProfileApplied {
        name: id,
        outcome,
    }));
    send_profiles(paths, events);
}

/// Validates a profile and either saves it for the next session or asks the
/// host to use it now. Nothing is saved before a requested fork lands.
fn apply_profile(
    paths: &PlakPaths,
    name: &str,
    fork_now: bool,
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
    if !fork_now {
        commit_profile(
            paths,
            prepared.id,
            &prepared.config,
            ProfileOutcome::Saved,
            events,
        );
        return None;
    }
    if facts.phase == SurfacePhase::Busy {
        refuse_profile(
            &prepared.id,
            "a turn is running; fork onto a profile when the root is idle".to_owned(),
            events,
        );
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
        finish: Box::new({
            let paths = paths.clone();
            let events = events.clone();
            move |outcome| {
                let outcome = match outcome {
                    ProfileForkOutcome::Forked => ProfileOutcome::Forked,
                    ProfileForkOutcome::Bound => ProfileOutcome::Bound,
                    ProfileForkOutcome::AlreadyActive => ProfileOutcome::AlreadyActive,
                    ProfileForkOutcome::Refused(reason) => {
                        refuse_profile(&prepared.id, reason, &events);
                        return;
                    }
                };
                commit_profile(&paths, prepared.id, &prepared.config, outcome, &events);
            }
        }),
    };
    Some(fork)
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
    let _ = events.send(HostEvent::TemplateReadiness(rows));
}

pub fn settings_snapshot(config: &ProductConfig) -> crate::surface_protocol::SettingsSnapshot {
    crate::surface_protocol::SettingsSnapshot {
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
        ]
        .into_iter()
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
        mouse: config.tui_mouse,
        compaction: config.compaction_enabled,
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
            let _ = events.send(HostEvent::Extension(SurfaceObservation::SettingsSnapshot(
                settings_snapshot(&config),
            )));
        }
        Err(error) => {
            let message = format!("settings could not be loaded: {error}");
            let _ = events.send(HostEvent::Extension(
                SurfaceObservation::SettingsLoadFailed(message.clone()),
            ));
            let _ = events.send(HostEvent::Row(TranscriptRow::error(
                NoticeScope::Settings,
                message,
            )));
        }
    }
}

pub(super) fn save_settings(
    paths: &PlakPaths,
    update: crate::surface_protocol::SettingsUpdate,
    ready_catalog: &[plakous_host_protocol::views::ProviderSection],
    events: &Sender<PlakExtension>,
) -> Option<ProductConfig> {
    if update.default_account.is_some() != update.default_model.is_some() {
        let message = "settings: default account and model must be selected together".to_owned();
        let _ = events.send(HostEvent::Extension(
            SurfaceObservation::SettingsSaveFailed(message.clone()),
        ));
        let _ = events.send(HostEvent::Row(TranscriptRow::error(
            NoticeScope::Settings,
            message,
        )));
        return None;
    }
    let current = match ProductConfig::load(paths) {
        Ok(current) => current,
        Err(error) => {
            let message = format!("settings: config load failed: {error}");
            let _ = events.send(HostEvent::Extension(
                SurfaceObservation::SettingsSaveFailed(message.clone()),
            ));
            let _ = events.send(HostEvent::Row(TranscriptRow::error(
                NoticeScope::Settings,
                message,
            )));
            return None;
        }
    };
    let selected_entry = update
        .default_account
        .as_ref()
        .zip(update.default_model.as_ref())
        .and_then(|(account, model)| {
            ready_catalog
                .iter()
                .find(|section| section.ready && section.account == *account)
                .and_then(|section| section.entries.iter().find(|entry| entry.id == *model))
        });
    let selected_composition: plakous_hosted::binding::CompositionSelection =
        update.composition.into();
    let exact_selection = match selected_entry {
        None => {
            update.default_account.is_none()
                && update.default_model.is_none()
                && update.default_effort.is_none()
                && update.default_extra.is_none()
        }
        Some(entry) => {
            let effort = if entry.effort_options.is_empty() {
                update.default_effort.is_none()
            } else {
                update
                    .default_effort
                    .as_ref()
                    .is_some_and(|value| entry.effort_options.contains(value))
            };
            let extra = update
                .default_extra
                .as_ref()
                .is_none_or(|value| entry.extras.iter().any(|candidate| candidate.id == *value));
            entry.unavailable_reason.is_none() && effort && extra
        }
    };
    // The Settings OpenRouter-protocol row is identity-bearing: it writes the
    // file for new sessions and never repaints a live binding (spec 8.1).
    let default_protocol = match update.protocol {
        plakous_host_protocol::ProtocolSelection::Auto => ProtocolSelection::Auto,
        plakous_host_protocol::ProtocolSelection::Chat => ProtocolSelection::Chat,
        plakous_host_protocol::ProtocolSelection::Responses => ProtocolSelection::Responses,
        plakous_host_protocol::ProtocolSelection::Messages => ProtocolSelection::Messages,
    };
    // Settings edits the root template body and the TUI/compaction rows only.
    // Roster and orchestration policy move through the successor-epoch apply.
    let mut config = current.clone();
    // Settings edits the tuple only; the template's authority fields
    // (description, tiers, delegation, hooks) stay as the file authored them.
    let requested_root = crate::product_config::AgentTemplate {
        account: update.default_account.clone().unwrap_or_default(),
        model: update.default_model.clone().unwrap_or_default(),
        effort: update.default_effort.clone(),
        extra: update.default_extra.clone(),
        composition: Some(selected_composition),
        protocol: (update.default_account.as_deref() == Some("openrouter"))
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
            let _ = events.send(HostEvent::Extension(
                SurfaceObservation::SettingsSaveFailed(message.clone()),
            ));
            let _ = events.send(HostEvent::Row(TranscriptRow::error(
                NoticeScope::Settings,
                message,
            )));
            return None;
        }
        if let Some(conflict) = composition_conflict(
            selected_composition,
            update.default_account.as_deref(),
            update.default_model.as_deref(),
            update.default_effort.as_deref(),
        ) {
            let message = format!("settings: composition conflict: {conflict}");
            let _ = events.send(HostEvent::Extension(
                SurfaceObservation::SettingsSaveFailed(message.clone()),
            ));
            let _ = events.send(HostEvent::Row(TranscriptRow::error(
                NoticeScope::Settings,
                message,
            )));
            return None;
        }
        // Commit the complete requested binding to one fresh template, then
        // publish its root pointer together with the other settings below.
        commit_root_template(&mut config, requested_root);
    }
    config.tui_mouse = update.mouse;
    config.compaction_enabled = update.compaction;
    config.scroll_lines = update.scroll_lines;
    config.show_thinking = update.show_thinking;
    config.group_tool_calls = update.group_tool_calls;
    config.reduced_motion = update.reduced_motion;
    config.theme = match update.theme {
        crate::surface_protocol::SettingsTheme::DefaultDark => {
            crate::product_config::ProductTheme::DefaultDark
        }
        crate::surface_protocol::SettingsTheme::DefaultLight => {
            crate::product_config::ProductTheme::DefaultLight
        }
        crate::surface_protocol::SettingsTheme::CatppuccinLatte => {
            crate::product_config::ProductTheme::CatppuccinLatte
        }
        crate::surface_protocol::SettingsTheme::CatppuccinFrappe => {
            crate::product_config::ProductTheme::CatppuccinFrappe
        }
        crate::surface_protocol::SettingsTheme::CatppuccinMacchiato => {
            crate::product_config::ProductTheme::CatppuccinMacchiato
        }
        crate::surface_protocol::SettingsTheme::CatppuccinMocha => {
            crate::product_config::ProductTheme::CatppuccinMocha
        }
    };
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
            let _ = events.send(HostEvent::Extension(SurfaceObservation::SettingsApplied(
                settings_snapshot(&config),
            )));
            let _ = events.send(HostEvent::Row(TranscriptRow::info(
                NoticeScope::Settings,
                note,
            )));
            Some(config)
        }
        Err(error) => {
            let message = format!("settings could not be saved: {error}");
            let _ = events.send(HostEvent::Extension(
                SurfaceObservation::SettingsSaveFailed(message.clone()),
            ));
            let _ = events.send(HostEvent::Row(TranscriptRow::error(
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
        .and_then(|mut store| store.acknowledge_agent_attempt(tree_id, acknowledgement, now));
    if let Err(error) = recorded {
        let _ = events.send(HostEvent::Row(TranscriptRow::warning(
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
            let _ = events.send(HostEvent::Extension(
                SurfaceObservation::AgentAcknowledgements {
                    tree_id: tree_id.to_owned(),
                    attempts,
                },
            ));
        }
        Err(error) => {
            let _ = events.send(HostEvent::Row(TranscriptRow::warning(
                NoticeScope::Tree,
                format!("agent acknowledgements are unavailable: {error}"),
            )));
            let _ = events.send(HostEvent::Extension(
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
    chrome_generation: std::sync::Arc<std::sync::Mutex<u64>>,
}

impl ProductSurface {
    /// The surfaces of one product home.
    #[must_use]
    pub fn new(paths: PlakPaths) -> Self {
        Self {
            paths,
            chrome_generation: std::sync::Arc::new(std::sync::Mutex::new(0)),
        }
    }

    /// A boxed handler for a [`HostConfig`](plakous_hosted::runtime::HostConfig)'s `surface` field.
    #[must_use]
    pub fn boxed(paths: &PlakPaths) -> Box<dyn SurfaceHandler<PlakExtension>> {
        Box::new(Self::new(paths.clone()))
    }
}

impl SurfaceHandler<PlakExtension> for ProductSurface {
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
        matches!(intent, SurfaceIntent::OpenUsage)
    }

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
                let _ = events.send(HostEvent::Row(TranscriptRow::info(
                    NoticeScope::Settings,
                    format!("config: {path}"),
                )));
                let _ = events.send(HostEvent::Status {
                    phase: "idle".into(),
                    queue_len: facts.queue_len,
                    detail: Some(format!("config: {path}")),
                });
            }
            SurfaceIntent::OpenSettings => emit_settings(&self.paths, events),
            SurfaceIntent::ShowMcp => {
                let body =
                    crate::commands::mcp::session_overview(&self.paths, facts.session.as_ref());
                let _ = events.send(HostEvent::Extension(SurfaceObservation::McpOverview(body)));
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
            SurfaceIntent::OpenUsage => {
                emit_settings(&self.paths, events);
                let _ = events.send(HostEvent::Extension(SurfaceObservation::OpenUsageOverlay));
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
                    std::thread::spawn(move || {
                        let config = ProductConfig::load(&paths)
                            .unwrap_or_else(|_| ProductConfig::default_product());
                        let row = session_profile(&config, paths.home(), &session);
                        let label = crate::product_workspace::workspace_label(&session.workspace);
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
            SurfaceIntent::ApplyProfile { name, fork_now } => {
                return SurfaceEffect {
                    profile_fork: apply_profile(&self.paths, &name, fork_now, facts, events),
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

    fn refresh_readiness(
        &self,
    ) -> Result<
        (
            Vec<plakous_host_protocol::views::ProviderDef>,
            Vec<plakous_host_protocol::views::ProviderSection>,
        ),
        String,
    > {
        crate::presentation::tui_presentation_values(&self.paths)
            .map_err(|error| error.message().to_owned())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    #[test]
    fn stale_composer_refresh_cannot_publish_profile_or_workspace() {
        let config = ProductConfig::default_product();
        let session = plakous_hosted::runtime::SurfaceSession {
            account: None,
            model: None,
            effort: None,
            fast: false,
            workspace: std::path::PathBuf::from("."),
            roster: None,
            mcp: plakous_hosted::runtime::SurfaceMcp::default(),
        };
        let row = session_profile(&config, std::path::Path::new("."), &session);
        let (events, rx) = host_event_sink();
        let generation = std::sync::Mutex::new(2);
        publish_composer_chrome(&generation, 1, row.clone(), "old".into(), &events);
        assert!(rx.try_recv().is_err());
        publish_composer_chrome(&generation, 2, row, "current".into(), &events);
        assert!(matches!(
            rx.try_recv().unwrap(),
            HostEvent::Extension(SurfaceObservation::SessionProfile(_))
        ));
        assert_eq!(
            rx.try_recv().unwrap(),
            HostEvent::Extension(SurfaceObservation::WorkspaceChrome("current".into()))
        );
    }
    #[test]
    fn composer_refresh_uses_the_launch_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let workspace = dir.path().join("launch-workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let paths = PlakPaths::explicit(home);
        ProductConfig::default_product().save(&paths).unwrap();
        let expected = crate::product_workspace::workspace_label(&workspace);
        let (events, rx) = host_event_sink();
        let session = plakous_hosted::runtime::SurfaceSession {
            account: None,
            model: None,
            effort: None,
            fast: false,
            workspace,
            roster: None,
            mcp: plakous_hosted::runtime::SurfaceMcp::default(),
        };
        ProductSurface::new(paths).dispatch(
            SurfaceIntent::RefreshComposerChrome,
            SurfaceFacts {
                phase: SurfacePhase::Detached,
                attached_one_shot: false,
                catalog: &[],
                queue_len: 0,
                session: Some(session),
            },
            &events,
        );
        assert!(
            matches!(rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap(),
            HostEvent::Extension(SurfaceObservation::SessionProfile(row)) if row.id == "custom")
        );
        assert_eq!(
            rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap(),
            HostEvent::Extension(SurfaceObservation::WorkspaceChrome(expected))
        );
    }

    #[test]
    fn session_identity_uses_frozen_cards_not_current_config() {
        let dir = tempfile::tempdir().unwrap();
        let config = ProductConfig::default_product();
        let prepared = crate::profiles::prepare(&config, dir.path(), "claude-default").unwrap();
        let root = &prepared.root;
        let frozen = crate::roster_input::snapshot_from_config(
            &prepared.config,
            root.account.clone(),
            root.model.clone(),
            root.effort.clone(),
        );
        let mut session = plakous_hosted::runtime::SurfaceSession {
            account: Some(root.account.clone()),
            model: Some(root.model.clone()),
            effort: root.effort.clone(),
            fast: root.is_fast(),
            workspace: dir.path().to_owned(),
            roster: Some(frozen),
            mcp: plakous_hosted::runtime::SurfaceMcp::default(),
        };
        let matched = session_profile(&prepared.config, dir.path(), &session);
        assert_eq!(matched.id, "claude-default");
        let working_set = |row: &crate::surface_protocol::ProfileRow| {
            row.cards.iter().filter(|card| card.depth == 0).count()
        };
        assert_eq!(working_set(&matched), 3);
        let frozen = session.roster.as_mut().unwrap();
        frozen.roster.values_mut().next().unwrap().tuple_id = "frozen-session-model".into();
        frozen.templates_digest = "different-frozen-roster".into();
        let custom = session_profile(&prepared.config, dir.path(), &session);
        assert_eq!(custom.id, "custom");
        assert_eq!(working_set(&custom), 3);
        assert!(
            custom
                .cards
                .iter()
                .any(|card| card.model == "frozen-session-model")
        );
        session.roster = None;
        let without_tree = session_profile(&prepared.config, dir.path(), &session);
        assert_eq!(without_tree.id, "custom");
        assert_eq!(
            without_tree.cards,
            [] as [crate::surface_protocol::ProfileRole; 0]
        );
    }

    /// Settings lists the frozen working set in config order, the same shape
    /// Profiles uses, including a one-line reference when one working-set
    /// card names another.
    #[test]
    fn session_profile_keeps_config_order_and_sibling_references() {
        let dir = tempfile::tempdir().unwrap();
        let paths = PlakPaths::explicit(dir.path().join("home"));
        paths.ensure_layout().unwrap();
        for (name, extra) in [
            ("zeta", "spawns = [\"alpha\", \"zeta\"]\n"),
            ("alpha", "spawns = [\"note\"]\n"),
            ("note", ""),
            ("rootish", ""),
        ] {
            std::fs::write(
                paths.user_agents_dir().join(format!("{name}.toml")),
                format!(
                    "account = \"codex\"\nmodel = \"gpt-5.6-luna\"\ndescription = \"{name}\"\n{extra}"
                ),
            )
            .unwrap();
        }
        std::fs::create_dir_all(paths.home().join("profiles")).unwrap();
        std::fs::write(
            paths.home().join("profiles/nest.toml"),
            "root = \"rootish\"\nworking_set = [\"zeta\", \"alpha\"]\n",
        )
        .unwrap();
        let config = ProductConfig::load(&paths).unwrap();
        let prepared = crate::profiles::prepare(&config, paths.home(), "nest").unwrap();
        let root = &prepared.root;
        let session = plakous_hosted::runtime::SurfaceSession {
            account: Some(root.account.clone()),
            model: Some(root.model.clone()),
            effort: root.effort.clone(),
            fast: root.is_fast(),
            workspace: dir.path().to_owned(),
            roster: Some(crate::roster_input::snapshot_from_config(
                &prepared.config,
                root.account.clone(),
                root.model.clone(),
                root.effort.clone(),
            )),
            mcp: plakous_hosted::runtime::SurfaceMcp::default(),
        };
        let shape = |cards: &[crate::surface_protocol::ProfileRole]| {
            cards
                .iter()
                .map(|card| (card.label.clone(), card.depth))
                .collect::<Vec<_>>()
        };
        let profile = crate::profiles::list(&prepared.config, paths.home())
            .rows
            .into_iter()
            .find(|row| row.id == "nest")
            .unwrap();
        let settings = session_profile(&prepared.config, paths.home(), &session);
        assert_eq!(settings.id, "nest");
        assert_eq!(
            shape(&settings.cards),
            [
                ("zeta".to_owned(), 0),
                ("alpha".to_owned(), 1),
                ("alpha".to_owned(), 0),
                ("note".to_owned(), 1),
            ]
        );
        assert_eq!(shape(&settings.cards), shape(&profile.cards));
    }

    #[test]
    fn default_effort_identity_survives_none_and_explicit_default() {
        let dir = tempfile::tempdir().unwrap();
        let config = ProductConfig::default_product();
        let mut prepared = crate::profiles::prepare(&config, dir.path(), "claude-default").unwrap();
        let root_name = prepared.config.root_agent.clone().unwrap();
        prepared
            .config
            .templates
            .get_mut(&root_name)
            .unwrap()
            .effort = None;
        let root = prepared.config.root_template().unwrap();
        let session = plakous_hosted::runtime::SurfaceSession {
            account: Some(root.account.clone()),
            model: Some(root.model.clone()),
            effort: None,
            fast: root.is_fast(),
            workspace: dir.path().to_owned(),
            roster: Some(crate::roster_input::snapshot_from_config(
                &prepared.config,
                root.account.clone(),
                root.model.clone(),
                None,
            )),
            mcp: plakous_hosted::runtime::SurfaceMcp::default(),
        };
        assert_eq!(
            session_profile(&prepared.config, dir.path(), &session).id,
            "claude-default"
        );
        let explicit = plakous_hosted::runtime::SurfaceSession {
            effort: Some("default".into()),
            ..session
        };
        assert_eq!(
            session_profile(&prepared.config, dir.path(), &explicit).id,
            "claude-default"
        );
    }

    #[test]
    fn child_wake_reads_only_config_and_observes_immediate_changes() {
        use super::*;
        use std::ffi::OsStr;
        let dir = tempfile::tempdir().unwrap();
        let paths =
            PlakPaths::resolve(Some(OsStr::new(dir.path())), Some(OsStr::new("/home/u"))).unwrap();
        paths.ensure_layout().unwrap();
        let surface = ProductSurface::new(paths.clone());
        assert_eq!(surface.child_wake_enabled(), Some(true));
        std::fs::write(
            paths.user_agents_dir().join("broken.toml"),
            "not valid [toml",
        )
        .unwrap();
        std::fs::write(paths.config_file(), "[turn]\nauto_continue = false\n").unwrap();
        assert_eq!(surface.child_wake_enabled(), Some(false));
        std::fs::write(paths.config_file(), "[turn]\nauto_continue = true\n").unwrap();
        assert_eq!(surface.child_wake_enabled(), Some(true));
        std::fs::write(paths.config_file(), "[turn]\nauto_continue = 1\n").unwrap();
        assert_eq!(surface.child_wake_enabled(), None);
    }
    use super::*;
    use crate::surface_protocol::{AgentAcknowledgement, SurfaceIntent, UserAction};
    use plakous_hosted::runtime::{ShellHostConfig, note_idle_only};
    use plakous_hosted::sessions::SessionRecord;

    use crate::home::paths::PlakPaths;
    use crate::product_config::ProductConfig;
    use plakous_hosted::binding::CompositionSelection as ProductComposition;

    /// The event sink the hosted fixtures hand a settings save.
    fn host_event_sink() -> (
        Sender<PlakExtension>,
        crate::surface_protocol::HostEventReceiver,
    ) {
        let (tx, rx) = crate::surface_protocol::host_event_channel();
        (Sender::over(tx), rx)
    }

    #[test]
    fn an_agent_acknowledgement_is_persisted_and_answered_with_the_complete_set() {
        let (_root, paths) = PlakPaths::isolated_test_home();
        let facts = || SurfaceFacts {
            phase: SurfacePhase::Busy,
            attached_one_shot: false,
            catalog: &[],
            queue_len: 0,
            session: None,
        };
        let (events, rx) = host_event_sink();
        let surface = ProductSurface::new(paths.clone());
        let occurrence = AgentAcknowledgement {
            agent_id: "c3".into(),
            attempt_id: "c3-a1".into(),
            attention: "needs recovery".into(),
            ended_at: Some(10),
        };
        surface.dispatch(
            SurfaceIntent::AcknowledgeAgent {
                tree_id: "tree-1".into(),
                agent_id: occurrence.agent_id.clone(),
                attempt_id: occurrence.attempt_id.clone(),
                attention: occurrence.attention.clone(),
                ended_at: occurrence.ended_at,
            },
            facts(),
            &events,
        );
        let expected = HostEvent::Extension(SurfaceObservation::AgentAcknowledgements {
            tree_id: "tree-1".into(),
            attempts: vec![occurrence],
        });
        assert_eq!(rx.try_recv().unwrap(), expected);
        // A fresh surface over the same home is a restart: the set is still there.
        ProductSurface::new(paths).dispatch(
            SurfaceIntent::ListAgentAcknowledgements {
                tree_id: "tree-1".into(),
            },
            facts(),
            &events,
        );
        assert_eq!(rx.try_recv().unwrap(), expected);
        surface.dispatch(
            SurfaceIntent::AcknowledgeAgent {
                tree_id: "tree-1".into(),
                agent_id: String::new(),
                attempt_id: "c3-a2".into(),
                attention: "failed".into(),
                ended_at: None,
            },
            facts(),
            &events,
        );
        assert!(
            rx.try_recv().is_err(),
            "an id-less acknowledgement is dropped"
        );
    }

    #[test]
    fn every_product_intent_uses_immutable_phase_facts_and_bounded_observations() {
        for phase in [
            SurfacePhase::Idle,
            SurfacePhase::Busy,
            SurfacePhase::Detached,
        ] {
            for (intent_index, attached_one_shot) in
                (0..4).flat_map(|intent| [false, true].map(|attached| (intent, attached)))
            {
                let (_root, paths) = PlakPaths::isolated_test_home();
                let config = ProductConfig::default_product();
                config.save(&paths).unwrap();
                let handler = ProductSurface::new(paths.clone());
                let intent = match intent_index {
                    0 => SurfaceIntent::OpenConfigFile,
                    1 => SurfaceIntent::OpenSettings,
                    2 => SurfaceIntent::SaveSettings(settings_snapshot(&config)),
                    3 => SurfaceIntent::OpenUsage,
                    _ => unreachable!(),
                };
                assert_eq!(handler.needs_usage(&intent), intent_index == 3);
                let (events, received) = host_event_sink();
                let effect = handler.dispatch(
                    intent.clone(),
                    SurfaceFacts {
                        session: None,
                        phase,
                        attached_one_shot,
                        catalog: &[],
                        queue_len: 7,
                    },
                    &events,
                );
                let observed = received.try_iter().collect::<Vec<_>>();
                assert_eq!(effect.saved.is_some(), intent_index == 2);
                match intent {
                    SurfaceIntent::OpenConfigFile if phase == SurfacePhase::Busy => assert_eq!(
                        observed,
                        vec![HostEvent::Row(TranscriptRow::info(
                            NoticeScope::Session,
                            "config: available when idle (finish or interrupt the turn)"
                        ))]
                    ),
                    SurfaceIntent::OpenConfigFile => {
                        assert!(observed.iter().any(|event| matches!(event,
                        HostEvent::Status { phase, queue_len: 7, detail: Some(detail) }
                        if phase == "idle" && detail.starts_with("config: "))));
                    }
                    SurfaceIntent::OpenSettings => assert!(matches!(
                        observed.as_slice(),
                        [HostEvent::Extension(SurfaceObservation::SettingsSnapshot(
                            _
                        ))]
                    )),
                    SurfaceIntent::SaveSettings(_) => {
                        let saved = ProductConfig::load(&paths).unwrap();
                        assert_eq!(
                            effect.saved.unwrap().root_template,
                            saved.root_template_fact()
                        );
                        assert!(observed.iter().any(|event| matches!(
                            event,
                            HostEvent::Extension(SurfaceObservation::SettingsApplied(_))
                        )));
                    }
                    SurfaceIntent::OpenUsage
                        if phase == SurfacePhase::Idle && attached_one_shot =>
                    {
                        assert_eq!(
                            observed,
                            [] as [plakous_host_protocol::SessionObservation<PlakExtension>; 0]
                        );
                    }
                    SurfaceIntent::OpenUsage => assert!(matches!(
                        observed.as_slice(),
                        [
                            HostEvent::Extension(SurfaceObservation::SettingsSnapshot(_)),
                            HostEvent::Extension(SurfaceObservation::OpenUsageOverlay),
                        ]
                    )),
                    SurfaceIntent::ListProfiles
                    | SurfaceIntent::RefreshComposerChrome
                    | SurfaceIntent::ShowMcp
                    | SurfaceIntent::ApplyProfile { .. }
                    | SurfaceIntent::AcknowledgeAgent { .. }
                    | SurfaceIntent::ListAgentAcknowledgements { .. } => {
                        unreachable!("profile and acknowledgement intents have their own tests")
                    }
                }
            }
        }
    }

    #[test]
    fn refused_product_save_returns_no_binding_effect_and_preserves_the_file() {
        let (_root, paths) = PlakPaths::isolated_test_home();
        let config = ProductConfig::default_product();
        config.save(&paths).unwrap();
        let before = std::fs::read(paths.config_file()).unwrap();
        let mut update = settings_snapshot(&config);
        update.default_account = Some("unadmitted".into());
        update.default_model = Some("unadmitted-model".into());
        let (events, received) = host_event_sink();
        let effect = ProductSurface::new(paths.clone()).dispatch(
            SurfaceIntent::SaveSettings(update),
            SurfaceFacts {
                session: None,
                phase: SurfacePhase::Busy,
                attached_one_shot: false,
                catalog: &[],
                queue_len: 3,
            },
            &events,
        );
        assert!(effect.saved.is_none());
        assert_eq!(std::fs::read(paths.config_file()).unwrap(), before);
        assert!(received.try_iter().any(|event| matches!(
            event,
            HostEvent::Extension(SurfaceObservation::SettingsSaveFailed(_))
        )));
    }

    /// A config whose `[root] agent` names the one working-set template, so a
    /// Settings save is deciding whether to rewrite a file-authored root.
    fn roster_shared_root_config() -> ProductConfig {
        let mut config = ProductConfig::default_product();
        config.templates.insert(
            "qa".to_owned(),
            crate::product_config::AgentTemplate {
                account: "codex".to_owned(),
                model: "gpt-5.6-luna".to_owned(),
                effort: Some("low".to_owned()),
                description: "qa worker".to_owned(),
                ..crate::product_config::AgentTemplate::default()
            },
        );
        config.root_agent = Some("qa".to_owned());
        config.working_set =
            plakous_hosted::agent_tree_protocol::WorkingSet::parse(["qa"]).unwrap();
        config
    }

    /// A settings notice names the row the operator actually changed.
    ///
    /// The fixed line said `theme applied` after every toggle, so flipping
    /// `Show thinking blocks` reported a theme change that had not happened
    /// and had in fact just been reverted.
    #[test]
    fn a_settings_notice_names_only_what_changed() {
        let before = ProductConfig::default_product();

        let mut after = before.clone();
        after.show_thinking = !before.show_thinking;
        let note = settings_saved_note(&before, &after, false);
        assert_eq!(note, "settings saved \u{b7} show thinking blocks");
        assert!(
            !note.contains("theme"),
            "a toggle must not report a theme change: {note}"
        );

        let mut themed = before.clone();
        themed.theme = if before.theme == crate::product_config::ProductTheme::DefaultDark {
            crate::product_config::ProductTheme::DefaultLight
        } else {
            crate::product_config::ProductTheme::DefaultDark
        };
        assert_eq!(
            settings_saved_note(&before, &themed, false),
            "settings saved \u{b7} theme applied"
        );

        let mut both = themed.clone();
        both.reduced_motion = !before.reduced_motion;
        assert_eq!(
            settings_saved_note(&before, &both, false),
            "settings saved \u{b7} theme applied, reduced motion"
        );

        // A save that changed nothing says so rather than claiming a theme.
        assert_eq!(
            settings_saved_note(&before, &before, false),
            "settings saved \u{b7} nothing changed"
        );
        assert_eq!(
            settings_saved_note(&before, &before, true),
            "settings saved \u{b7} defaults (next launch)"
        );
    }

    #[test]
    fn settings_status_rejects_composition_without_an_exact_tuple() {
        for composition in [
            ProductComposition::Direct,
            ProductComposition::NativePtc,
            ProductComposition::JsPtc,
            ProductComposition::PyPtc,
        ] {
            assert!(
                composition_conflict(composition, None, None, None)
                    .is_some_and(|reason| reason.contains("exact default account/model tuple"))
            );
        }
        assert_eq!(
            composition_conflict(ProductComposition::FollowCatalog, None, None, None),
            None
        );
        assert!(
            composition_conflict(
                ProductComposition::NativePtc,
                Some("openrouter"),
                Some("vendor/model"),
                None,
            )
            .is_some_and(|reason| reason.contains("OpenRouter has no native programmatic"))
        );
    }

    /// `native-ptc` follows each vertical's own enrollment and the refusal
    /// names the vertical and the harness alternatives.
    #[test]
    fn settings_native_ptc_follows_the_vertical_enrollment() {
        let conflict = |account: &str, model: &str| {
            composition_conflict(
                ProductComposition::NativePtc,
                Some(account),
                Some(model),
                None,
            )
        };
        // Codex enrolls Code Mode under either account spelling; the OpenAI
        // API enrolls it on its CodeModeOnly models.
        assert_eq!(conflict("codex", "gpt-5.6-sol"), None);
        assert_eq!(conflict("chatgpt", "gpt-5.6-sol"), None);
        assert_eq!(conflict("openai", "gpt-5.6-sol"), None);
        for (account, model, vertical) in [
            ("claude", "claude-sonnet-5", "Claude has no native"),
            ("grok", "grok-4.6", "Grok has no native"),
            ("supergrok", "grok-4.6", "Grok has no native"),
            (
                "anthropic",
                "claude-sonnet-5",
                "the Anthropic API has no native",
            ),
            ("xai-api", "grok-4.6", "the xAI API has no native"),
            ("openrouter", "vendor/model", "OpenRouter has no native"),
        ] {
            let reason = conflict(account, model).unwrap_or_else(|| panic!("{account} admitted"));
            assert!(reason.contains(vertical), "{account}: {reason}");
            assert!(
                reason.contains("use js-ptc, py-ptc or direct"),
                "{account}: {reason}"
            );
        }
    }

    /// The exact-Ready-catalog gate ran before the host knew
    /// whether the save changed the binding at all, so a theme, mouse or
    /// reading-width save was refused with
    /// `defaults are not admitted by the exact Ready catalog` whenever the root
    /// template's account was not logged in — the seeded config's own state.
    /// readiness is an affordance, not a gate on unrelated writes.
    #[test]
    fn a_view_only_save_under_a_non_ready_root_applies_and_leaves_the_templates_alone() {
        use std::ffi::OsStr;
        let dir = tempfile::tempdir().unwrap();
        let paths =
            PlakPaths::resolve(Some(OsStr::new(dir.path())), Some(OsStr::new("/home/u"))).unwrap();
        paths.ensure_layout().unwrap();
        let mut seeded = ProductConfig::default_product();
        seeded.templates.insert(
            "primary".to_owned(),
            crate::product_config::AgentTemplate {
                account: "grok".to_owned(),
                model: "grok-4.6".to_owned(),
                ..crate::product_config::AgentTemplate::default()
            },
        );
        seeded.root_agent = Some("primary".to_owned());
        seeded.save(&paths).unwrap();
        let before = std::fs::read_to_string(paths.config_file()).unwrap();
        let before_template =
            std::fs::read_to_string(paths.user_agents_dir().join("primary.toml")).unwrap();
        // Not logged in: the catalog holds no Ready section for the root.
        let catalog: Vec<plakous_host_protocol::views::ProviderSection> = Vec::new();

        let view_only = crate::surface_protocol::SettingsUpdate {
            default_account: Some("grok".into()),
            default_model: Some("grok-4.6".into()),
            default_effort: None,
            default_extra: None,
            composition: plakous_host_protocol::CompositionSelection::FollowCatalog,
            theme: crate::surface_protocol::SettingsTheme::CatppuccinMacchiato,
            ..crate::surface_protocol::SettingsUpdate::default()
        };
        let (events, rx) = host_event_sink();
        save_settings(&paths, view_only, &catalog, &events);
        let observed = rx.try_iter().collect::<Vec<_>>();
        assert!(
            observed.iter().any(|event| matches!(
                event,
                HostEvent::Extension(SurfaceObservation::SettingsApplied(_))
            )),
            "a view-only save always succeeds: {observed:?}"
        );
        assert!(!observed.iter().any(|event| matches!(
            event,
            HostEvent::Extension(SurfaceObservation::SettingsSaveFailed(_))
        )));
        let saved = ProductConfig::load(&paths).unwrap();
        assert_eq!(
            saved.theme,
            crate::product_config::ProductTheme::CatppuccinMacchiato
        );
        assert_eq!(saved.root_agent.as_deref(), Some("primary"));
        assert_eq!(saved.templates.len(), 1);
        assert_eq!(
            saved.templates.get("primary").map(|t| t.model.as_str()),
            Some("grok-4.6")
        );
        assert!(
            !saved.templates.contains_key("default"),
            "no default template is minted by a view row"
        );
        let after = std::fs::read_to_string(paths.config_file()).unwrap();
        let root_lines = |text: &str| {
            text.lines()
                .filter(|line| line.starts_with("[root]") || line.starts_with("agent ="))
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            root_lines(&before),
            root_lines(&after),
            "[root] is byte-identical"
        );
        assert_eq!(
            std::fs::read_to_string(paths.user_agents_dir().join("primary.toml")).unwrap(),
            before_template,
            "the template file body is byte-identical"
        );

        // The identity rows are still validated, with the exact reason.
        let identity = crate::surface_protocol::SettingsUpdate {
            default_account: Some("grok".into()),
            default_model: Some("grok-4.5".into()),
            default_effort: None,
            default_extra: None,
            composition: plakous_host_protocol::CompositionSelection::FollowCatalog,
            ..crate::surface_protocol::SettingsUpdate::default()
        };
        let (events, rx) = host_event_sink();
        save_settings(&paths, identity, &catalog, &events);
        let observed = rx.try_iter().collect::<Vec<_>>();
        assert!(observed.iter().any(|event| matches!(
            event,
            HostEvent::Extension(SurfaceObservation::SettingsSaveFailed(message))
                if message == "settings: defaults are not admitted by the exact Ready catalog"
        )));
        assert_eq!(
            ProductConfig::load(&paths)
                .unwrap()
                .templates
                .get("primary")
                .map(|t| t.model.as_str()),
            Some("grok-4.6"),
            "a refused identity save writes nothing"
        );
    }

    fn codex_catalog() -> Vec<plakous_host_protocol::views::ProviderSection> {
        vec![plakous_host_protocol::views::ProviderSection {
            account: "codex".into(),
            label: "Codex".into(),
            ready: true,
            entries: vec![plakous_host_protocol::views::ModelEntry {
                id: "gpt-exact".into(),
                tag: "reasoning".into(),
                reasoning: true,
                effort_options: vec!["medium".into()],
                extras: Vec::new(),
                unavailable_reason: None,
            }],
        }]
    }

    /// A home with a user profile `mine` rooted on codex `gpt-exact`.
    fn profile_paths(dir: &std::path::Path) -> PlakPaths {
        use std::ffi::OsStr;
        let paths = PlakPaths::resolve(Some(OsStr::new(dir)), Some(OsStr::new("/home/u"))).unwrap();
        paths.ensure_layout().unwrap();
        std::fs::create_dir_all(dir.join("agents")).unwrap();
        std::fs::create_dir_all(dir.join("profiles")).unwrap();
        std::fs::write(
            dir.join("agents/mine-root.toml"),
            "account = \"codex\"\nmodel = \"gpt-exact\"\neffort = \"medium\"\ndescription = \"root\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("agents/mine-worker.toml"),
            "account = \"codex\"\nmodel = \"gpt-exact\"\neffort = \"medium\"\ndescription = \"worker\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("profiles/mine.toml"),
            "root = \"mine-root\"\nworking_set = [\"mine-worker\"]\n",
        )
        .unwrap();
        paths
    }

    fn apply(
        paths: &PlakPaths,
        name: &str,
        fork_now: bool,
        phase: SurfacePhase,
    ) -> (SurfaceEffect, Vec<HostEvent>) {
        let (events, rx) = host_event_sink();
        let catalog = codex_catalog();
        let effect = ProductSurface::new(paths.clone()).dispatch(
            SurfaceIntent::ApplyProfile {
                name: name.to_owned(),
                fork_now,
            },
            SurfaceFacts {
                session: None,
                phase,
                attached_one_shot: false,
                catalog: &catalog,
                queue_len: 0,
            },
            &events,
        );
        drop(events);
        (effect, rx.try_iter().collect())
    }

    #[test]
    fn next_session_saves_the_profile_and_reports_it() {
        let dir = tempfile::tempdir().unwrap();
        let paths = profile_paths(dir.path());
        let (effect, observed) = apply(&paths, "mine", false, SurfacePhase::Idle);
        assert!(effect.profile_fork.is_none());
        assert!(observed.iter().any(|event| matches!(
            event,
            HostEvent::Extension(SurfaceObservation::ProfileApplied { name, outcome: ProfileOutcome::Saved })
                if name == "mine"
        )));
        assert!(observed.iter().any(|event| matches!(
            event,
            HostEvent::Extension(SurfaceObservation::Profiles(view)) if view.current.as_deref() == Some("mine")
        )));
        assert_eq!(
            ProductConfig::load(&paths).unwrap().root_agent.as_deref(),
            Some("mine-root")
        );
    }

    #[test]
    fn fork_now_saves_only_once_the_host_reports_it_landed() {
        let dir = tempfile::tempdir().unwrap();
        let paths = profile_paths(dir.path());
        let before = std::fs::read(paths.config_file()).ok();

        let (effect, observed) = apply(&paths, "mine", true, SurfacePhase::Idle);
        assert!(
            observed.is_empty(),
            "nothing is reported before the host answers"
        );
        let fork = effect.profile_fork.expect("a fork request");
        assert_eq!(
            (fork.account.as_str(), fork.model.as_str()),
            ("codex", "gpt-exact")
        );
        assert!(
            fork.roster
                .templates
                .keys()
                .any(|name| name.as_str() == "mine-worker")
        );
        assert_eq!(
            std::fs::read(paths.config_file()).ok(),
            before,
            "nothing saved yet"
        );
        fork.refuse("rebind refused");
        assert_eq!(
            std::fs::read(paths.config_file()).ok(),
            before,
            "a refusal saves nothing"
        );

        let (effect, _) = apply(&paths, "mine", true, SurfacePhase::Idle);
        let fork = effect.profile_fork.expect("a fork request");
        (fork.finish)(ProfileForkOutcome::Forked);
        assert_eq!(
            ProductConfig::load(&paths).unwrap().root_agent.as_deref(),
            Some("mine-root")
        );
    }

    #[test]
    fn fork_now_during_a_turn_is_refused_before_the_host_sees_it() {
        let dir = tempfile::tempdir().unwrap();
        let paths = profile_paths(dir.path());
        let (effect, observed) = apply(&paths, "mine", true, SurfacePhase::Busy);
        assert!(effect.profile_fork.is_none());
        assert!(observed.iter().any(|event| matches!(
            event,
            HostEvent::Extension(SurfaceObservation::ProfileRefused { reason, .. }) if reason.contains("turn is running")
        )));
    }

    /// A fresh home's unattached session takes the profile's root and
    /// attaches on it, instead of refusing "fork now needs an idle attached
    /// session".
    #[test]
    fn an_unattached_empty_session_binds_the_profile_and_reattaches() {
        let dir = tempfile::tempdir().unwrap();
        let paths = profile_paths(dir.path());
        let session = SessionRecord::new(dir.path().to_path_buf());
        let (tx, rx, join) = crate::shell::host::spawn_shell_host(ShellHostConfig {
            surface: Some(ProductSurface::boxed(&paths)),
            binding: crate::shell::binding_document::ProductRootBindingDocument::shared(&paths),
            subscriptions: crate::shell::host::open_product_subscriptions(),
            openrouter_credentials: crate::openrouter_product::ProductOpenRouterCredentials::shared(
                &paths,
            ),
            product_home: paths.home().to_path_buf(),
            home: paths.session_home(),
            store: std::sync::Arc::new(crate::store::session_records::SqliteSessionStore::new(
                paths.clone(),
            )),
            session,
            cwd: dir.path().to_path_buf(),
            reason: "missing_defaults".into(),
            ready_catalog: plakous_host_protocol::views::ReadyCatalog::new(codex_catalog()),
        });
        tx.send(UserAction::Extension(SurfaceIntent::ApplyProfile {
            name: "mine".into(),
            fork_now: true,
        }))
        .unwrap();
        join.join().unwrap().unwrap();
        let observed = rx.try_iter().collect::<Vec<_>>();
        assert!(
            observed.iter().any(|event| matches!(
                event,
                HostEvent::Extension(SurfaceObservation::ProfileApplied {
                    outcome: ProfileOutcome::Bound,
                    ..
                })
            )),
            "{observed:#?}"
        );
        assert!(
            observed
                .iter()
                .any(|event| matches!(event, HostEvent::ReattachReady))
        );
        assert_eq!(
            ProductConfig::load(&paths).unwrap().root_agent.as_deref(),
            Some("mine-root")
        );
    }

    /// Use now on an unattached, empty session binds an OpenRouter root with
    /// the wire and composition its template names, or a direct API root with
    /// its route and effort, then attaches on it.
    #[test]
    fn use_now_binds_an_openrouter_or_api_root_from_its_template() {
        use crate::product_config::{
            ProductOpenRouterRoutingSelection, ProductProtocolSelection, ProductTargetSelection,
            ProductWireBinding,
        };
        let openrouter = (
            "account = \"openrouter\"\nmodel = \"vendor/model\"\nprotocol = \"responses\"\ncomposition = \"py-ptc\"\n",
            "vendor/model",
            ProductTargetSelection::OpenRouter {
                wire: ProductWireBinding::resolve(ProductProtocolSelection::Responses),
                routing: ProductOpenRouterRoutingSelection::GatewayManaged,
            },
            None,
            crate::product_config::ProductCompositionSelection::PyPtc,
        );
        let api = (
            "account = \"openai\"\nmodel = \"gpt-5.6-sol\"\neffort = \"low\"\n",
            "gpt-5.6-sol",
            ProductTargetSelection::Platform {
                provider: plakous_hosted::binding::PlatformProvider::OpenAi,
            },
            Some("low"),
            // plakous-default follows the OpenAI API's enrolled CodeModeOnly row.
            crate::product_config::ProductCompositionSelection::NativePtc,
        );
        for (template, model, target, effort, composition) in [openrouter, api] {
            let account = template.split('"').nth(1).unwrap().to_owned();
            let dir = tempfile::tempdir().unwrap();
            let paths = PlakPaths::resolve(
                Some(std::ffi::OsStr::new(dir.path())),
                Some(std::ffi::OsStr::new("/home/u")),
            )
            .unwrap();
            paths.ensure_layout().unwrap();
            std::fs::create_dir_all(dir.path().join("agents")).unwrap();
            std::fs::create_dir_all(dir.path().join("profiles")).unwrap();
            std::fs::write(
                dir.path().join("agents/api-root.toml"),
                format!("{template}description = \"root\"\n"),
            )
            .unwrap();
            std::fs::write(
                dir.path().join("agents/api-worker.toml"),
                format!("{template}description = \"worker\"\n"),
            )
            .unwrap();
            std::fs::write(
                dir.path().join("profiles/api.toml"),
                "root = \"api-root\"\nworking_set = [\"api-worker\"]\n",
            )
            .unwrap();
            let session = SessionRecord::new(dir.path().to_path_buf());
            let id = session.id.clone();
            let store = std::sync::Arc::new(
                crate::store::session_records::SqliteSessionStore::new(paths.clone()),
            );
            let (tx, rx, join) = crate::shell::host::spawn_shell_host(ShellHostConfig {
                surface: Some(ProductSurface::boxed(&paths)),
                binding: crate::shell::binding_document::ProductRootBindingDocument::shared(&paths),
                subscriptions: crate::shell::host::open_product_subscriptions(),
                openrouter_credentials:
                    crate::openrouter_product::ProductOpenRouterCredentials::shared(&paths),
                product_home: paths.home().to_path_buf(),
                home: paths.session_home(),
                store: store.clone(),
                session,
                cwd: dir.path().to_path_buf(),
                reason: "missing_defaults".into(),
                ready_catalog: plakous_host_protocol::views::ReadyCatalog::new(vec![
                    plakous_host_protocol::views::ProviderSection {
                        account: account.clone(),
                        label: account.clone(),
                        ready: true,
                        entries: vec![plakous_host_protocol::views::ModelEntry {
                            id: model.into(),
                            tag: "api".into(),
                            reasoning: false,
                            effort_options: Vec::new(),
                            extras: Vec::new(),
                            unavailable_reason: None,
                        }],
                    },
                ]),
            });
            tx.send(UserAction::Extension(SurfaceIntent::ApplyProfile {
                name: "api".into(),
                fork_now: true,
            }))
            .unwrap();
            drop(tx);
            join.join().unwrap().unwrap();
            let observed = rx.try_iter().collect::<Vec<_>>();
            assert!(
                observed.iter().any(|event| matches!(
                    event,
                    HostEvent::Extension(SurfaceObservation::ProfileApplied {
                        outcome: ProfileOutcome::Bound,
                        ..
                    })
                )),
                "{account}: {observed:#?}"
            );
            assert!(
                observed
                    .iter()
                    .any(|event| matches!(event, HostEvent::ReattachReady)),
                "{account}: {observed:#?}"
            );
            assert_eq!(
                ProductConfig::load(&paths).unwrap().root_agent.as_deref(),
                Some("api-root"),
                "{account}"
            );
            let bound =
                plakous_hosted::session_store::SessionRecordStore::load(&*store, &id).unwrap();
            assert_eq!(bound.account.as_deref(), Some(account.as_str()));
            assert_eq!(bound.model.as_deref(), Some(model));
            assert_eq!(bound.target_selection, target, "{account}");
            assert_eq!(bound.effort.as_deref(), effort, "{account}");
            assert_eq!(bound.composition, composition, "{account}");
        }
    }

    #[test]
    fn settings_save_uses_exact_ready_catalog_not_effort_name_heuristics() {
        use std::ffi::OsStr;
        let dir = tempfile::tempdir().unwrap();
        let paths =
            PlakPaths::resolve(Some(OsStr::new(dir.path())), Some(OsStr::new("/home/u"))).unwrap();
        paths.ensure_layout().unwrap();
        let catalog = vec![plakous_host_protocol::views::ProviderSection {
            account: "codex".into(),
            label: "Codex".into(),
            ready: true,
            entries: vec![
                plakous_host_protocol::views::ModelEntry {
                    id: "gpt-exact".into(),
                    tag: "reasoning".into(),
                    reasoning: true,
                    effort_options: vec!["bespoke".into()],
                    extras: Vec::new(),
                    unavailable_reason: None,
                },
                plakous_host_protocol::views::ModelEntry {
                    id: "gpt-unqualified".into(),
                    tag: "not qualified".into(),
                    reasoning: true,
                    effort_options: vec!["medium".into()],
                    extras: Vec::new(),
                    unavailable_reason: Some("tuple not qualified".into()),
                },
            ],
        }];
        let session = SessionRecord::new(dir.path().to_path_buf());
        let (tx, rx, join) = crate::shell::host::spawn_shell_host(ShellHostConfig {
            surface: Some(ProductSurface::boxed(&paths)),
            binding: crate::shell::binding_document::ProductRootBindingDocument::shared(&paths),
            subscriptions: crate::shell::host::open_product_subscriptions(),
            openrouter_credentials: crate::openrouter_product::ProductOpenRouterCredentials::shared(
                &paths,
            ),
            product_home: paths.home().to_path_buf(),
            home: paths.session_home(),
            store: std::sync::Arc::new(crate::store::session_records::SqliteSessionStore::new(
                paths.clone(),
            )),
            session,
            cwd: dir.path().to_path_buf(),
            reason: "missing_defaults".into(),
            ready_catalog: plakous_host_protocol::views::ReadyCatalog::new(catalog),
        });
        tx.send(UserAction::Extension(SurfaceIntent::SaveSettings(
            crate::surface_protocol::SettingsUpdate {
                default_account: Some("codex".into()),
                default_model: Some("gpt-exact".into()),
                default_effort: Some("bespoke".into()),
                default_extra: None,
                composition: plakous_host_protocol::CompositionSelection::FollowCatalog,
                composition_options: vec![crate::surface_protocol::SettingsCompositionOption {
                    composition: plakous_host_protocol::CompositionSelection::FollowCatalog,
                    effective_presentation: "direct".into(),
                    unavailable_reason: None,
                }]
                .into_boxed_slice(),
                ..crate::surface_protocol::SettingsUpdate::default()
            },
        )))
        .unwrap();
        tx.send(UserAction::Extension(SurfaceIntent::SaveSettings(
            crate::surface_protocol::SettingsUpdate {
                default_effort: Some("medium".into()),
                default_account: Some("codex".into()),
                default_model: Some("gpt-unqualified".into()),
                default_extra: None,
                composition: plakous_host_protocol::CompositionSelection::FollowCatalog,
                composition_options: vec![crate::surface_protocol::SettingsCompositionOption {
                    composition: plakous_host_protocol::CompositionSelection::FollowCatalog,
                    effective_presentation: "direct".into(),
                    unavailable_reason: None,
                }]
                .into_boxed_slice(),
                ..crate::surface_protocol::SettingsUpdate::default()
            },
        )))
        .unwrap();
        drop(tx);
        join.join().unwrap().unwrap();
        let observed = rx.try_iter().collect::<Vec<_>>();
        assert!(observed.iter().any(|event| matches!(
            event,
            HostEvent::Extension(SurfaceObservation::SettingsApplied(_))
        )));
        assert!(observed.iter().any(|event| matches!(
            event,
            HostEvent::Extension(SurfaceObservation::SettingsSaveFailed(message)) if message.contains("Ready catalog")
        )));
        assert_eq!(
            observed
                .iter()
                .filter(|event| matches!(
                    event,
                    HostEvent::Extension(SurfaceObservation::SettingsSaveFailed(_))
                ))
                .count(),
            1
        );
        let config = ProductConfig::load(&paths).unwrap();
        assert_eq!(config.default_effort(), Some("bespoke"));
    }

    #[test]
    fn note_idle_only_names_the_live_intent() {
        let (events, rx) = host_event_sink();

        note_idle_only(&events, "fork");

        assert_eq!(
            rx.try_recv(),
            Ok(HostEvent::Row(TranscriptRow::info(
                NoticeScope::Session,
                "fork: available when idle (finish or interrupt the turn)"
            )))
        );
        note_idle_only(&events, "invoke_tool");
        assert_eq!(
            rx.try_recv(),
            Ok(HostEvent::Row(TranscriptRow::info(
                NoticeScope::Session,
                "invoke_tool: available when idle (finish or interrupt the turn)"
            )))
        );
        assert_ne!(
            rx.try_recv(),
            Ok(HostEvent::Row(TranscriptRow::info(
                NoticeScope::Session,
                "auth/binding: available when idle (finish or interrupt the turn)"
            )))
        );
    }

    /// A Settings save that changes only a view row must leave `[root]` and the
    /// template bodies exactly as the files authored them. Committing the
    /// unchanged tuple anyway ran the working-set mint and
    /// orphaned a file-authored root template.
    #[test]
    fn a_view_only_settings_save_leaves_the_root_template_alone() {
        let current = roster_shared_root_config();
        let mut requested = current.root_template().unwrap().clone();
        // The Settings tab paints the omitted file fields as their defaults.
        requested.composition = Some(plakous_hosted::binding::CompositionSelection::FollowCatalog);
        assert!(
            !settings_save_changes_root_binding(&current, &requested),
            "a theme-only save carries no identity change"
        );

        let mut config = current.clone();
        if settings_save_changes_root_binding(&current, &requested) {
            commit_root_template(&mut config, requested);
        }
        config.theme = crate::product_config::ProductTheme::CatppuccinMacchiato;
        assert_eq!(config.root_agent.as_deref(), Some("qa"));
        assert!(
            !config.templates.contains_key("default"),
            "no default template may be minted: {:?}",
            config.templates.keys().collect::<Vec<_>>()
        );
        assert_eq!(config.templates.len(), 1);
    }

    /// The same save with a real identity change still rewrites the root
    /// template, so the guard narrows the trigger without removing it.
    #[test]
    fn an_identity_settings_save_still_rewrites_the_root_template() {
        let current = roster_shared_root_config();
        let mut requested = current.root_template().unwrap().clone();
        requested.model = "gpt-5.3-codex-spark".to_owned();
        assert!(settings_save_changes_root_binding(&current, &requested));

        let mut config = current.clone();
        commit_root_template(&mut config, requested);
        assert_eq!(config.root_agent.as_deref(), Some("default"));
        assert_eq!(
            config
                .templates
                .get("qa")
                .map(|template| template.model.as_str()),
            Some("gpt-5.6-luna"),
            "the working-set template keeps its own construction"
        );
        assert_eq!(
            config
                .templates
                .get("default")
                .map(|template| template.model.as_str()),
            Some("gpt-5.3-codex-spark")
        );
    }
}
