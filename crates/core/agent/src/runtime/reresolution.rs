//! Fresh exact re-resolution of a model whose admitted evidence window closed.
//!
//! The runtime knows nothing of providers. A host that resolved the model
//! against windowed evidence installs a [`ModelReresolver`] with the window's
//! end; between model operations past that end the driver asks it for a fresh
//! resolution of the same exact model and swaps it in only when every
//! durable axis is unchanged.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use plakous_model::{
    CompiledProviderConfiguration, ConfiguredLanguageModel, Digest, LanguageModel,
    RuntimeModelAuthority, Timestamp,
};

/// One fresh exact resolution of the model an agent already runs.
pub struct ReresolvedModel {
    configured: ConfiguredLanguageModel,
    admitted_until: Timestamp,
}

impl ReresolvedModel {
    /// Pairs a freshly resolved model with the end of its admitted evidence
    /// window.
    #[must_use]
    pub const fn new(configured: ConfiguredLanguageModel, admitted_until: Timestamp) -> Self {
        Self {
            configured,
            admitted_until,
        }
    }
}

impl fmt::Debug for ReresolvedModel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReresolvedModel")
            .field(
                "effective_configuration",
                &self.configured.configuration().effective_identity(),
            )
            .field("admitted_until", &self.admitted_until)
            .finish_non_exhaustive()
    }
}

/// A refused re-resolution. The message is redaction-safe and names the
/// remedy; it is the turn's terminal error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelReresolutionError {
    message: String,
}

impl ModelReresolutionError {
    /// Creates a refusal whose message names the cause and the remedy.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// Returns the redaction-safe message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ModelReresolutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ModelReresolutionError {}

/// Future returned by [`ModelReresolver::reresolve`].
pub type ModelReresolutionOperation =
    Pin<Box<dyn Future<Output = Result<ReresolvedModel, ModelReresolutionError>> + Send>>;

/// Host-owned source of a fresh exact resolution of the same model.
///
/// The runtime calls it only between model operations, never with a prepared
/// call outstanding, and only once the current resolution's admitted window
/// has closed. `expired_until` is the end of that closed window: an
/// implementation shared by several agents returns a resolution another caller
/// already installed when its window ends later, so concurrent callers share
/// one resolution.
pub trait ModelReresolver: Send + Sync {
    /// Resolves the same exact model afresh.
    fn reresolve(self: Arc<Self>, expired_until: Timestamp) -> ModelReresolutionOperation;

    /// Reports that the runtime swapped in the resolution whose window ends at
    /// `admitted_until`. A refused or failed resolution is never reported, so
    /// an implementation that shares resolutions installs one only after a
    /// runtime accepted it.
    fn accepted(&self, admitted_until: Timestamp) {
        let _ = admitted_until;
    }
}

/// The driver's re-resolution state: the hook and the axes a fresh
/// resolution must keep.
#[derive(Clone)]
pub(super) struct ModelRebind {
    pub(super) reresolver: Arc<dyn ModelReresolver>,
    pub(super) admitted_until: Timestamp,
    account: Digest,
    credential_authority: Digest,
    generation: u64,
}

impl ModelRebind {
    pub(super) fn new(
        reresolver: Arc<dyn ModelReresolver>,
        admitted_until: Timestamp,
        authority: &RuntimeModelAuthority,
    ) -> Self {
        Self {
            reresolver,
            admitted_until,
            account: authority.account_binding(),
            credential_authority: authority.credential_authority_binding(),
            generation: authority.credential_generation(),
        }
    }

    /// Admits a fresh resolution only when the selected and effective
    /// configuration identities, account, credential authority and window are
    /// unchanged or advanced, and its own authority is current. Returns the
    /// model and configuration to swap in.
    pub(super) fn admit(
        &mut self,
        current: &CompiledProviderConfiguration,
        fresh: ReresolvedModel,
        checked_at: Timestamp,
    ) -> Result<(Arc<dyn LanguageModel>, Arc<CompiledProviderConfiguration>), &'static str> {
        let ReresolvedModel {
            configured,
            admitted_until,
        } = fresh;
        let configuration = configured.configuration();
        let authority = configured.initial_authority();
        if configuration.selected_identity() != current.selected_identity()
            || configuration.effective_identity() != current.effective_identity()
        {
            return Err("fresh resolution changed the model configuration");
        }
        if authority.account_binding() != self.account
            || authority.credential_authority_binding() != self.credential_authority
            || authority.credential_generation() < self.generation
        {
            return Err("fresh resolution changed the account or credential authority");
        }
        if admitted_until <= checked_at {
            return Err("fresh resolution is already outside its admitted window");
        }
        super::admission::validate_configuration_runtime_authority(
            configuration,
            configuration.effective().descriptor(),
            authority,
            checked_at,
        )
        .map_err(|_| "fresh resolution carries no current authority")?;
        self.admitted_until = admitted_until;
        self.generation = authority.credential_generation();
        Ok((configured.model(), Arc::new(configuration.clone())))
    }
}
