//! The product half of the detached shell host.
//!
//! The loop itself is hosted: it consumes `UserAction`s, publishes
//! `HostEvent`s, and edits the durable queue through the same admission
//! helpers the attached loop uses. What is product is everything it needs
//! supplied to it -- the surfaces, the root-binding document, the native
//! subscription service, the OpenRouter credential -- plus the two
//! `ProductConfig` helpers the Settings save shares with it, and the
//! thread-spawning entry points the binary calls.

use std::sync::Arc;

use plakous::subscriptions::{NativeSubscriptions, SubscriptionPaths};

use crate::home::paths::PlakPaths;
use crate::product_config::ProductConfig;
use plakous_hosted::runtime::shell::run_shell_host_loop;
use plakous_hosted::runtime::{Sender, ShellFallbackFacts, ShellHostConfig, ShellHostRunner};

use crate::shell::settings::ProductSurface;
use crate::surface_protocol::PlakExtension;

/// The product's native-subscription service, found through its own state home.
///
/// Product construction resolves the state home and supplies the resulting
/// service to the hosted session.
pub fn open_product_subscriptions() -> Option<NativeSubscriptions> {
    let state_home = crate::home::paths::application_state_home().ok()?;
    let paths = SubscriptionPaths::from_xdg_state_home(state_home).ok()?;
    Some(NativeSubscriptions::new(paths))
}

/// Whether a Settings save carries an identity change for the root template.
///
/// The identity rows are account, model, effort, extra, composition, and
/// protocol (spec 02 §5.5). Everything else in the Settings registry is a view or
/// terminal-input row and owns nothing under `[root]` or the template files.
pub fn settings_save_changes_root_binding(
    current: &ProductConfig,
    requested: &crate::product_config::AgentTemplate,
) -> bool {
    match current.root_template() {
        // No root binding is configured: only a request that establishes one
        // may write. A save that also carries no binding leaves the file alone.
        None => !requested.account.is_empty() && !requested.model.is_empty(),
        // An omitted `composition` / `protocol` in the file is the same
        // construction as the default the Settings tab paints for it, so a
        // theme-only save on a file-authored template is not a change.
        Some(existing) => {
            let normalize = |template: &crate::product_config::AgentTemplate| {
                (
                    template.account.clone(),
                    template.model.clone(),
                    template.effort.clone(),
                    template.extra.clone(),
                    template.composition.unwrap_or_default(),
                    template.protocol.unwrap_or_default(),
                )
            };
            normalize(existing) != normalize(requested)
        }
    }
}

pub fn commit_root_template(
    config: &mut ProductConfig,
    template: crate::product_config::AgentTemplate,
) {
    if template.account.is_empty() || template.model.is_empty() {
        config.clear_root_binding();
        return;
    }
    *config.root_template_mut() = template;
}

/// Plak's shell host, as the hosted startup path sees it.
pub struct ProductShellHost {
    paths: PlakPaths,
}

impl ProductShellHost {
    /// The shell host of one product home.
    #[must_use]
    pub const fn new(paths: PlakPaths) -> Self {
        Self { paths }
    }

    /// A boxed handle for a [`plakous_hosted::runtime::TuiHostPreparation`].
    #[must_use]
    pub fn boxed(paths: &PlakPaths) -> Box<dyn ShellHostRunner<PlakExtension>> {
        Box::new(Self::new(paths.clone()))
    }
}

impl ShellHostRunner<PlakExtension> for ProductShellHost {
    fn fallback_facts(&self) -> ShellFallbackFacts<PlakExtension> {
        ShellFallbackFacts {
            surface: Some(ProductSurface::boxed(&self.paths)),
            binding: crate::shell::binding_document::ProductRootBindingDocument::shared(
                &self.paths,
            ),
            subscriptions: open_product_subscriptions(),
            openrouter_credentials: crate::openrouter_product::ProductOpenRouterCredentials::shared(
                &self.paths,
            ),
        }
    }

    fn run(
        self: Box<Self>,
        config: ShellHostConfig<PlakExtension>,
        actions: crate::surface_protocol::UserActionReceiver,
        events: Sender<PlakExtension>,
        clear_publication: Option<Arc<std::sync::Mutex<Option<String>>>>,
    ) -> crate::CliResult<()> {
        run_shell_host_loop(config, actions, events, clear_publication);
        Ok(())
    }
}

/// Spawn the non-agent shell host used for TUI-first auth empty state.
#[must_use]
pub fn spawn_shell_host(
    config: ShellHostConfig<PlakExtension>,
) -> (
    crate::surface_protocol::UserActionSender,
    crate::surface_protocol::HostEventReceiver,
    std::thread::JoinHandle<crate::CliResult<()>>,
) {
    spawn_shell_host_with_clear_publication(config, None)
}

/// The same, with the `/clear` publication cell the TUI reads back.
#[must_use]
pub fn spawn_shell_host_with_clear_publication(
    config: ShellHostConfig<PlakExtension>,
    clear_publication: Option<Arc<std::sync::Mutex<Option<String>>>>,
) -> (
    crate::surface_protocol::UserActionSender,
    crate::surface_protocol::HostEventReceiver,
    std::thread::JoinHandle<crate::CliResult<()>>,
) {
    let (action_tx, action_rx) = crate::surface_protocol::user_action_channel();
    let (event_tx, event_rx) = crate::surface_protocol::host_event_channel();
    let event_tx = Sender::over(event_tx);
    let handle = std::thread::spawn(move || {
        run_shell_host_loop(config, action_rx, event_tx, clear_publication);
        Ok(())
    });
    (action_tx, event_rx, handle)
}

/// State roots over Plak's own implementations of every product port, for
/// tests that want the real thing under a temporary product home.
///
/// The hosted side's own tests use `ProductStateRoots::with_doubles`; this is
/// the product's half of that split, and it is the only place the five
/// implementations are named together.
#[cfg(any(test, feature = "test-support"))]
#[allow(
    clippy::expect_used,
    reason = "a temporary product home is absolute by construction"
)]
#[must_use]
pub fn product_state_roots(paths: &PlakPaths) -> plakous_hosted::agent::ProductStateRoots {
    // The runtime store cannot create its parent: the temporary home's state
    // root exists before the roots bind it.
    std::fs::create_dir_all(paths.state_dir()).expect("test state root");
    plakous_hosted::agent::ProductStateRoots::new(
        paths.home().to_path_buf(),
        paths.runtime_database(),
        paths.home().join("test-state"),
        Arc::new(crate::telemetry_sink::ProductTelemetryFactory::new(
            paths.clone(),
        )),
        Arc::new(crate::store::tree::ProductTreeStoreFactory::new(
            paths.clone(),
        )),
        Arc::new(crate::roster_input::ProductRosterSource::new(Some(
            paths.clone(),
        ))),
        Arc::new(crate::sidecars::ProductSidecarLocator),
        crate::openrouter_product::ProductOpenRouterCredentials::shared(paths),
        crate::platform_product::ProductPlatformCredentials::shared(paths)
            .expect("valid API sources"),
        Arc::new(crate::store::session_records::SqliteSessionStore::new(
            paths.clone(),
        )),
    )
    .expect("temporary product home is absolute")
    .with_job_output_root(paths.job_outputs())
}
