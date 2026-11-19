use plakous::agent::TelemetryContext;
use plakous::model::{ModelError, ModelErrorKind, ModelHttpStatus, SafeDiagnostic, Timestamp};
use plakous_hosted::product_observation::wall_timestamp;

use super::{id_text, timestamp_ns};

const MAX_EXCERPT_BYTES: usize = 300;

/// Bounded diagnostics kept separately from the content-free model attempt.
#[derive(Debug, Eq, PartialEq)]
pub struct ModelFailureDiagnostic {
    context: TelemetryContext,
    at: Timestamp,
    kind: ModelErrorKind,
    http_status: Option<u16>,
    provider_code: Option<String>,
    excerpt: SafeDiagnostic,
}

impl ModelFailureDiagnostic {
    pub(crate) fn capture(context: TelemetryContext, error: &ModelError) -> Option<Self> {
        let source = error.context()?;
        let http_status = source.http_status().map(ModelHttpStatus::get);
        let provider_code = source.provider_code().map(str::to_owned);
        if error.kind() == ModelErrorKind::OutcomeUnknown
            || (http_status.is_none() && provider_code.is_none())
        {
            return None;
        }
        // Reuse the public diagnostic, not the transport source or response
        // body. Redact before cutting bytes so no partial token can survive.
        Some(Self {
            context,
            at: wall_timestamp(),
            kind: error.kind(),
            http_status,
            provider_code,
            excerpt: SafeDiagnostic::for_provider_excerpt(error.diagnostic().as_str()),
        })
    }

    fn excerpt(&self) -> &str {
        let text = self.excerpt.as_str();
        &text[..text.floor_char_boundary(MAX_EXCERPT_BYTES)]
    }

    pub(crate) fn log(&self) {
        tracing::warn!(
            target: "plak.model",
            session_id = %self.context.session_id().as_uuid(),
            attempt_id = ?self.context.attempt_id().map(plakous::model::AttemptId::as_uuid),
            kind = ?self.kind,
            http_status = self.http_status,
            provider_code = self.provider_code.as_deref(),
            excerpt = self.excerpt(),
            "model request failed",
        );
    }

    pub(super) fn write(&self, transaction: &rusqlite::Transaction<'_>) -> rusqlite::Result<()> {
        let detail = serde_json::json!({
            "attempt_id": self.context.attempt_id().map(id_text),
            "http_status": self.http_status,
            "provider_code": self.provider_code,
            "excerpt": self.excerpt(),
        });
        transaction.execute(
            "INSERT INTO product_events (at, kind, session_id, agent_id, status)
             VALUES (?1, 'model_attempt_failed', ?2, ?3, ?4)",
            rusqlite::params![
                timestamp_ns(self.at),
                id_text(self.context.session_id()),
                id_text(self.context.agent_id()),
                detail.to_string(),
            ],
        )?;
        Ok(())
    }
}
