//! Attachment-free, home-scoped product reads. No session store is opened here.

mod accounts;
mod clock;
mod definitions;
mod limits;
mod recorded;

use crate::home::paths::PlakPaths;
use plakd_wire_types::{
    PlakExtension,
    console::{
        CapabilityRemedy, CapabilityState, ConfiguredSwitches, ConsoleFace, ConsoleWelcome,
        Diagnostic, DisabledReason, HostRequest, HostResponse, HostWelcome, ReadAction,
        ReadRefusal, ReadRemedy, ReadScope, UnavailableCode, ValidationCode, ValidationIssue,
    },
};
use plakous_host_protocol::wire::{ConsoleCapability, RequestRefusal};
use plakous_hosted::host_service::wire::{
    HostPrincipal, HostReadLimits, HostReadTask, HostRequestContext,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

mod pages;
#[cfg(test)]
mod tests;
mod usage;
mod wire_size;

pub(super) struct Console {
    paths: PlakPaths,
    state: std::path::PathBuf,
    runtime: Option<tokio::runtime::Handle>,
    pages: Mutex<pages::Pages>,
    provider: Arc<tokio::sync::Semaphore>,
    definition_nonce: [u8; 16],
    switches: ConfiguredSwitches,
}

pub(super) fn admission_limits() -> HostReadLimits {
    limits::admission()
}

impl Console {
    pub fn new(
        paths: PlakPaths,
        state: std::path::PathBuf,
        runtime: Option<tokio::runtime::Handle>,
        switches: ConfiguredSwitches,
    ) -> Arc<Self> {
        Arc::new(Self {
            paths,
            state,
            runtime,
            pages: Mutex::default(),
            provider: Arc::new(tokio::sync::Semaphore::new(limits::PROVIDER_READS)),
            definition_nonce: *uuid::Uuid::new_v4().as_bytes(),
            switches,
        })
    }

    pub fn welcome(&self, principal: &HostPrincipal, limits: &HostReadLimits) -> HostWelcome {
        let socket = matches!(principal, HostPrincipal::Socket(_));
        let accounts = socket || self.switches.web_accounts_read;
        let config = socket || self.switches.web_config_read;
        HostWelcome {
            console: ConsoleWelcome {
                face: if socket {
                    ConsoleFace::Socket
                } else {
                    ConsoleFace::Web
                },
                accounts_read: CapabilityState {
                    enabled: accounts,
                    disabled_reason: (!accounts).then_some(DisabledReason::Configuration),
                    remedy: (!accounts)
                        .then_some(CapabilityRemedy::EnableWebAccountsReadAndRestart),
                },
                config_read: CapabilityState {
                    enabled: config,
                    disabled_reason: (!config).then_some(DisabledReason::Configuration),
                    remedy: (!config).then_some(CapabilityRemedy::EnableWebConfigReadAndRestart),
                },
                configured_switches: self.switches,
                limits: limits::welcome(limits),
            },
        }
    }

    pub fn request(
        self: &Arc<Self>,
        context: HostRequestContext,
        request: HostRequest,
    ) -> HostReadTask<PlakExtension> {
        let accounts = is_account_request(&request);
        let permitted = matches!(context.principal, HostPrincipal::Socket(_))
            || if accounts {
                self.switches.web_accounts_read
            } else {
                self.switches.web_config_read
            };
        if !permitted {
            let capability = if accounts {
                ConsoleCapability::AccountsRead
            } else {
                ConsoleCapability::ConfigRead
            };
            return Box::pin(async move { Err(RequestRefusal::CapabilityDisabled { capability }) });
        }
        let this = Arc::clone(self);
        let runtime = self.runtime.clone();
        Box::pin(async move {
            let Some(runtime) = runtime else {
                return Ok(refused(unavailable(
                    ReadScope::Accounts,
                    UnavailableCode::Storage,
                )));
            };
            let cancelled = Arc::new(AtomicBool::new(false));
            let cancel = Arc::clone(&cancelled);
            let task = runtime.spawn(async move { this.read(context, request, cancelled).await });
            let _cancel = Cancel {
                abort: task.abort_handle(),
                cancelled: cancel,
            };
            task.await.unwrap_or_else(|_| {
                Ok(refused(unavailable(
                    ReadScope::Accounts,
                    UnavailableCode::Storage,
                )))
            })
        })
    }

    pub(super) const fn switches(&self) -> ConfiguredSwitches {
        self.switches
    }

    async fn read(
        self: Arc<Self>,
        context: HostRequestContext,
        request: HostRequest,
        cancelled: Arc<AtomicBool>,
    ) -> Result<HostResponse, RequestRefusal> {
        if !is_account_request(&request) {
            return Ok(self
                .read_definitions(context, request)
                .await
                .unwrap_or_else(refused));
        }
        let deadline = Instant::now() + limits::admission().deadline;
        let selection = pages::Selection::from_request(&request);
        let (cursor, limit) = match &request {
            HostRequest::AccountsStatus { cursor, limit, .. }
            | HostRequest::AccountsUsage { cursor, limit, .. } => (cursor.as_deref(), *limit),
            _ => unreachable!("definition requests have their own bounded reader"),
        };
        let limit = match limit.unwrap_or(limits::PAGE_ROWS) {
            value if (1..=limits::PAGE_ROWS).contains(&value) => {
                usize::try_from(value).unwrap_or(100)
            }
            _ => return Ok(refused(validation("limit", ValidationCode::InvalidLimit))),
        };
        let include_telemetry = matches!(request, HostRequest::AccountsUsage { .. });
        let revision = match pages::source_revision(
            &self.paths,
            &self.state,
            selection.account_id.as_deref(),
            include_telemetry,
            deadline,
            &cancelled,
        )
        .await
        {
            Ok(revision) => revision,
            Err(reason) => return Ok(refused(reason)),
        };
        if let Some(cursor) = cursor {
            return Ok(self
                .pages
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .resume(cursor, &context, &selection, &revision, limit)
                .unwrap_or_else(refused));
        }
        let Some(as_of) = clock::utc(clock::now_ms()) else {
            return Ok(refused(unavailable(
                ReadScope::Accounts,
                UnavailableCode::Storage,
            )));
        };
        let response = match request {
            HostRequest::AccountsStatus { account_id, .. } => {
                self.status(account_id, as_of, deadline, &cancelled).await
            }
            HostRequest::AccountsUsage {
                account_id, window, ..
            } => {
                self.usage(account_id, window, as_of, deadline, Arc::clone(&cancelled))
                    .await
            }
            _ => unreachable!("definition requests have their own bounded reader"),
        };
        let response = match response {
            Ok(response) => response,
            Err(reason) => return Ok(refused(reason)),
        };
        let after = pages::source_revision(
            &self.paths,
            &self.state,
            selection.account_id.as_deref(),
            include_telemetry,
            deadline,
            &cancelled,
        )
        .await;
        if !matches!(after, Ok(after) if after == revision) {
            return Ok(refused(ReadRefusal::Busy {
                scope: ReadScope::Accounts,
                retry_after_ms: 1_000,
            }));
        }
        Ok(self
            .pages
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .start(response, context, selection, revision, limit)
            .unwrap_or_else(refused))
    }
}

fn is_account_request(request: &HostRequest) -> bool {
    matches!(
        request,
        HostRequest::AccountsStatus { .. } | HostRequest::AccountsUsage { .. }
    )
}

struct Cancel {
    abort: tokio::task::AbortHandle,
    cancelled: Arc<AtomicBool>,
}
impl Drop for Cancel {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        self.abort.abort();
    }
}

fn validation(field: &str, code: ValidationCode) -> ReadRefusal {
    ReadRefusal::ValidationFailed {
        issues: vec![ValidationIssue {
            field: field.to_owned(),
            code,
        }],
    }
}
fn unavailable(scope: ReadScope, cause: UnavailableCode) -> ReadRefusal {
    ReadRefusal::Unavailable {
        scope,
        cause,
        remedies: vec![ReadRemedy {
            action: ReadAction::Retry,
            account_id: None,
        }],
    }
}
fn diagnostic(code: &str, subject: Option<&str>, detail: &str) -> Diagnostic {
    Diagnostic {
        code: code.to_owned(),
        subject: subject.map(str::to_owned),
        detail: detail.to_owned(),
    }
}
fn refused(reason: ReadRefusal) -> HostResponse {
    HostResponse::Refused { reason }
}
