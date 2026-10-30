use super::{
    ContextAdmissionError, ContextAdmissionRequirement, ContextAnchor, ContextCapacity,
    ContextContractError, ContextEvidenceClass, ContextHeadroom, ContextHistoryPlan,
    ContextInputParts, ContextMeasurement, ContextNonGuaranteeDisposition,
    ContextPlannerDisposition, ContextPlannerDispositionKind, ContextRequestAnchor, ContextScope,
    ContextStatusKind, ContextUnavailable, ContextWireInput, CountBound, Digest, DigestDomain,
    EvaluationFailure, ModelRequest, OutputReservation, Serialize, context_admission_margin,
    context_occupancy_tokens, evaluate_rule, semantic_digest,
};

/// Inspectable result of complete-projection context evaluation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ContextStatus {
    descriptor_binding: Digest,
    model_input_identity: Digest,
    scope: Option<ContextScope>,
    capacity: Option<ContextCapacity>,
    measurement: Option<ContextMeasurement>,
    reservation: Option<OutputReservation>,
    planner: Option<ContextPlannerDisposition>,
    kind: ContextStatusKind,
    identity: Digest,
    /// Byte composition of the measured body, when the preparing provider
    /// attached one. Observational and presentation-only: it is outside the
    /// status identity and grants no admission authority.
    input_parts: Option<ContextInputParts>,
    /// The provider count the occupancy is anchored on, when the measuring
    /// runtime has one. Presentation-only like `input_parts`.
    context_anchor: Option<ContextAnchor>,
    wire_input: Option<ContextWireInput>,
    /// Freshness-insensitive binding used by item-delta admission, not the
    /// exact descriptor binding in the persisted status identity.
    #[serde(skip)]
    wire_binding: Option<Digest>,
    request_anchor: Option<ContextRequestAnchor>,
    unanchored_estimate: bool,
    local_input_upper_bound: Option<u64>,
    semantic_summary_bound: u64,
}

#[derive(Serialize)]
struct ContextStatusContent<'a> {
    descriptor_binding: Digest,
    model_input_identity: Digest,
    scope: Option<Digest>,
    capacity: Option<Digest>,
    non_guarantee_disposition: Option<ContextNonGuaranteeDisposition>,
    measurement: Option<Digest>,
    reservation: Option<Digest>,
    planner: Option<Digest>,
    kind: &'a ContextStatusKind,
}

impl ContextStatus {
    pub(crate) fn unavailable(
        descriptor_binding: Digest,
        model_input_identity: Digest,
        scope: Option<ContextScope>,
        capacity: Option<ContextCapacity>,
        measurement: Option<ContextMeasurement>,
        reservation: Option<OutputReservation>,
        unavailable: ContextUnavailable,
    ) -> Result<Self, ContextContractError> {
        Self::new(
            descriptor_binding,
            model_input_identity,
            scope,
            capacity,
            measurement,
            reservation,
            None,
            ContextStatusKind::Unavailable(unavailable),
        )
    }

    pub(crate) fn evaluate(
        capacity: ContextCapacity,
        measurement: ContextMeasurement,
        reservation: Option<OutputReservation>,
        request: &ModelRequest,
    ) -> Result<Self, ContextContractError> {
        let scope = capacity.scope().clone();
        let descriptor_binding = scope.descriptor_binding();
        let model_input_identity = scope.model_input_identity();
        let mut planner = None;
        let kind = if request.model_input_identity() != model_input_identity {
            ContextStatusKind::Unavailable(ContextUnavailable::ScopeDrift {
                field: "planner model input",
            })
        } else if measurement.scope().identity() != scope.identity()
            || reservation
                .as_ref()
                .is_some_and(|reservation| reservation.scope_identity() != scope.identity())
        {
            ContextStatusKind::Unavailable(ContextUnavailable::ScopeDrift {
                field: "context fact",
            })
        } else {
            match ContextHistoryPlan::new(request.input()) {
                Err(_) => {
                    ContextStatusKind::Unavailable(ContextUnavailable::HistoryPlanningUnavailable)
                }
                Ok(history_plan) => {
                    match evaluate_rule(
                        capacity.rule(),
                        measurement.bound(),
                        reservation.as_ref().map(OutputReservation::tokens),
                    ) {
                        Ok(headroom) => ContextStatusKind::Fits { headroom },
                        Err(EvaluationFailure::Overflow(overflow)) => {
                            let disposition = history_plan.disposition().clone();
                            let kind = match disposition.kind() {
                                ContextPlannerDispositionKind::EligibleWholeGroups
                                | ContextPlannerDispositionKind::EligibleActiveTurnPrefix => {
                                    ContextStatusKind::WouldOverflow(overflow)
                                }
                                ContextPlannerDispositionKind::ProtectedOnly => {
                                    ContextStatusKind::ProtectedOverflow(overflow)
                                }
                            };
                            planner = Some(disposition);
                            kind
                        }
                        Err(EvaluationFailure::ArithmeticOverflow) => {
                            ContextStatusKind::Unavailable(ContextUnavailable::ArithmeticOverflow)
                        }
                    }
                }
            }
        };
        Self::new(
            descriptor_binding,
            model_input_identity,
            Some(scope),
            Some(capacity),
            Some(measurement),
            reservation,
            planner,
            kind,
        )
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "status retains every authority-bearing evaluation input"
    )]
    fn new(
        descriptor_binding: Digest,
        model_input_identity: Digest,
        scope: Option<ContextScope>,
        capacity: Option<ContextCapacity>,
        measurement: Option<ContextMeasurement>,
        reservation: Option<OutputReservation>,
        planner: Option<ContextPlannerDisposition>,
        kind: ContextStatusKind,
    ) -> Result<Self, ContextContractError> {
        let content = ContextStatusContent {
            descriptor_binding,
            model_input_identity,
            scope: scope.as_ref().map(ContextScope::identity),
            capacity: capacity.as_ref().map(ContextCapacity::identity),
            non_guarantee_disposition: capacity
                .as_ref()
                .and_then(ContextCapacity::non_guarantee_disposition),
            measurement: measurement.as_ref().map(ContextMeasurement::identity),
            reservation: reservation.as_ref().map(OutputReservation::identity),
            planner: planner.as_ref().map(ContextPlannerDisposition::identity),
            kind: &kind,
        };
        let identity = semantic_digest(DigestDomain::ContextAdmission, 3, &content)?;
        Ok(Self {
            descriptor_binding,
            model_input_identity,
            scope,
            capacity,
            measurement,
            reservation,
            planner,
            kind,
            identity,
            input_parts: None,
            context_anchor: None,
            wire_input: None,
            wire_binding: None,
            request_anchor: None,
            unanchored_estimate: false,
            local_input_upper_bound: None,
            semantic_summary_bound: u64::from(crate::SEMANTIC_COMPACTION_SUMMARY_TOKENS),
        })
    }

    pub(crate) const fn with_semantic_summary_bound(mut self, bound: u64) -> Self {
        self.semantic_summary_bound = bound;
        self
    }

    /// Remaining conservative input headroom after a matching completed response.
    #[must_use]
    pub fn completed_response_headroom(
        &self,
        anchor: &ContextRequestAnchor,
    ) -> Option<ContextHeadroom> {
        if Some(anchor.binding) != self.wire_binding
            || !self.scope.as_ref().is_some_and(|scope| {
                anchor.route == *scope.route() && scope.serves_same_model(&anchor.model)
            })
        {
            return None;
        }
        let capacity = self.capacity.as_ref()?;
        let bound = anchor
            .usage
            .input_tokens()
            .saturating_add(anchor.usage.output_tokens())
            .saturating_add(context_admission_margin(capacity.rule().input_ceiling()));
        evaluate_rule(
            capacity.rule(),
            CountBound::CustomEnvelopeUpperBound {
                tokens: bound,
                declaration: self.identity,
            },
            self.reservation.as_ref().map(OutputReservation::tokens),
        )
        .ok()
    }

    /// Shared hosted/core compaction trigger, using this request's stable prefix.
    #[must_use]
    pub fn compaction_trigger_tokens(&self) -> Option<u64> {
        let stable = self.input_parts.map_or(0, |parts| {
            parts
                .system_bytes()
                .div_ceil(4)
                .saturating_add(parts.tool_definition_bytes().div_ceil(4))
        });
        Some(
            self.capacity.as_ref()?.rule().compaction_trigger_tokens(
                self.reservation
                    .as_ref()
                    .map_or(0, OutputReservation::tokens),
                stable,
                self.semantic_summary_bound,
            ),
        )
    }

    pub(crate) fn evaluate_wire_input(
        mut self,
        request: &ModelRequest,
        input: &ContextWireInput,
        anchor: Option<&ContextRequestAnchor>,
        binding: Digest,
        require_bound: bool,
    ) -> Result<Self, ContextContractError> {
        self.semantic_summary_bound = u64::from(
            crate::semantic_compaction_output_bound(
                request.limits().max_output_tokens(),
                request.reasoning(),
            )
            .get(),
        );
        let Some(capacity) = self.capacity.as_ref() else {
            return Ok(self);
        };
        if matches!(self.kind, ContextStatusKind::Unavailable(_)) {
            return Ok(self);
        }
        self.wire_binding = Some(binding);
        let anchor = anchor.filter(|anchor| {
            anchor.binding == binding
                && self.scope.as_ref().is_some_and(|scope| {
                    &anchor.route == scope.route() && scope.serves_same_model(&anchor.model)
                })
        });
        let ceiling = capacity.rule().input_ceiling();
        let margin = context_admission_margin(ceiling);
        // A reviewed reference-client envelope already measures this complete
        // prepared input. Replacing that certificate with one token per wire
        // byte can refuse every compaction candidate after replay is rendered
        // fresh and its old item digests no longer match the usage anchor.
        // Image transport bytes are not visual-token evidence in either path.
        let whole_input_bound = match self.measurement.as_ref().map(ContextMeasurement::bound) {
            Some(CountBound::ReferenceClientEnvelopeUpperBound { tokens, .. }) => input
                .upper_bound()
                .min(tokens.saturating_add(input.image_tokens())),
            _ => input.upper_bound(),
        };
        let bounded = anchor
            .map_or_else(
                || whole_input_bound,
                |anchor| anchor.occupied_tokens(input).min(whole_input_bound),
            )
            .saturating_add(margin);
        let reserved = self.reservation.as_ref().map(OutputReservation::tokens);
        let mut evaluation = evaluate_rule(
            capacity.rule(),
            CountBound::CustomEnvelopeUpperBound {
                tokens: bounded,
                declaration: self.identity,
            },
            reserved,
        );
        if anchor.is_none()
            && !require_bound
            && matches!(evaluation, Err(EvaluationFailure::Overflow(_)))
        {
            // This is explicitly not a guarantee: a plausible first request is
            // decided by the current provider, with typed overflow recovery.
            evaluation = evaluate_rule(
                capacity.rule(),
                CountBound::CustomEnvelopeUpperBound {
                    tokens: input.estimate_tokens(),
                    declaration: self.identity,
                },
                reserved,
            );
            self.unanchored_estimate = evaluation.is_ok();
        }
        self.local_input_upper_bound = (!self.unanchored_estimate).then_some(bounded);
        self.kind = match evaluation {
            Ok(headroom) => ContextStatusKind::Fits { headroom },
            Err(EvaluationFailure::ArithmeticOverflow) => {
                ContextStatusKind::Unavailable(ContextUnavailable::ArithmeticOverflow)
            }
            Err(EvaluationFailure::Overflow(overflow)) => {
                let plan = ContextHistoryPlan::new(request.input()).map_err(|_| {
                    ContextContractError::Serialization {
                        reason: "history planning unavailable".to_owned(),
                    }
                })?;
                let disposition = plan.disposition().clone();
                let protected = matches!(
                    disposition.kind(),
                    ContextPlannerDispositionKind::ProtectedOnly
                );
                self.planner = Some(disposition);
                if protected {
                    ContextStatusKind::ProtectedOverflow(overflow)
                } else {
                    ContextStatusKind::WouldOverflow(overflow)
                }
            }
        };
        self.wire_input = Some(input.clone());
        self.request_anchor = anchor.cloned();
        self.identity = semantic_digest(
            DigestDomain::ContextAdmission,
            5,
            &(
                self.identity,
                &self.wire_input,
                &self.request_anchor,
                self.unanchored_estimate,
                &self.kind,
            ),
        )?;
        Ok(self)
    }

    /// The selected conservative input bound, including the local margin.
    /// Absent when a non-guaranteed estimate delegated to the provider.
    #[must_use]
    pub const fn local_input_upper_bound(&self) -> Option<u64> {
        self.local_input_upper_bound
    }

    /// Returns the item measurement used by local admission.
    #[must_use]
    pub const fn wire_input(&self) -> Option<&ContextWireInput> {
        self.wire_input.as_ref()
    }

    /// Whether an unanchored estimate delegated the final decision to the provider.
    #[must_use]
    pub const fn uses_unanchored_estimate(&self) -> bool {
        self.unanchored_estimate
    }

    /// Attaches the byte composition of the measured body. The identity is
    /// unchanged: the parts describe the same bytes the measurement counted.
    #[must_use]
    pub const fn with_input_parts(mut self, input_parts: Option<ContextInputParts>) -> Self {
        self.input_parts = input_parts;
        self
    }

    /// Returns the byte composition of the measured body, when attached.
    #[must_use]
    pub const fn input_parts(&self) -> Option<ContextInputParts> {
        self.input_parts
    }

    /// Attaches the provider count occupancy is anchored on. The identity is
    /// unchanged and no admission disposition reads it.
    #[must_use]
    pub const fn with_context_anchor(mut self, anchor: Option<ContextAnchor>) -> Self {
        self.context_anchor = anchor;
        self
    }

    /// Returns the attached occupancy anchor.
    #[must_use]
    pub const fn context_anchor(&self) -> Option<ContextAnchor> {
        self.context_anchor
    }

    /// The `bytes/4` estimate of the whole measured request, from the byte
    /// parts. `None` without parts: the measurement bound is on another
    /// scale (wire bytes on some routes) and is never anchored.
    #[must_use]
    pub fn occupancy_estimate_tokens(&self) -> Option<u64> {
        self.wire_input
            .as_ref()
            .map(ContextWireInput::estimate_tokens)
            .or_else(|| self.input_parts.map(ContextInputParts::occupancy_tokens))
    }

    /// The unanchored size of this request: the parts estimate, or the
    /// measurement's upper bound without parts. Compaction records use this
    /// for both their before and after figures so the two share one basis.
    #[must_use]
    pub fn unanchored_occupancy_tokens(&self) -> Option<u64> {
        self.occupancy_estimate_tokens().or_else(|| {
            self.measurement
                .as_ref()
                .map(|measurement| measurement.bound().upper_bound())
        })
    }

    /// The context measure of this request: [`context_occupancy_tokens`] over
    /// the parts estimate and the attached anchor, or the unanchored bound
    /// when no parts were attached.
    #[must_use]
    pub fn occupancy_tokens(&self) -> Option<u64> {
        if let (Some(input), Some(anchor)) = (&self.wire_input, &self.request_anchor) {
            return Some(anchor.occupied_tokens(input));
        }
        if let Some(input) = &self.wire_input {
            return Some(input.estimate_tokens());
        }
        match self.occupancy_estimate_tokens() {
            Some(estimate) => Some(context_occupancy_tokens(estimate, self.context_anchor)),
            None => self.unanchored_occupancy_tokens(),
        }
    }

    /// Returns the exact resolved-descriptor binding.
    #[must_use]
    pub const fn descriptor_binding(&self) -> Digest {
        self.descriptor_binding
    }

    /// Returns the complete model-input projection identity.
    #[must_use]
    pub const fn model_input_identity(&self) -> Digest {
        self.model_input_identity
    }

    /// Returns the exact scope when a descriptor contract was available.
    #[must_use]
    pub const fn scope(&self) -> Option<&ContextScope> {
        self.scope.as_ref()
    }

    /// Returns admitted capacity when available.
    #[must_use]
    pub const fn capacity(&self) -> Option<&ContextCapacity> {
        self.capacity.as_ref()
    }

    /// Returns admitted measurement when available.
    #[must_use]
    pub const fn measurement(&self) -> Option<&ContextMeasurement> {
        self.measurement.as_ref()
    }

    /// Returns explicit output reservation when available.
    #[must_use]
    pub const fn reservation(&self) -> Option<&OutputReservation> {
        self.reservation.as_ref()
    }

    /// Returns sealed closed-policy evidence when arithmetic overflowed.
    #[must_use]
    pub const fn planner_disposition(&self) -> Option<&ContextPlannerDisposition> {
        self.planner.as_ref()
    }

    /// Returns the checked status.
    #[must_use]
    pub const fn kind(&self) -> &ContextStatusKind {
        &self.kind
    }

    /// Returns the status identity.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.identity
    }

    /// Returns the evidence class when capacity evidence was admitted.
    #[must_use]
    pub fn evidence_class(&self) -> Option<&ContextEvidenceClass> {
        self.capacity.as_ref().map(ContextCapacity::evidence_class)
    }

    /// Returns the mandatory warning for reference-client-envelope capacity.
    #[must_use]
    pub fn non_guarantee_disposition(&self) -> Option<ContextNonGuaranteeDisposition> {
        if self.unanchored_estimate {
            return Some(ContextNonGuaranteeDisposition::UnanchoredProviderDecision);
        }
        self.capacity
            .as_ref()
            .and_then(ContextCapacity::non_guarantee_disposition)
    }

    /// Freezes an admitted request only when this exact status fits.
    ///
    /// # Errors
    ///
    /// Returns typed unavailability/overflow or exact request-projection drift.
    pub fn admit(&self, request: &ModelRequest) -> Result<ContextAdmission, ContextAdmissionError> {
        self.admit_with_requirement(request, ContextAdmissionRequirement::AnyBounded)
    }

    /// Freezes an admitted request only when this exact status fits and meets
    /// the caller's closed minimum-evidence-class requirement.
    ///
    /// # Errors
    ///
    /// Returns before dispatch when release-verified capacity was required but this
    /// status carries a reference-client-envelope operating envelope.
    pub fn admit_with_requirement(
        &self,
        request: &ModelRequest,
        requirement: ContextAdmissionRequirement,
    ) -> Result<ContextAdmission, ContextAdmissionError> {
        if request.model_input_identity() != self.model_input_identity {
            return Err(ContextAdmissionError::Unavailable(
                ContextUnavailable::ScopeDrift {
                    field: "model input",
                },
            ));
        }
        match &self.kind {
            ContextStatusKind::Fits { .. }
                if requirement == ContextAdmissionRequirement::ReleaseVerifiedRequired
                    && (self.evidence_class() != Some(&ContextEvidenceClass::ReleaseVerified)
                        || self.unanchored_estimate) =>
            {
                Err(ContextAdmissionError::Unavailable(
                    ContextUnavailable::EvidenceRequirementNotMet {
                        required: requirement,
                        actual: self.evidence_class().cloned(),
                    },
                ))
            }
            ContextStatusKind::Fits { .. } => {
                ContextAdmission::new(request.clone(), self.clone(), requirement)
            }
            ContextStatusKind::WouldOverflow(overflow) => {
                Err(ContextAdmissionError::WouldOverflow(*overflow))
            }
            ContextStatusKind::ProtectedOverflow(overflow) => {
                Err(ContextAdmissionError::ProtectedOverflow(*overflow))
            }
            ContextStatusKind::Unavailable(unavailable) => {
                Err(ContextAdmissionError::Unavailable(unavailable.clone()))
            }
        }
    }
}

/// A descriptor-checked whole-projection admission witness.
#[derive(Clone, Debug, PartialEq)]
pub struct ContextAdmission {
    request: ModelRequest,
    status: ContextStatus,
    requirement: ContextAdmissionRequirement,
    identity: Digest,
}

impl ContextAdmission {
    fn new(
        request: ModelRequest,
        status: ContextStatus,
        requirement: ContextAdmissionRequirement,
    ) -> Result<Self, ContextAdmissionError> {
        let requirement_bytes = serde_json::to_vec(&requirement).map_err(|error| {
            ContextContractError::Serialization {
                reason: error.to_string(),
            }
        })?;
        let identity = Digest::sha256(
            DigestDomain::ContextAdmission,
            4,
            &[
                &request.identity().identity_bytes(),
                &status.identity().identity_bytes(),
                &requirement_bytes,
            ],
        )?;
        Ok(Self {
            request,
            status,
            requirement,
            identity,
        })
    }

    /// Returns the exact immutable admitted request.
    #[must_use]
    pub const fn request(&self) -> &ModelRequest {
        &self.request
    }

    /// Returns the complete checked status witness.
    #[must_use]
    pub const fn status(&self) -> &ContextStatus {
        &self.status
    }

    /// Returns the caller-selected minimum evidence class bound into this receipt.
    #[must_use]
    pub const fn requirement(&self) -> ContextAdmissionRequirement {
        self.requirement
    }

    /// Returns the admitted evidence class.
    #[must_use]
    pub fn evidence_class(&self) -> Option<&ContextEvidenceClass> {
        self.status.evidence_class()
    }

    /// Returns the mandatory warning for reference-client-envelope capacity.
    #[must_use]
    pub fn non_guarantee_disposition(&self) -> Option<ContextNonGuaranteeDisposition> {
        self.status.non_guarantee_disposition()
    }

    /// Returns the admission identity.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.identity
    }
}
