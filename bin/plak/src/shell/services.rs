//! The composition root's service handles, constructed once at CLI entry.
//!
//! `Services` is what a command receives instead of reaching back into the
//! environment: the resolved [`Home`], the shared product-record service, and
//! the one extra telemetry sink this
//! build attaches to every product state root. Commands borrow; nothing here
//! is re-derived per verb, per attach round, or per turn.

use std::sync::{Arc, OnceLock};

use crate::home::paths::PlakPaths;
use plakous_hosted::agent::ExtraTelemetrySink;

use crate::CliResult;
use crate::shell::home::Home;

/// How this build makes its extra telemetry sink, when it has one.
///
/// A plain `fn` pointer, not a value: constructing the OTLP bridge opens a
/// socket and spawns an exporter thread, and most verbs (`--version`,
/// `help`, `config *`, `doctor`) never emit telemetry at all.
pub(crate) type TelemetrySinkFactory = fn() -> Option<Arc<dyn ExtraTelemetrySink>>;

/// Handles every product command shares for the life of the process.
pub(crate) struct Services {
    home: Home,
    session_records: OnceLock<Arc<crate::store::session_records::SqliteSessionStore>>,
    telemetry_sink_factory: Option<TelemetrySinkFactory>,
    extra_telemetry_sink: OnceLock<Option<Arc<dyn ExtraTelemetrySink>>>,
}

impl Services {
    #[cfg(test)]
    pub(crate) fn for_home(paths: PlakPaths) -> Self {
        Self {
            home: Home::explicit(paths.clone(), paths.home().join("test-state")),
            session_records: OnceLock::new(),
            telemetry_sink_factory: None,
            extra_telemetry_sink: OnceLock::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn product_store_opens(&self) -> usize {
        self.session_records
            .get()
            .map_or(0, |store| store.opened_connections())
    }

    /// Resolves the home once and captures how this build makes its extra
    /// telemetry sink.
    ///
    /// The composition root passes the sink explicitly to every state root.
    /// It is built on first use rather than at CLI
    /// entry, so a command that opens no state root pays nothing for it.
    pub(crate) fn from_env(telemetry_sink_factory: Option<TelemetrySinkFactory>) -> Self {
        let home = Home::from_env();
        if let Ok(state) = home.state() {
            crate::product_doctor::install_credential_state_home(state.to_path_buf());
        }
        Self {
            home,
            session_records: OnceLock::new(),
            telemetry_sink_factory,
            extra_telemetry_sink: OnceLock::new(),
        }
    }

    pub(crate) const fn home(&self) -> &Home {
        &self.home
    }

    /// The product home layout, or the one resolution failure.
    pub(crate) fn paths(&self) -> CliResult<&PlakPaths> {
        self.home.paths()
    }

    /// The extra telemetry sink for a state root opened by this process,
    /// built on the first call and shared by every later one.
    pub(crate) fn extra_telemetry_sink(&self) -> Option<Arc<dyn ExtraTelemetrySink>> {
        self.extra_telemetry_sink
            .get_or_init(|| self.telemetry_sink_factory.and_then(|build| build()))
            .clone()
    }

    pub(crate) fn session_records(
        &self,
    ) -> CliResult<Arc<dyn crate::store::session_records::SessionRecordStore>> {
        let paths = self.paths()?;
        Ok(self
            .session_records
            .get_or_init(|| {
                Arc::new(crate::store::session_records::SqliteSessionStore::new(
                    paths.clone(),
                ))
            })
            .clone())
    }

    /// Product state roots under this home, with the extra sink attached.
    pub(crate) fn state_roots(&self) -> CliResult<plakous_hosted::agent::ProductStateRoots> {
        crate::commands::support::product_state_roots_from(
            self.paths()?.home().to_path_buf(),
            self.home.state()?.to_path_buf(),
            self.extra_telemetry_sink(),
            self.session_records()?,
        )
    }
}
