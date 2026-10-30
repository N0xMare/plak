//! Typed, non-transcript host lifecycle observations.

use crate::notice_remedy::NoticeRemedy;

/// Latest host phase, not a terminal result or a notice phase.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum HostPhase {
    #[default]
    /// The idle phase.
    Idle,
    /// The running phase.
    Running,
    /// The preparing phase.
    Preparing,
    /// The interrupting phase.
    Interrupting,
    /// The interrupted phase.
    Interrupted,
    /// The error phase.
    Error,
    /// The not ready phase.
    NotReady,
    /// The compacting phase.
    Compacting,
    /// The clear phase.
    Clear,
    /// The resume phase.
    Resume,
    /// The fork phase.
    Fork,
    /// The rebind phase.
    Rebind,
}

impl HostPhase {
    /// Stable neutral phase spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Running => "running",
            Self::Preparing => "preparing",
            Self::Interrupting => "interrupting",
            Self::Interrupted => "interrupted",
            Self::Error => "error",
            Self::NotReady => "not_ready",
            Self::Compacting => "compacting",
            Self::Clear => "clear",
            Self::Resume => "resume",
            Self::Fork => "fork",
            Self::Rebind => "rebind",
        }
    }
}

/// Independent tree/queue admission pauses.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HostPause {
    /// The restored tree requires explicit admission.
    pub tree: bool,
    /// Automatic queue drain requires explicit admission.
    pub queue: bool,
}

/// Bounded factual diagnostic; never used to route operator remedies.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiagnosticText(String);

impl DiagnosticText {
    /// Bound an already redaction-safe diagnostic at a UTF-8 boundary.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        let mut value = value.into();
        let mut end = value.len().min(4096);
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        value.truncate(end);
        Self(value)
    }
    /// Factual diagnostic bytes.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Typed attach/startup failure; composition retains its remedy separately from prose.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostFailure {
    /// Bounded, redaction-safe failure facts.
    pub diagnostic: DiagnosticText,
    /// Semantic repair selected at the failure source.
    pub remedy: Option<NoticeRemedy>,
}

/// Why no canonical agent is attached.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DetachedReason {
    /// No root profile has been selected.
    MissingDefaults,
    /// The root profile omitted mandatory reasoning effort.
    EffortRequired,
    /// Exact runtime construction refused.
    ResolveFailed(HostFailure),
    /// A credential's typed condition refused attachment.
    CredentialUnusable(HostFailure),
    /// Explicit recovery is required before attachment.
    RecoveryRequired(HostFailure),
    /// Another bounded factual condition, with no inferred remedy.
    Other(DiagnosticText),
}

/// Origin distinguishes the existing direct and queued status spellings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SteerSource {
    /// A direct steer action.
    Direct,
    /// A queued item delivered as a steer.
    QueuedDelivery,
}

/// One bounded control failure, never a frontend instruction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControlFailure {
    /// Redaction-safe runtime diagnostic.
    pub diagnostic: DiagnosticText,
}

/// Why the runtime has not been observed to settle a cancel request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CancelUnsettled {
    /// The runtime refused cancellation.
    RuntimeRefused {
        /// Typed cause.
        cause: ControlFailure,
    },
    /// The host's settlement deadline elapsed.
    Deadline {
        /// Typed timeout ms.
        timeout_ms: u64,
    },
}

impl std::fmt::Display for CancelUnsettled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RuntimeRefused { cause } => write!(
                f,
                "cancel was refused by the runtime ({}); the turn is still running",
                cause.diagnostic.as_str()
            ),
            Self::Deadline { timeout_ms } => write!(
                f,
                "cancel did not settle within {timeout_ms}ms; the turn is still running"
            ),
        }
    }
}

/// A stage in a clear or profile-rebind transition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostTransitionStage {
    /// The clear resolve authority stage.
    ClearResolveAuthority,
    /// The clear build candidate stage.
    ClearBuildCandidate,
    /// The clear publish candidate stage.
    ClearPublishCandidate,
    /// The clear activate stage.
    ClearActivate,
    /// The rebind resolve target stage.
    RebindResolveTarget,
    /// The rebind prepare runtime stage.
    RebindPrepareRuntime,
    /// The rebind publish intent stage.
    RebindPublishIntent,
    /// The rebind canonical transition stage.
    RebindCanonicalTransition,
    /// The rebind open target stage.
    RebindOpenTarget,
    /// The rebind activate stage.
    RebindActivate,
}

/// Typed status facts; absence of a detail remains distinct from every case here.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HostStatusDetail {
    /// Product settings location, supplied by the product settings adapter.
    ConfigurationPath {
        /// Configuration file path, not a command or remedy.
        path: String,
    },
    /// Releasing Previous Attachment.
    ReleasingPreviousAttachment,
    /// No Active Turn.
    NoActiveTurn,
    /// Queue Projection Catching Up.
    QueueProjectionCatchingUp,
    /// Creating Fresh Conversation.
    CreatingFreshConversation,
    /// Forking Session.
    ForkingSession,
    /// Prompt Preparation Stopped.
    PromptPreparationStopped,
    /// Recovering Interrupted Turn.
    RecoveringInterruptedTurn,
    /// Recovery Incomplete.
    RecoveryIncomplete,
    /// Prompt Not Admitted Queue Paused.
    PromptNotAdmittedQueuePaused,
    /// Recovery Pending.
    RecoveryPending,
    /// Turn Complete.
    TurnComplete,
    /// Prompt Not Admitted.
    PromptNotAdmitted,
    /// No Attached Turn To Interrupt.
    NoAttachedTurnToInterrupt,
    /// Queue Delivery Needs Attachment.
    QueueDeliveryNeedsAttachment,
    /// Reattach Pending.
    ReattachPending,
    /// Choose Profile.
    ChooseProfile,
    /// Interrupt Requested.
    InterruptRequested,
    /// Queued Follow Up.
    QueuedFollowUp,
    /// Ready on the exact model, with independent admission pauses.
    Ready {
        /// Typed model id.
        model_id: String,
        /// Typed pause.
        pause: HostPause,
    },
    /// The previous clear attempt left this model active.
    ClearRefused {
        /// Typed model id.
        model_id: String,
    },
    /// A profile transition targets this root model.
    ForkingProfileRoot {
        /// Typed model.
        model: String,
    },
    /// Rejoining the already admitted turn.
    RestoringAdmittedTurn {
        /// Typed pause.
        pause: HostPause,
    },
    /// One bounded external plan diagnostic.
    PlanDiagnostic {
        /// Typed diagnostic.
        diagnostic: DiagnosticText,
    },
    /// No canonical runtime is attached.
    Detached {
        /// Typed reason.
        reason: DetachedReason,
    },
    /// A prompt was refused because no runtime is attached.
    DetachedPromptRefused {
        /// Typed reason.
        reason: DetachedReason,
    },
    /// Elapsed duration since the transition began, in integer milliseconds.
    TransitionStage {
        /// Typed stage.
        stage: HostTransitionStage,
        /// Typed cumulative elapsed ms.
        cumulative_elapsed_ms: u64,
    },
    /// Startup/open failure with a separately typed repair.
    Failure {
        /// Typed failure.
        failure: HostFailure,
    },
    /// Cancellation has not settled.
    CancelUnsettled {
        /// Typed cause.
        cause: CancelUnsettled,
    },
    /// An admitted steer, retaining its source.
    Steering {
        /// Typed source.
        source: SteerSource,
    },
    /// A runtime refused a steer.
    SteerRejected {
        /// Typed cause.
        cause: ControlFailure,
    },
}
