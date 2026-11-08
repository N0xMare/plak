//! Attachment-free, home-scoped product reads. No session store is opened here.

mod accounts;
mod clock;
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
}

pub(super) fn admission_limits() -> HostReadLimits {
    limits::admission()
}

impl Console {
    pub fn new(
        paths: PlakPaths,
        state: std::path::PathBuf,
        runtime: Option<tokio::runtime::Handle>,
    ) -> Arc<Self> {
        Arc::new(Self {
            paths,
            state,
            runtime,
            pages: Mutex::default(),
            provider: Arc::new(tokio::sync::Semaphore::new(limits::PROVIDER_READS)),
        })
    }

    pub fn welcome(principal: &HostPrincipal, limits: &HostReadLimits) -> HostWelcome {
        let socket = matches!(principal, HostPrincipal::Socket(_));
        HostWelcome {
            console: ConsoleWelcome {
                face: if socket {
                    ConsoleFace::Socket
                } else {
                    ConsoleFace::Web
                },
                accounts_read: CapabilityState {
                    enabled: socket,
                    disabled_reason: (!socket).then_some(DisabledReason::Configuration),
                    remedy: (!socket).then_some(CapabilityRemedy::EnableWebAccountsReadAndRestart),
                },
                config_read: CapabilityState {
                    enabled: false,
                    disabled_reason: Some(DisabledReason::NotCompiled),
                    remedy: Some(CapabilityRemedy::UseMatchingBuild),
                },
                configured_switches: ConfiguredSwitches {
                    web_accounts_read: false,
                    web_config_read: false,
                },
                limits: limits::welcome(limits),
            },
        }
    }

    pub fn request(
        self: &Arc<Self>,
        context: HostRequestContext,
        request: HostRequest,
    ) -> HostReadTask<PlakExtension> {
        if !matches!(context.principal, HostPrincipal::Socket(_)) {
            return Box::pin(async {
                Err(RequestRefusal::CapabilityDisabled {
                    capability: ConsoleCapability::AccountsRead,
                })
            });
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

    async fn read(
        self: Arc<Self>,
        context: HostRequestContext,
        request: HostRequest,
        cancelled: Arc<AtomicBool>,
    ) -> Result<HostResponse, RequestRefusal> {
        let deadline = Instant::now() + limits::admission().deadline;
        let selection = pages::Selection::from_request(&request);
        let (cursor, limit) = match &request {
            HostRequest::AccountsStatus { cursor, limit, .. }
            | HostRequest::AccountsUsage { cursor, limit, .. } => (cursor.as_deref(), *limit),
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
