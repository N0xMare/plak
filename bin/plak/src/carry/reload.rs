//! Reload probes available to both CLI queries and interactive handoff.

use super::document::IMPORTABLE_VERSIONS;
use crate::home::paths::PlakPaths;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The installed `plak` a reload would start, and the state key it answered;
/// it crosses from the session's host to the face that asked (spec 02 §5.1).
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReloadTarget {
    pub(crate) executable: PathBuf,
    pub(crate) capacity: Option<Box<CarryCapacityProbe>>,
    pub(crate) capacity_error: Option<String>,
    /// `None` when the build did not answer in time or with one key.
    pub(crate) state_key: Option<String>,
    pub(crate) probe: Option<ReloadProbe>,
    /// The destination's exact-reopen verdict for a same-key reload; `None`
    /// when it was not asked, did not answer in time, or predates the question.
    pub(crate) reopen: Option<Box<ReopenProbe>>,
    /// The session's own reload request this reload performs, if any.
    pub(crate) request: Option<Box<crate::reload_request::ReloadRequest>>,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CarryCapacityProbe {
    pub(crate) route: String,
    pub(crate) total_window: Option<u64>,
    pub(crate) admitted_input: Option<u64>,
    pub(crate) estimated_tokens: u64,
    pub(crate) messages: usize,
}

impl CarryCapacityProbe {
    pub(crate) fn warning(&self) -> Option<String> {
        (self.admitted_input.is_some_and(|input|self.estimated_tokens > input)).then(|| format!(
            "estimated history ~{} tokens exceeds destination route {} input window {} tokens (total window {}); the first prompt will refuse unless the history compacts",
            self.estimated_tokens,self.route,self.admitted_input.unwrap_or(0),
            self.total_window.map_or_else(|| "not reported".to_owned(), |n|format!("{n} tokens")),
        ))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq, schemars::JsonSchema)]
pub(crate) struct ReloadProbe {
    /// Whether the client accepts the host-only resume marker.
    #[serde(default)]
    pub(crate) host_resume: bool,
    pub(crate) state_key: String,
    pub(crate) export_versions: Vec<u32>,
    pub(crate) config_error: Option<String>,
    /// The answering build's binary id, when its install manifest or the
    /// home's identity cache already names it; absent when it is not known
    /// without hashing, and from builds that predate it.
    #[serde(default)]
    pub(crate) binary_id: Option<String>,
}

pub(crate) fn reload_probe(paths: &PlakPaths) -> ReloadProbe {
    ReloadProbe {
        host_resume: true,
        state_key: crate::home::state::state_key(),
        export_versions: IMPORTABLE_VERSIONS.to_vec(),
        config_error: crate::product_config::SessionPolicy::load(paths).err(),
        binary_id: crate::build_identity::known_binary_id(),
    }
}

/// The destination's answer to "would exact reopen of this session attach?",
/// asked by `/reload` before teardown (hidden `version --reopen-probe <id>`).
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq, schemars::JsonSchema)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub(crate) enum ReopenProbe {
    /// Exact reopen attaches.
    Opens,
    /// Exact reopen refuses because this build derives a different frozen
    /// binding input, named by `change`.
    BindingChanged { change: String },
    /// Exact reopen refuses for another reason (login, configuration,
    /// unsettled work, storage); never a carry trigger.
    Refused { reason: String },
    /// The destination could not answer without renewing a credential
    /// (`needs_refresh`); the source treats it as no verdict.
    Unknown { reason: String },
}

/// How far ahead of now a native access token must stay valid for the probe
/// to resolve its route: past every route's renewal lead and the source's
/// deadline, so resolution never renews.
const PROBE_TOKEN_MARGIN: std::time::Duration = std::time::Duration::from_secs(600);

/// Runs in the destination build with the real home: reads the session and
/// its projected head without writing, resolves the binding as an attach
/// does, and verifies the exact open without taking its lease.
pub(crate) fn reopen_probe(services: &crate::shell::services::Services, id: &str) -> ReopenProbe {
    let verdict = || -> Result<ReopenProbe, String> {
        let paths = services
            .paths()
            .map_err(|error| error.message().to_owned())?;
        let record = services
            .session_records()
            .map_err(|error| error.message().to_owned())?
            .load(id)?;
        let (_, recorded, _, views) =
            crate::store::ProductStore::inspect_projection_read_only(paths, &record.id)?;
        let recorded = recorded
            .or(record.plakous_locator)
            .ok_or_else(|| "the session has no runtime to reopen".to_owned())?;
        let head = views.as_ref().map_or(recorded, |views| {
            plakous_hosted::runtime::projected_head(recorded, views)
        });
        crate::commands::session::verify_exact_reopen(services, record, head)
            .map_err(|error| error.message().to_owned())
    };
    verdict().unwrap_or_else(|reason| ReopenProbe::Refused { reason })
}

impl From<plakous_hosted::runtime::ReopenVerdict> for ReopenProbe {
    fn from(verdict: plakous_hosted::runtime::ReopenVerdict) -> Self {
        match verdict {
            plakous_hosted::runtime::ReopenVerdict::Opens => Self::Opens,
            plakous_hosted::runtime::ReopenVerdict::BindingChanged {
                change,
                current_revision,
            } => Self::BindingChanged {
                change: binding_change_words(&change, current_revision.as_deref()),
            },
            plakous_hosted::runtime::ReopenVerdict::Refused(reason) => Self::Refused { reason },
            plakous_hosted::runtime::ReopenVerdict::Unknown(reason) => Self::Unknown { reason },
        }
    }
}

/// Names a binding change for the carry note. A capability binding that
/// differs only by digest is named by its category, since the recorded side
/// keeps only the digest; this build's adapter revision is added when known.
pub(super) fn binding_change_words(
    change: &plakous::agent::BindingChange,
    current_revision: Option<&str>,
) -> String {
    match (change, current_revision) {
        (plakous::agent::BindingChange::ModelCapabilities, Some(revision)) => format!(
            "model capability binding (adapter revision, declared features or context contract; this build: {revision})"
        ),
        _ => change.to_string(),
    }
}

/// The probe never renews a credential: the source kills it at a deadline,
/// and a renewal killed mid-exchange could leave a rotating credential
/// quarantined. A native login that is renewing, needs renewal or expires
/// within the margin answers `unknown`; an unusable one is refused with its
/// login remedy. Routes without a native login pass.
pub(crate) fn reopen_credential_gate(account: Option<&str>) -> Option<ReopenProbe> {
    let account = account?;
    let provider = plakous_hosted::capabilities::for_account(account)?.native_provider()?;
    let state_home = crate::home::paths::application_state_home().ok()?;
    let paths = plakous::subscriptions::SubscriptionPaths::from_xdg_state_home(state_home).ok()?;
    let subscriptions = plakous::subscriptions::NativeSubscriptions::new(paths);
    let executor = plakous::application::ApplicationExecutor::new().ok()?;
    let status = match executor.block_on(subscriptions.status(provider)) {
        Ok(Ok(status)) => Some(status.credential()),
        _ => None,
    };
    let expiring = || {
        executor
            .block_on(subscriptions.access_token_expires_within(provider, PROBE_TOKEN_MARGIN))
            .ok()
            .and_then(Result::ok)
    };
    credential_verdict(account, status, expiring)
}

/// The gate's decision from the credential's local state alone.
pub(super) fn credential_verdict(
    account: &str,
    status: Option<plakous::subscriptions::NativeAuthStatus>,
    expiring: impl FnOnce() -> Option<bool>,
) -> Option<ReopenProbe> {
    use plakous::subscriptions::NativeAuthStatus;
    let needs_refresh = || ReopenProbe::Unknown {
        reason: "needs_refresh".to_owned(),
    };
    let unusable = |state: &str| ReopenProbe::Refused {
        reason: format!("the {account} login is {state}; run `plak login {account}`, then /reload"),
    };
    match status {
        Some(NativeAuthStatus::Ready) => match expiring() {
            Some(false) => None,
            _ => Some(needs_refresh()),
        },
        Some(NativeAuthStatus::RefreshRequired | NativeAuthStatus::Refreshing) | None => {
            Some(needs_refresh())
        }
        Some(NativeAuthStatus::SignedOut) => Some(unusable("signed out")),
        Some(NativeAuthStatus::Quarantined) => Some(unusable("quarantined")),
        Some(NativeAuthStatus::Revoked) => Some(unusable("revoked")),
        Some(NativeAuthStatus::LoginRequired) => Some(unusable("stored by an earlier build")),
        Some(NativeAuthStatus::WrittenByNewerBuild) => Some(ReopenProbe::Refused {
            reason: format!(
                "the {account} login was written by a newer build; run that build, or replace the login with `plak login {account}`"
            ),
        }),
        Some(_) => Some(unusable("not usable by this build")),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;

impl From<ReloadTarget> for plakd_wire_types::ReloadTarget {
    fn from(value: ReloadTarget) -> Self {
        Self {
            executable: value.executable,
            capacity: value.capacity.map(|value| Box::new((*value).into())),
            capacity_error: value.capacity_error,
            state_key: value.state_key,
            probe: value.probe.map(Into::into),
            reopen: value.reopen.map(|value| Box::new((*value).into())),
            request: value.request.map(|value| Box::new((*value).into())),
        }
    }
}

impl From<plakd_wire_types::ReloadTarget> for ReloadTarget {
    fn from(value: plakd_wire_types::ReloadTarget) -> Self {
        Self {
            executable: value.executable,
            capacity: value.capacity.map(|value| Box::new((*value).into())),
            capacity_error: value.capacity_error,
            state_key: value.state_key,
            probe: value.probe.map(Into::into),
            reopen: value.reopen.map(|value| Box::new((*value).into())),
            request: value.request.map(|value| Box::new((*value).into())),
        }
    }
}

impl From<CarryCapacityProbe> for plakd_wire_types::CarryCapacityProbe {
    fn from(value: CarryCapacityProbe) -> Self {
        Self {
            route: value.route,
            total_window: value.total_window,
            admitted_input: value.admitted_input,
            estimated_tokens: value.estimated_tokens,
            messages: value.messages,
        }
    }
}

impl From<plakd_wire_types::CarryCapacityProbe> for CarryCapacityProbe {
    fn from(value: plakd_wire_types::CarryCapacityProbe) -> Self {
        Self {
            route: value.route,
            total_window: value.total_window,
            admitted_input: value.admitted_input,
            estimated_tokens: value.estimated_tokens,
            messages: value.messages,
        }
    }
}

impl From<ReloadProbe> for plakd_wire_types::ReloadProbe {
    fn from(value: ReloadProbe) -> Self {
        Self {
            state_key: value.state_key,
            export_versions: value.export_versions,
            config_error: value.config_error,
            binary_id: value.binary_id,
        }
    }
}

impl From<plakd_wire_types::ReloadProbe> for ReloadProbe {
    fn from(value: plakd_wire_types::ReloadProbe) -> Self {
        Self {
            state_key: value.state_key,
            export_versions: value.export_versions,
            config_error: value.config_error,
            binary_id: value.binary_id,
        }
    }
}

impl From<ReopenProbe> for plakd_wire_types::ReopenProbe {
    fn from(value: ReopenProbe) -> Self {
        match value {
            ReopenProbe::Opens => Self::Opens,
            ReopenProbe::BindingChanged { change } => Self::BindingChanged { change },
            ReopenProbe::Refused { reason } => Self::Refused { reason },
            ReopenProbe::Unknown { reason } => Self::Unknown { reason },
        }
    }
}

impl From<plakd_wire_types::ReopenProbe> for ReopenProbe {
    fn from(value: plakd_wire_types::ReopenProbe) -> Self {
        match value {
            plakd_wire_types::ReopenProbe::Opens => Self::Opens,
            plakd_wire_types::ReopenProbe::BindingChanged { change } => {
                Self::BindingChanged { change }
            }
            plakd_wire_types::ReopenProbe::Refused { reason } => Self::Refused { reason },
            plakd_wire_types::ReopenProbe::Unknown { reason } => Self::Unknown { reason },
        }
    }
}
