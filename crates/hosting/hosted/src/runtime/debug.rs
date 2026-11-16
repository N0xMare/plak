//! Nonblocking diagnostics never inspect queued observations or credential sources.

use super::{HostTargetAuthority, OpenRouterHostEvidence, Sender};

impl<P: plakous_host_protocol::ProtocolExtension> std::fmt::Debug for Sender<P> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Sender").finish_non_exhaustive()
    }
}

impl std::fmt::Debug for HostTargetAuthority {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Native { provider, .. } => formatter
                .debug_struct("Native")
                .field("provider", provider)
                .finish_non_exhaustive(),
            Self::Platform { provider, .. } => formatter
                .debug_struct("Platform")
                .field("provider", provider)
                .finish_non_exhaustive(),
            Self::OpenRouter { .. } => formatter.debug_struct("OpenRouter").finish_non_exhaustive(),
            Self::Registered { provider, .. } => formatter
                .debug_struct("Registered")
                .field("provider", provider)
                .finish_non_exhaustive(),
        }
    }
}

impl std::fmt::Debug for OpenRouterHostEvidence {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OpenRouterHostEvidence")
            .field("literal_model", &self.literal_model)
            .field("wire", &self.wire)
            .finish_non_exhaustive()
    }
}
