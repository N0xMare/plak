//! Shared service-admission seals for Responses-family attempts.
//!
//! Dialects own public error types and Tower orchestration. This module owns
//! only the fail-closed seal ordering for ledgers (and optional lifecycle atoms).

use std::sync::Arc;
use std::sync::atomic::AtomicU8;

use super::admitted_await::ServiceAdmissionSeal;
use super::ledger::PhysicalRequestLedger;
use super::lifecycle::seal_service_attempt;

/// Seals a ledger and the pure attempt lifecycle atom together.
///
/// Used by xAI and OpenRouter: `close_service` then [`seal_service_attempt`].
pub struct LifecycleLedgerAdmission {
    lifecycle: Arc<AtomicU8>,
    ledger: Arc<PhysicalRequestLedger>,
    sealed: bool,
}

impl LifecycleLedgerAdmission {
    /// Creates an open admission guard for `lifecycle` and `ledger`.
    #[must_use]
    pub fn new(lifecycle: Arc<AtomicU8>, ledger: Arc<PhysicalRequestLedger>) -> Self {
        Self {
            lifecycle,
            ledger,
            sealed: false,
        }
    }
}

impl ServiceAdmissionSeal for LifecycleLedgerAdmission {
    fn seal(&mut self) {
        if !self.sealed {
            self.ledger.close_service();
            seal_service_attempt(&self.lifecycle);
            self.sealed = true;
        }
    }
}

impl Drop for LifecycleLedgerAdmission {
    fn drop(&mut self) {
        ServiceAdmissionSeal::seal(self);
    }
}

/// Seals OpenAI-style ledger-only service admission (no lifecycle atom).
///
/// Used when the dialect fences physical observations through
/// [`PhysicalRequestLedger`] alone (`LedgerPolicy::OPENAI_STRICT`).
pub struct LedgerOnlyAdmission {
    ledger: Arc<PhysicalRequestLedger>,
    sealed: bool,
}

impl LedgerOnlyAdmission {
    /// Creates an open admission guard for `ledger`.
    #[must_use]
    pub fn new(ledger: Arc<PhysicalRequestLedger>) -> Self {
        Self {
            ledger,
            sealed: false,
        }
    }
}

impl ServiceAdmissionSeal for LedgerOnlyAdmission {
    fn seal(&mut self) {
        if !self.sealed {
            self.ledger.close_service();
            self.sealed = true;
        }
    }
}

impl Drop for LedgerOnlyAdmission {
    fn drop(&mut self) {
        ServiceAdmissionSeal::seal(self);
    }
}
