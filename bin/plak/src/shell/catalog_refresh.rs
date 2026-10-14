//! The live catalog refresh the product schedules at host startup: every
//! Ready native vertical other than the attached one re-fetches its catalog
//! in the background and the refreshed provider snapshot is published to the
//! surface. Product policy, composed into the host through
//! `TuiHostPreparation::catalog_refresh`.

use std::thread;
use std::time::Instant;

use crate::home::paths::PlakPaths;
use crate::surface_protocol::{HostEvent, HostEventSender, PlakExtension};
use plakous::application::ApplicationExecutor;
use plakous::subscriptions::{
    CompatibilityStatus, NativeAuthStatus, NativeProvider, NativeSubscriptions, SubscriptionPaths,
};
use plakous_hosted::runtime::{TuiHostMode, oauth_provider_for_account};
use plakous_hosted::session_log::{SessionEvent, SessionLog};

/// The refresh hook for one prepared host mode: only an attached mode
/// excludes its selected provider. Logs under the record only once published.
pub(crate) fn hook(
    paths: PlakPaths,
    mode: &TuiHostMode<PlakExtension>,
) -> Box<dyn FnOnce(HostEventSender) + Send> {
    let (session_id, account) = refresh_context(mode);
    Box::new(move |events| spawn(paths, session_id, account, events))
}

fn refresh_context(mode: &TuiHostMode<PlakExtension>) -> (Option<String>, Option<String>) {
    let (record, attached_account) = match mode {
        TuiHostMode::Attached(config) => (&config.session, config.session.account.clone()),
        TuiHostMode::Shell(config) => (&config.session, None),
    };
    let session_id = (record.store_revision != 0).then(|| record.id.clone());
    (session_id, attached_account)
}

fn refresh_providers(attached_account: Option<&str>) -> impl Iterator<Item = NativeProvider> {
    let attached_provider = attached_account.and_then(oauth_provider_for_account);
    // Only an attached host already resolved its catalog. A selected account
    // in a fallback shell may be precisely the provider whose catalog is cold.
    [
        NativeProvider::Codex,
        NativeProvider::Xai,
        NativeProvider::Claude,
    ]
    .into_iter()
    .filter(move |provider| Some(*provider) != attached_provider)
}

fn spawn(
    paths: PlakPaths,
    session_id: Option<String>,
    attached_account: Option<String>,
    events: HostEventSender,
) {
    thread::spawn(move || {
        let refresh_started = Instant::now();
        let Ok(state_home) = crate::home::paths::application_state_home() else {
            return;
        };
        let Ok(sub_paths) = SubscriptionPaths::from_xdg_state_home(state_home) else {
            return;
        };
        let subscriptions = NativeSubscriptions::new(sub_paths);
        let Ok(executor) = ApplicationExecutor::new() else {
            return;
        };
        // Do not overlap an attached SuperGrok inference with a catalog GET.
        for provider in refresh_providers(attached_account.as_deref()) {
            let Ok(Ok(status)) = executor.block_on(subscriptions.status(provider)) else {
                continue;
            };
            if status.compatibility() != CompatibilityStatus::Current
                || status.credential() != NativeAuthStatus::Ready
            {
                continue;
            }
            // Stale snapshots must not suppress a background refresh.
            let _ = executor.block_on(subscriptions.model_catalog(provider));
        }
        if let Some(session_id) = session_id.as_deref()
            && let Ok(log) = SessionLog::open(&paths.session_home(), session_id)
        {
            let mut event = SessionEvent::new("catalog.refresh").u64(
                "elapsed_ms",
                u64::try_from(refresh_started.elapsed().as_millis()).unwrap_or(u64::MAX),
            );
            if let Some(account) = attached_account.as_deref() {
                event = event.str("account", account);
            }
            let _ = log.append_for(session_id, event);
        }
        if let Ok((providers, _)) = crate::presentation::tui_presentation_values(&paths) {
            let _ = events.send(HostEvent::ProviderSnapshot { providers });
        }
    });
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "unit tests")]

    use super::*;
    use crate::composition_tests::product_injected_attached_host;
    use plakous_hosted::runtime::ShellHostConfig;

    #[test]
    fn cold_shell_refreshes_selected_provider_while_attached_host_excludes_it() {
        for (account, provider) in [
            ("codex", NativeProvider::Codex),
            ("chatgpt", NativeProvider::Codex),
            ("grok", NativeProvider::Xai),
            ("supergrok", NativeProvider::Xai),
            ("xai-oauth", NativeProvider::Xai),
            ("claude", NativeProvider::Claude),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let (mut config, _, paths) = product_injected_attached_host(dir.path());
            config.session.account = Some(account.into());
            crate::store::sessions::save_session(&paths, &mut config.session).unwrap();
            let expected_id = config.session.id.clone();
            let mode = TuiHostMode::Attached(config);
            let (session_id, account) = refresh_context(&mode);
            assert_eq!(session_id.as_deref(), Some(expected_id.as_str()));
            let providers = refresh_providers(account.as_deref()).collect::<Vec<_>>();
            assert_eq!(providers.len(), 2);
            assert!(!providers.contains(&provider));

            let TuiHostMode::Attached(config) = mode else {
                unreachable!();
            };
            // The same persisted selection falls back because its ReadyCatalog
            // is empty. Its account is a selection, not an attached provider.
            let mode = TuiHostMode::Shell(ShellHostConfig {
                product_home: config.product_home,
                binding: crate::shell::binding_document::ProductRootBindingDocument::shared(&paths),
                subscriptions: None,
                openrouter_credentials:
                    crate::openrouter_product::ProductOpenRouterCredentials::shared(&paths),
                home: config.home,
                store: config.store,
                session: config.session,
                cwd: config.cwd,
                reason: "resolve_failed: selected model is absent from the discovered ReadyCatalog"
                    .into(),
                ready_catalog: plakous_host_protocol::views::ReadyCatalog::default(),
                surface: config.surface,
            });
            let (session_id, account) = refresh_context(&mode);
            assert_eq!(session_id.as_deref(), Some(expected_id.as_str()));
            assert_eq!(account, None);
            assert_eq!(
                refresh_providers(account.as_deref()).collect::<Vec<_>>(),
                [
                    NativeProvider::Codex,
                    NativeProvider::Xai,
                    NativeProvider::Claude
                ]
            );
        }
    }

    #[test]
    fn unpublished_session_does_not_acquire_catalog_log_ownership() {
        let dir = tempfile::tempdir().unwrap();
        let (config, _, _) = product_injected_attached_host(dir.path());
        assert_eq!(config.session.store_revision, 0);
        let (session_id, account) = refresh_context(&TuiHostMode::Attached(config));
        assert_eq!(session_id, None);
        assert_eq!(account.as_deref(), Some("codex"));
    }
}
