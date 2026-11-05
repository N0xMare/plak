//! A host's session policies: idle release and how a lost node continues.

use std::time::Duration;

/// How long a host keeps an idle session before releasing it, and the
/// attachments its residency counts.
#[derive(Clone, Debug)]
pub struct IdleRelease {
    /// How long every residency condition must stay clear.
    pub after: Duration,
    /// The session's host service attachments.
    pub attachments: crate::host_service::AttachmentCount,
}

/// How long a host keeps an idle session resident (spec 02 §4.4).
pub const IDLE_RELEASE: Duration = Duration::from_secs(300);

/// How a node whose host was lost continues (`[host] resume_after_loss`).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ResumeAfterLoss {
    /// Hold for the operator's Resume; the default.
    #[default]
    Hold,
    /// Continue each such node once by itself.
    Continue,
}
