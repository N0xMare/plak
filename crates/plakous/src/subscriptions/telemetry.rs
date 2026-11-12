use std::sync::{Arc, Mutex, OnceLock};

use plakous_agent::{
    Clock, CredentialProviderLabel, CredentialRefreshDetail, CredentialRefreshFailureClass,
    CredentialRefreshFailureReason, CredentialRefreshPhase, HttpStatusClass, TelemetryContext,
    TelemetryEmitter, TelemetryObservation, TelemetryObservationKind,
};
use plakous_auth::oauth_orchestrator::{
    CredentialRefreshEvent as AuthRefreshEvent, CredentialRefreshFailureClass as AuthFailureClass,
    CredentialRefreshFailureReason as AuthFailureReason, CredentialRefreshObserver,
    CredentialRefreshPhase as AuthRefreshPhase, HttpStatusClass as AuthHttpStatusClass,
};
use plakous_model::{SystemClock, Timestamp};

use super::NativeProvider;

static CREDENTIAL_TELEMETRY: OnceLock<Mutex<Option<Arc<TelemetryEmitter>>>> = OnceLock::new();

/// Installs the process-wide credential-lifecycle telemetry emitter.
///
/// Native manager construction looks this up so agent/TUI paths that open
/// telemetry before constructing [`super::NativeSubscriptions`] still record refreshes.
pub fn install_credential_telemetry(emitter: Arc<TelemetryEmitter>) {
    let slot = CREDENTIAL_TELEMETRY.get_or_init(|| Mutex::new(None));
    *slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(emitter);
}

pub(super) fn credential_refresh_observer(
    provider: NativeProvider,
    account: Option<String>,
) -> Option<Arc<dyn CredentialRefreshObserver>> {
    let emitter = CREDENTIAL_TELEMETRY
        .get()?
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()?;
    let label = CredentialProviderLabel::new(provider.as_str()).ok()?;
    Some(Arc::new(FacadeCredentialObserver {
        account: plakous_agent::CredentialAccountLabel::new(&account.unwrap_or_else(|| {
            if provider == NativeProvider::Xai {
                "grok".into()
            } else {
                provider.as_str().into()
            }
        }))
        .ok(),
        provider: label,
        emitter,
    }))
}

struct FacadeCredentialObserver {
    account: Option<plakous_agent::CredentialAccountLabel>,
    provider: CredentialProviderLabel,
    emitter: Arc<TelemetryEmitter>,
}

impl CredentialRefreshObserver for FacadeCredentialObserver {
    fn observe(&self, event: AuthRefreshEvent) {
        let timestamp = observation_timestamp();
        self.emitter.emit(TelemetryObservation::new(
            TelemetryContext::credential_lifecycle(),
            timestamp,
            TelemetryObservationKind::CredentialRefresh {
                provider: self.provider,
                phase: map_refresh_phase(event.phase),
                failure_class: event.failure_class.map(map_refresh_failure),
                transport_phase: event.transport_phase.map(map_transport_phase),
                http_status: event.http_status,
                elapsed: Some(event.elapsed),
                generation: event.generation,
                detail: CredentialRefreshDetail {
                    account: self.account,
                    reason: event.detail.reason.and_then(map_failure_reason),
                    expires_in_secs: event.detail.expires_in_secs,
                    id_token_iat_age_secs: event.detail.id_token_iat_age_secs,
                    id_token_exp_in_secs: event.detail.id_token_exp_in_secs,
                    oauth_error: plakous_agent::CredentialRefreshDiagnostic::new(
                        event.detail.oauth_error_code.as_deref(),
                        event.detail.oauth_error_description.as_deref(),
                    ),
                },
            },
        ));
    }
}

fn observation_timestamp() -> Timestamp {
    SystemClock.now()
}

fn map_refresh_phase(phase: AuthRefreshPhase) -> CredentialRefreshPhase {
    match phase {
        AuthRefreshPhase::Began => CredentialRefreshPhase::Began,
        AuthRefreshPhase::Completed => CredentialRefreshPhase::Completed,
        AuthRefreshPhase::Quarantined => CredentialRefreshPhase::Quarantined,
        AuthRefreshPhase::Restored => CredentialRefreshPhase::Restored,
        _ => CredentialRefreshPhase::OutcomeUnknown,
    }
}

fn map_refresh_failure(class: AuthFailureClass) -> CredentialRefreshFailureClass {
    match class {
        AuthFailureClass::Timeout => CredentialRefreshFailureClass::Timeout,
        AuthFailureClass::HttpStatus(status) => {
            CredentialRefreshFailureClass::HttpStatus(map_http_status_class(status))
        }
        AuthFailureClass::Validation => CredentialRefreshFailureClass::Validation,
        AuthFailureClass::Store => CredentialRefreshFailureClass::Store,
        AuthFailureClass::Cancelled => CredentialRefreshFailureClass::Cancelled,
        AuthFailureClass::NotReady => CredentialRefreshFailureClass::NotReady,
        _ => CredentialRefreshFailureClass::Transport,
    }
}

fn map_failure_reason(reason: AuthFailureReason) -> Option<CredentialRefreshFailureReason> {
    Some(match reason {
        AuthFailureReason::TokenShape => CredentialRefreshFailureReason::TokenShape,
        AuthFailureReason::ResponseTooLarge => CredentialRefreshFailureReason::ResponseTooLarge,
        AuthFailureReason::ExpiresIn => CredentialRefreshFailureReason::ExpiresIn,
        AuthFailureReason::AccessTokenExpired => CredentialRefreshFailureReason::AccessTokenExpired,
        AuthFailureReason::MissingRefreshToken => {
            CredentialRefreshFailureReason::MissingRefreshToken
        }
        AuthFailureReason::IdTokenHeader => CredentialRefreshFailureReason::IdTokenHeader,
        AuthFailureReason::IdTokenUnknownKid => CredentialRefreshFailureReason::IdTokenUnknownKid,
        AuthFailureReason::IdTokenSigningKey => CredentialRefreshFailureReason::IdTokenSigningKey,
        AuthFailureReason::IdTokenSignature => CredentialRefreshFailureReason::IdTokenSignature,
        AuthFailureReason::IdTokenIss => CredentialRefreshFailureReason::IdTokenIss,
        AuthFailureReason::IdTokenAud => CredentialRefreshFailureReason::IdTokenAud,
        AuthFailureReason::IdTokenExp => CredentialRefreshFailureReason::IdTokenExp,
        AuthFailureReason::IdTokenIat => CredentialRefreshFailureReason::IdTokenIat,
        AuthFailureReason::IdTokenNbf => CredentialRefreshFailureReason::IdTokenNbf,
        AuthFailureReason::IdTokenAzp => CredentialRefreshFailureReason::IdTokenAzp,
        AuthFailureReason::IdTokenSub => CredentialRefreshFailureReason::IdTokenSub,
        AuthFailureReason::IdTokenClaims => CredentialRefreshFailureReason::IdTokenClaims,
        AuthFailureReason::SubjectMismatch => CredentialRefreshFailureReason::SubjectMismatch,
        AuthFailureReason::SelectionChanged => CredentialRefreshFailureReason::SelectionChanged,
        AuthFailureReason::RetiredFormat => CredentialRefreshFailureReason::RetiredFormat,
        AuthFailureReason::StoredCredential => CredentialRefreshFailureReason::StoredCredential,
        AuthFailureReason::ProviderRejected => CredentialRefreshFailureReason::ProviderRejected,
        _ => return None,
    })
}

fn map_http_status_class(class: AuthHttpStatusClass) -> HttpStatusClass {
    match class {
        AuthHttpStatusClass::ClientError => HttpStatusClass::ClientError,
        AuthHttpStatusClass::ServerError => HttpStatusClass::ServerError,
        _ => HttpStatusClass::Other,
    }
}

fn map_transport_phase(phase: plakous_http::Phase) -> plakous_model::ModelRetryPhase {
    use plakous_http::Phase as H;
    use plakous_model::ModelRetryPhase as M;
    match phase {
        H::Resolution => M::Resolution,
        H::Connect => M::Connect,
        H::Tls => M::Tls,
        H::RequestWrite => M::RequestWrite,
        H::RequestWriteOrResponseHead => M::RequestWriteOrResponseHead,
        H::ResponseHead => M::ResponseHead,
        H::ResponseBody => M::ResponseBody,
        _ => M::Cleanup,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plakous_agent::{TelemetrySink, TelemetrySinkError};

    #[derive(Default)]
    struct Recorded(Mutex<Vec<TelemetryObservation>>);

    impl TelemetrySink for Recorded {
        fn try_emit(&self, observation: TelemetryObservation) -> Result<(), TelemetrySinkError> {
            self.0
                .lock()
                .map_err(|_| TelemetrySinkError::Closed)?
                .push(observation);
            Ok(())
        }
    }

    #[test]
    fn provider_error_body_is_filtered_before_telemetry_records_it()
    -> Result<(), Box<dyn std::error::Error>> {
        let sink = Arc::new(Recorded::default());
        let emitter = Arc::new(TelemetryEmitter::enabled(sink.clone()));
        for provider in [
            NativeProvider::Codex,
            NativeProvider::Xai,
            NativeProvider::Claude,
        ] {
            let mut detail = plakous_auth::oauth_orchestrator::CredentialRefreshDetail::default();
            detail.record_oauth_error(br#"{"error":"invalid_grant","error_description":"Grant revoked sk-private-fixture refresh_token=private-fixture eyJheader.payload.signature", "access_token":"private-body-field"}"#);
            FacadeCredentialObserver {
                account: Some(plakous_agent::CredentialAccountLabel::new("selected/work")?),
                provider: CredentialProviderLabel::new(provider.as_str())?,
                emitter: emitter.clone(),
            }
            .observe(AuthRefreshEvent {
                phase: AuthRefreshPhase::Quarantined,
                failure_class: Some(AuthFailureClass::HttpStatus(
                    AuthHttpStatusClass::ClientError,
                )),
                transport_phase: None,
                http_status: Some(400),
                elapsed: std::time::Duration::from_secs(1),
                generation: 7,
                detail,
            });
        }
        let rows = sink.0.lock().map_err(|_| "recording sink poisoned")?;
        assert_eq!(rows.len(), 3);
        for row in rows.iter() {
            let TelemetryObservationKind::CredentialRefresh {
                detail, generation, ..
            } = row.kind()
            else {
                return Err("unexpected telemetry kind".into());
            };
            assert_eq!(generation, 7);
            assert_eq!(
                detail
                    .account
                    .map(|account| account.as_str().to_owned())
                    .as_deref(),
                Some("selected/work")
            );
            assert_eq!(detail.oauth_error.code(), Some("invalid_grant"));
            let description = detail.oauth_error.description().unwrap_or("");
            assert!(description.starts_with("Grant revoked"));
            assert!(!description.contains("private"));
            assert!(!description.contains("eyJ"));
        }
        Ok(())
    }

    #[test]
    fn credential_refresh_reason_keeps_its_label_across_layers() {
        for reason in [
            AuthFailureReason::IdTokenExp,
            AuthFailureReason::RetiredFormat,
            AuthFailureReason::SubjectMismatch,
            AuthFailureReason::ProviderRejected,
        ] {
            assert_eq!(
                map_failure_reason(reason).map(CredentialRefreshFailureReason::as_str),
                Some(reason.as_str())
            );
        }
    }

    #[test]
    fn credential_refresh_local_dispatch_refusal_is_not_transport() {
        assert!(!matches!(
            map_refresh_failure(AuthFailureClass::NotReady),
            CredentialRefreshFailureClass::Transport
        ));
    }
}
