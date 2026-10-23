//! Turn usage, context projection, and subscription usage refresh.

use std::future::Future;
use std::thread;
use std::time::{Duration, Instant};

use plakous::agent::TurnUsage;
use plakous::application::ApplicationExecutor;
use plakous::sessions::{ContextQuery, SessionContextStatusKind, SessionOperationId};
use plakous::subscriptions::{NativeProvider, NativeSubscriptions};

use crate::agent::AttachedCodingSession;
use crate::context::{
    ContextDisposition, ContextProjection, project_context_status, window_percent,
};
use crate::session_log::{SessionEvent, SessionLog};
use crate::sessions::{self, SessionRecord};
use crate::{CliFailure, CliResult};
use std::sync::Arc;

use super::admission::emit_unavailable_context;
use super::logging::log_event;
use super::{
    HeldUsage, HostEvent, NativeSubscriptionRefresh, SUBSCRIPTION_USAGE_JOIN_DEADLINE, Sender,
    SessionLedger, SubscriptionUsageRefresh,
};

pub(super) fn project_turn_usage<P: plakous_host_protocol::ProtocolExtension>(
    usage: TurnUsage,
    peak_prepared_input_tokens: Option<u64>,
) -> HostEvent<P> {
    let model = usage.model();
    HostEvent::TurnUsage {
        input_tokens: model.input_tokens(),
        output_tokens: model.output_tokens(),
        reasoning_tokens: model.reasoning_tokens(),
        cached_input_tokens: model.cached_input_tokens(),
        cache_write_tokens: model.cache_write_tokens(),
        model_operations: Some(usage.model_operations()),
        reporting_model_operations: Some(usage.model_operations_reporting_usage()),
        peak_prepared_input_tokens,
        terminal: true,
    }
}

/// Publishes a winning projector transaction's terminal observation.
pub(super) fn emit_projected_turn_usage<P: plakous_host_protocol::ProtocolExtension>(
    events: &Sender<P>,
    session_id: &str,
    turn_id: plakous::model::TurnId,
    usage: TurnUsage,
    log: Option<&SessionLog>,
    ledger: &SessionLedger,
) {
    let peak_prepared_input_tokens = usage.peak_input_tokens();
    ledger.record_turn_usage(usage);
    let _ = events.send(project_turn_usage(usage, peak_prepared_input_tokens));
    log_projected_turn_usage(session_id, turn_id, usage, log);
}

pub(super) fn log_projected_turn_usage(
    session_id: &str,
    turn_id: plakous::model::TurnId,
    usage: TurnUsage,
    log: Option<&SessionLog>,
) {
    let model = usage.model();
    let mut record = SessionEvent::new("turn.usage")
        .str("turn_id", turn_id.to_string())
        .u64("input_tokens", model.input_tokens())
        .u64("output_tokens", model.output_tokens())
        .u64("reasoning_tokens", model.reasoning_tokens())
        .u64("cached_input_tokens", model.cached_input_tokens())
        .u64("cache_write_tokens", model.cache_write_tokens())
        .u64("model_operations", usage.model_operations())
        .u64(
            "reporting_model_operations",
            usage.model_operations_reporting_usage(),
        );
    if let Some(peak) = usage.peak_input_tokens() {
        record = record.u64("peak_prepared_input_tokens", peak);
    }
    log_event(log, session_id, record);
}

/// Records one operation's own prepared input and returns the running peak.
///
/// The provider reports `input_tokens` as the complete prompt of *that*
/// request, so the largest one a turn saw is the closest thing the host has to
/// "how big did this conversation get". Zero is not an observation.
pub(super) fn record_operation_prepared_input(
    peak: &mut u64,
    session: &mut SessionRecord,
    update: &plakous::model::UsageUpdate,
    log: Option<&SessionLog>,
) {
    let tokens = update.usage().input_tokens();
    if tokens == 0 {
        return;
    }
    *peak = (*peak).max(tokens);
    log_event(
        log,
        &session.id,
        SessionEvent::new("model.operation_usage")
            .u64("prepared_input_tokens", tokens)
            .u64("peak_prepared_input_tokens", *peak),
    );
}

pub(super) fn report_unavailable_context<P: plakous_host_protocol::ProtocolExtension>(
    events: &Sender<P>,
) {
    emit_unavailable_context(events);
}

pub(super) const CONTEXT_QUERY_FAILED: &str = "canonical context query failed";

pub(super) fn query_context(
    executor: &ApplicationExecutor,
    opened: &AttachedCodingSession,
    query: ContextQuery,
) -> CliResult<ContextProjection> {
    let operation = SessionOperationId::new(format!("plak:context:{}", uuid::Uuid::new_v4()))
        .map_err(|_| CliFailure::new("context query identity is invalid"))?;
    #[cfg(test)]
    if let Some(failure) = crate::error::read_faults::take(CONTEXT_QUERY_FAILED) {
        return Err(failure.into());
    }
    let report = executor
        .block_on(opened.context_status(operation, query))
        .map_err(|_| CliFailure::new("application execution failed"))?
        .map_err(|error| CliFailure::session_read(CONTEXT_QUERY_FAILED, error))?;
    if report.locator() != opened.locator() {
        return Err(CliFailure::new(
            "canonical context report locator does not match the attached session",
        ));
    }
    Ok(match report.kind() {
        SessionContextStatusKind::Available(status) => project_context_status(status),
        SessionContextStatusKind::RecoveryRequired(_) => ContextProjection::recovery_required(),
        SessionContextStatusKind::Unavailable(_) => ContextProjection::unavailable(),
        _ => ContextProjection::unavailable(),
    })
}

/// Last-compiled enrolled instruction-source bytes from the live harness
/// plan. Detached rebind (no agent) reports none; occupancy then leaves
/// `AGENTS.md` inside the system row rather than rediscovering files.
#[must_use]
pub(super) fn last_enrolled_source_bytes(opened: &AttachedCodingSession) -> u64 {
    let Some(agent) = opened.agent() else {
        return 0;
    };
    let plan = agent.harness_plan().instructions();
    let contents: Vec<&str> = plan
        .sources()
        .iter()
        .map(plakous::agent::ResolvedContextSourceFact::content)
        .collect();
    plakous::prompts::instructions::inlined_instruction_source_bytes(
        plan.composed_instructions(),
        &contents,
    )
}

const UNATTRIBUTED_CATEGORY: &str = "Used (unattributed)";
const PROVIDER_OBSERVED_CATEGORY: &str = "Provider-observed (est.)";
const LIVE_GROWTH_CATEGORY: &str = "Live turn growth (est.)";

pub(super) fn emit_context_projection<P: plakous_host_protocol::ProtocolExtension>(
    events: &Sender<P>,
    projection: &ContextProjection,
    counters: sessions::ContextCounters,
    enrolled_source_bytes: u64,
) {
    let _ = events.send(HostEvent::Usage(context_usage(
        projection,
        counters,
        enrolled_source_bytes,
    )));
}

/// The Plakous context measure advanced through a running turn.
///
/// Between canonical context queries the host cannot measure a request, so
/// it keeps the measure's two inputs itself: a whole-request estimate (the
/// held projection's, plus `bytes/4` of streamed assistant text and tool
/// results since) and the anchor, re-anchored on each provider usage report
/// against the estimate as of that round's request. The reading is always
/// [`plakous::model::context_occupancy_tokens`] over those, the one formula
/// the next context query states exactly.
///
/// Within a turn the reading never falls: context only grows until a
/// compaction, so a lower `bytes/4` reading is estimate error. Only a provider
/// count (or a compaction) may state a lower figure.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct LiveOccupancy {
    /// Whole-request estimate through the last round boundary.
    estimate: Option<u64>,
    /// The estimate as of the request the current round sent.
    request_estimate: Option<u64>,
    anchor: Option<plakous::model::ContextAnchor>,
    /// Whether `estimate` is a parts estimate. A bound is on another scale,
    /// so a provider count never anchors it (spec 01 §10.3).
    parts_estimate: bool,
    /// Bytes appended since the last round boundary.
    pending_round_bytes: u64,
    /// The last reading shown this turn; `None` after a compaction.
    floor: Option<u64>,
    /// A provider count arrived since the last reading, which it may lower.
    provider_counted: bool,
}

impl LiveOccupancy {
    pub(super) const fn from_projection(projection: &ContextProjection) -> Self {
        let estimate = if projection.occupied_tokens().is_some() {
            projection.occupancy_estimate()
        } else {
            None
        };
        let parts_estimate = projection.measures_input_parts();
        Self {
            estimate,
            request_estimate: if parts_estimate { estimate } else { None },
            anchor: projection.context_anchor(),
            parts_estimate,
            pending_round_bytes: 0,
            floor: None,
            provider_counted: false,
        }
    }

    /// The driver prepared the next request of this turn. Its parts estimate
    /// replaces the running one, and the newest provider count this turn saw
    /// stays the anchor: the held projection's anchor is only as new as the
    /// turn's start, and re-reading it each round made the meter fall back to
    /// that older count before every round and rise again on its usage report.
    pub(super) fn next_request(self, projection: &ContextProjection) -> Self {
        let mut next = Self::from_projection(projection);
        if next.parts_estimate && self.parts_estimate {
            next.anchor = self.anchor;
        }
        next.floor = self.floor;
        next
    }

    /// A model round starts: what was appended becomes part of its request.
    pub(super) const fn round_started(&mut self) {
        if let Some(estimate) = self.estimate {
            self.estimate = Some(estimate.saturating_add(self.pending_round_bytes / 4));
        }
        self.pending_round_bytes = 0;
        self.request_estimate = if self.parts_estimate {
            self.estimate
        } else {
            None
        };
    }

    pub(super) const fn appended(&mut self, bytes: u64) {
        self.pending_round_bytes = self.pending_round_bytes.saturating_add(bytes);
    }

    /// A replay discarded the current round's streamed output.
    pub(super) const fn discard_round_output(&mut self) {
        self.pending_round_bytes = 0;
    }

    pub(super) const fn pending_round_bytes(&self) -> u64 {
        self.pending_round_bytes
    }

    /// The provider counted the current round's request. Zero is no report.
    pub(super) fn provider_input(&mut self, input_tokens: u64) {
        if let Some(anchor) = self
            .request_estimate
            .and_then(|estimate| plakous::model::ContextAnchor::new(input_tokens, estimate))
        {
            self.anchor = Some(anchor);
            self.provider_counted = true;
        }
    }

    /// A compaction committed: its successor's measured estimate, else its
    /// audited bound, unanchored. Only a measured estimate can be anchored.
    pub(super) const fn compacted(&mut self, measured: Option<u64>, bound: u64) {
        let (estimate, parts_estimate) = match measured {
            Some(estimate) => (estimate, true),
            None => (bound, false),
        };
        self.estimate = Some(estimate);
        self.request_estimate = if parts_estimate { Some(estimate) } else { None };
        self.anchor = None;
        self.parts_estimate = parts_estimate;
        self.pending_round_bytes = 0;
        self.floor = None;
        self.provider_counted = false;
    }

    pub(super) const fn anchored(self) -> bool {
        self.anchor.is_some()
    }

    /// The reading to show now, recorded as this turn's floor.
    pub(super) fn tokens(&mut self) -> Option<u64> {
        let measured = self.estimate.map(|estimate| {
            plakous::model::context_occupancy_tokens(
                estimate.saturating_add(self.pending_round_bytes / 4),
                self.anchor,
            )
        })?;
        let reading = match self.floor {
            Some(floor) if !self.provider_counted => measured.max(floor),
            _ => measured,
        };
        self.floor = Some(reading);
        self.provider_counted = false;
        Some(reading)
    }
}

/// Refreshes occupancy during a running turn. Every prepared request
/// publishes its own parts, so the breakdown is the current request's, never a
/// held one that would need an age.
#[allow(
    clippy::too_many_arguments,
    reason = "the live reading is its estimate and whether a provider count anchors it"
)]
pub(super) fn emit_live_context_projection<P: plakous_host_protocol::ProtocolExtension>(
    events: &Sender<P>,
    projection: &ContextProjection,
    counters: sessions::ContextCounters,
    enrolled_source_bytes: u64,
    compacted_mid_turn: bool,
    estimated_occupancy: Option<u64>,
    live_anchored: bool,
    tool_names: &[String],
) {
    let mut usage = if compacted_mid_turn {
        let Some(input_tokens) = estimated_occupancy else {
            return;
        };
        // A committed compact invalidates the old request body. Its measured
        // categories and prepared bound cannot be used as a floor for the new
        // body; the live reading is the only size.
        let Some(window) = projection
            .total_window()
            .map(crate::context::TotalContextWindow::tokens)
        else {
            return;
        };
        let percent = window_percent(input_tokens, window);
        let mut usage = context_usage(projection, counters, enrolled_source_bytes);
        usage.breakdown = Some(
            plakous_host_protocol::views::presentation::ContextBreakdownView {
                used_tokens: input_tokens,
                total_tokens: window,
                categories: vec![
                    plakous_host_protocol::views::presentation::ContextCategoryView {
                        label: LIVE_GROWTH_CATEGORY.to_owned(),
                        tokens: input_tokens,
                    },
                ],
                informational: Vec::new(),
                auto_compact_trigger_tokens: projection.auto_compact_trigger(),
                occupancy_anchored: live_anchored,
                turns: Some(counters.turns),
                tool_calls: Some(counters.tool_calls),
                compactions: Some(counters.compactions),
            },
        );
        usage.prepared_input = None;
        usage.gauge_percent = percent.map(|value| u8::try_from(value.min(100)).unwrap_or(100));
        usage.display_percent = percent;
        usage.ceiling_excess = None;
        usage
    } else {
        context_usage(projection, counters, enrolled_source_bytes)
    };
    // The live reading replaces the held one: the provider's count of the
    // request just sent plus what was appended since is the same measure the
    // held projection states, only newer.
    if !compacted_mid_turn
        && let Some(estimated) = estimated_occupancy
        && let Some(window) = usage.total_window
        && let Some(breakdown) = &mut usage.breakdown
        && estimated != breakdown.used_tokens
    {
        let percent = window_percent(estimated, window);
        usage.gauge_percent = percent.map(|value| u8::try_from(value.min(100)).unwrap_or(100));
        usage.display_percent = percent;
        let held = breakdown.used_tokens;
        breakdown.used_tokens = estimated;
        fit_categories(
            &mut breakdown.categories,
            held,
            estimated,
            LIVE_GROWTH_CATEGORY,
        );
    }
    if let Some(breakdown) = &mut usage.breakdown {
        if estimated_occupancy.is_some() {
            breakdown.occupancy_anchored = live_anchored;
        }
        append_request_tool_names(breakdown, tool_names);
    }
    let _ = events.send(HostEvent::Usage(usage));
}

fn append_request_tool_names(
    breakdown: &mut plakous_host_protocol::views::presentation::ContextBreakdownView,
    names: &[String],
) {
    const AGENT_TOOLS: &[&str] = &[
        "spawn_agent",
        "wait_agent",
        "send_agent_message",
        "list_agents",
        "submit_result",
        "interrupt_agent",
        "recover_agent",
        "close_agent",
        "adopt_candidate",
        "discard_workspace",
    ];
    let (agent, coding): (Vec<_>, Vec<_>) = names
        .iter()
        .partition(|name| AGENT_TOOLS.contains(&name.as_str()));
    for (label, tools) in [
        ("Coding tools on the last request", coding),
        ("Agent tools on the last request", agent),
    ] {
        if !tools.is_empty() {
            breakdown.informational.push(
                plakous_host_protocol::views::presentation::ContextInfoRowView {
                    label: label.to_owned(),
                    tokens: 0,
                    detail: Some(tools.into_iter().cloned().collect::<Vec<_>>().join(", ")),
                },
            );
        }
    }
}

/// Refits occupying categories that state `from` tokens to state `to`. The
/// categories split a request's `bytes÷4` estimate; a larger total adds the
/// difference as its own `excess_label` row, and a smaller one scales every
/// category down in proportion, so the board never paints more than the
/// meter states.
fn fit_categories(
    categories: &mut Vec<plakous_host_protocol::views::presentation::ContextCategoryView>,
    from: u64,
    to: u64,
    excess_label: &str,
) {
    if to >= from {
        if to > from {
            categories.push(
                plakous_host_protocol::views::presentation::ContextCategoryView {
                    label: excess_label.to_owned(),
                    tokens: to - from,
                },
            );
        }
        return;
    }
    let scale = |tokens: u64| {
        u64::try_from(u128::from(tokens) * u128::from(to) / u128::from(from)).unwrap_or(u64::MAX)
    };
    let target = scale(categories.iter().map(|category| category.tokens).sum());
    let mut scaled_total = 0_u64;
    for category in categories.iter_mut() {
        category.tokens = scale(category.tokens);
        scaled_total = scaled_total.saturating_add(category.tokens);
    }
    // Flooring each row loses less than one token per row; the largest row
    // carries it so the rows sum to the scaled total.
    if let Some(largest) = categories.iter_mut().max_by_key(|category| category.tokens) {
        largest.tokens = largest
            .tokens
            .saturating_add(target.saturating_sub(scaled_total));
    }
    categories.retain(|category| category.tokens > 0);
}

fn context_usage(
    projection: &ContextProjection,
    counters: sessions::ContextCounters,
    enrolled_source_bytes: u64,
) -> plakous_host_protocol::views::presentation::ContextUsage {
    let prepared = projection.prepared_input();
    let ceiling = projection.safe_admitted_input_ceiling();
    let utilization = projection.actionable_utilization();
    let display_percent = match (projection.occupied_tokens(), projection.total_window()) {
        (Some(occupied), Some(window)) => window_percent(occupied, window.tokens()),
        _ => None,
    };
    plakous_host_protocol::views::presentation::ContextUsage {
        breakdown: context_breakdown(projection, counters, enrolled_source_bytes),
        prepared_input: prepared.map(|value| match value {
            crate::context::PreparedInputCount::Exact(tokens) => {
                plakous_host_protocol::views::presentation::PreparedInputUsage::Exact(tokens)
            }
            crate::context::PreparedInputCount::UpperBound(tokens) => {
                plakous_host_protocol::views::presentation::PreparedInputUsage::UpperBound(tokens)
            }
        }),
        total_window: projection
            .total_window()
            .map(crate::context::TotalContextWindow::tokens),
        admitted_ceiling: ceiling.map(|value| plakous_host_protocol::views::presentation::AdmittedContextCeiling {
            tokens: value.tokens(),
            basis: match value.basis() {
                crate::context::AdmittedCeilingBasis::ExactCapacity => "exact",
                crate::context::AdmittedCeilingBasis::GuaranteedCapacityFloor => "guaranteed-floor",
                crate::context::AdmittedCeilingBasis::OperatingEnvelope => "envelope",
            }
            .to_owned(),
        }),
        gauge_percent: utilization.map(crate::context::ActionableUtilization::gauge_percent),
        display_percent,
        ceiling_excess: utilization
            .and_then(crate::context::ActionableUtilization::ceiling_excess)
            .map(|excess| match excess {
                crate::context::CeilingExcess::ExactOverflow(tokens) => {
                    plakous_host_protocol::views::presentation::ContextCeilingExcess::ExactOverflow(tokens)
                }
                crate::context::CeilingExcess::UpperBoundExcess(tokens) => {
                    plakous_host_protocol::views::presentation::ContextCeilingExcess::UpperBoundExcess(tokens)
                }
            }),
        evidence: projection.evidence().label().to_owned(),
        non_guarantee: projection
            .non_guarantee()
            .map(|value| value.label().to_owned()),
        status: projection.disposition().label().to_owned(),
    }
}

/// The Context tab breakdown: occupancy is the Plakous context measure, its
/// categories split the request's `part_bytes÷4` (spec 02 §5.6) fitted to that
/// total, enrolled `AGENTS.md` is split out of system, and the admission bound
/// is not smeared onto categories. Categories are labelled `est.` because the
/// provider reports no per-part tokenizer count.
pub(super) fn context_breakdown(
    projection: &ContextProjection,
    counters: sessions::ContextCounters,
    enrolled_source_bytes: u64,
) -> Option<plakous_host_protocol::views::presentation::ContextBreakdownView> {
    let total_tokens = projection.total_window()?.tokens();
    let estimate = projection.input_estimate_with_enrolled(enrolled_source_bytes);
    let used_tokens = projection.occupied_tokens()?;
    let mut categories = Vec::new();
    let mut informational = Vec::new();
    if let Some(estimate) = estimate {
        let category = |label: String, tokens: u64| {
            plakous_host_protocol::views::presentation::ContextCategoryView { label, tokens }
        };
        if estimate.system_tokens > 0 {
            categories.push(category(
                "System prompt (est.)".to_owned(),
                estimate.system_tokens,
            ));
        }
        if estimate.tool_definition_tokens > 0 {
            categories.push(category(
                format!(
                    "{} tool{} on the last request; tokens est.",
                    estimate.tool_definition_count,
                    if estimate.tool_definition_count == 1 {
                        ""
                    } else {
                        "s"
                    }
                ),
                estimate.tool_definition_tokens,
            ));
        }
        if estimate.enrolled_source_tokens > 0 {
            categories.push(category(
                "Auto-loaded (est. · AGENTS.md)".to_owned(),
                estimate.enrolled_source_tokens,
            ));
        }
        if estimate.message_tokens > 0 {
            categories.push(category(
                "Messages (est.)".to_owned(),
                estimate.message_tokens,
            ));
        }
        if estimate.reasoning_tokens > 0 {
            categories.push(category(
                "Reasoning replay (est.)".to_owned(),
                estimate.reasoning_tokens,
            ));
        }
        if estimate.envelope_tokens > 0 {
            categories.push(category(
                UNATTRIBUTED_CATEGORY.to_owned(),
                estimate.envelope_tokens,
            ));
        }
        let occupancy = estimate.occupancy_tokens();
        let estimated: u64 = categories.iter().map(|category| category.tokens).sum();
        fit_categories(
            &mut categories,
            estimated,
            used_tokens,
            PROVIDER_OBSERVED_CATEGORY,
        );
        if let Some(prepared) = projection.prepared_input() {
            let padding = prepared.tokens().saturating_sub(occupancy);
            if padding > 0 {
                informational.push(
                    plakous_host_protocol::views::presentation::ContextInfoRowView {
                        label: "Admission padding (est.)".to_owned(),
                        tokens: padding,
                        detail: None,
                    },
                );
            }
        }
    }
    Some(
        plakous_host_protocol::views::presentation::ContextBreakdownView {
            used_tokens,
            total_tokens,
            categories,
            informational,
            auto_compact_trigger_tokens: projection.auto_compact_trigger(),
            occupancy_anchored: projection.context_anchor().is_some(),
            turns: Some(counters.turns),
            tool_calls: Some(counters.tool_calls),
            compactions: Some(counters.compactions),
        },
    )
}

pub(super) fn report_context<P: plakous_host_protocol::ProtocolExtension>(
    executor: &ApplicationExecutor,
    opened: &AttachedCodingSession,
    events: &Sender<P>,
    stage: &'static str,
    native_compact_facet: bool,
    counters: sessions::ContextCounters,
) -> CliResult<ContextProjection> {
    let projection = match query_context(executor, opened, ContextQuery::Current) {
        Ok(projection) => projection,
        Err(error) if error.store_refusal() == Some(crate::error::StoreRefusal::Busy) => {
            tracing::warn!(
                target: "plak.context",
                stage,
                refusal = "busy",
                "context meter unavailable for this refresh"
            );
            emit_unavailable_context(events);
            return Ok(ContextProjection::unavailable());
        }
        Err(error) => {
            return Err(error.with_context(format!("context meter {stage}")));
        }
    };
    emit_context_projection(
        events,
        &projection,
        counters,
        last_enrolled_source_bytes(opened),
    );
    emit_capacity_recovery(events, projection.disposition(), native_compact_facet);
    Ok(projection)
}

pub(super) fn request_subscription_usage_service<P: plakous_host_protocol::ProtocolExtension>(
    subscriptions: Option<NativeSubscriptions>,
    refresh: &SubscriptionUsageRefresh<P>,
) {
    refresh.request(subscriptions);
}

/// Serves a still-fresh held projection, or starts a provider read.
///
/// Idle `/usage` and Usage/Context tab opens share this path. A projection
/// younger than [`crate::subscription_usage::USAGE_PROJECTION_FRESHNESS`] is
/// current (`stale_age_seconds = None`) and does not hit the network — Claude's
/// `/api/oauth/usage` 429s if every tab switch re-queries. The session ledger
/// is host-owned, so it is overlaid from the live totals rather than frozen
/// with the meters.
pub(super) fn request_subscription_usage_cached<P: plakous_host_protocol::ProtocolExtension>(
    held: &HeldUsage,
    ledger: &SessionLedger,
    events: &Sender<P>,
    refresh: &SubscriptionUsageRefresh<P>,
    subscriptions: Option<NativeSubscriptions>,
) {
    if let Some(mut fresh) = held.fresh() {
        fresh.session = ledger.project();
        let _ = events.send(HostEvent::UsageLimits(fresh));
        let _ = events.send(HostEvent::SubscriptionUsage(
            plakous_host_protocol::views::presentation::SubscriptionUsageView::Read,
        ));
        return;
    }
    refresh.request(subscriptions);
}

impl HeldUsage {
    #[cfg(test)]
    pub(super) fn store(&self, view: &plakous_host_protocol::views::presentation::UsageLimitsView) {
        if let Ok(mut held) = self.inner.lock() {
            *held = Some((view.clone(), Instant::now()));
        }
    }

    /// Returns the held projection as current when it is still within the idle
    /// freshness window. `stale_age_seconds` is cleared: this is not the
    /// in-turn overlay.
    pub(super) fn fresh(
        &self,
    ) -> Option<plakous_host_protocol::views::presentation::UsageLimitsView> {
        let held = self.inner.lock().ok()?;
        let (view, observed) = held.as_ref()?;
        crate::subscription_usage::cached_if_fresh(view, observed.elapsed())
    }

    /// Merges a newly projected read with the last Ready meters, then stores
    /// the result.
    ///
    /// A Claude 429 does not throw away numbers the host already holds.
    /// A retained observation keeps its original freshness clock.
    pub(super) fn preserve_and_store(
        &self,
        incoming: plakous_host_protocol::views::presentation::UsageLimitsView,
    ) -> plakous_host_protocol::views::presentation::UsageLimitsView {
        let Ok(mut held) = self.inner.lock() else {
            return incoming;
        };
        let previous = held.as_ref().map(|(view, _)| view);
        let retained = incoming.verticals.iter().any(|vertical| matches!(
            &vertical.state,
            plakous_host_protocol::views::presentation::VerticalUsageState::Unavailable { reason }
                if crate::subscription_usage::is_rate_limited_reason(reason)
                    || crate::subscription_usage::is_busy_reason(reason)
        ));
        let merged =
            crate::subscription_usage::preserve_ready_across_rate_limit(incoming, previous);
        let observed = if retained {
            held.as_ref()
                .map_or_else(Instant::now, |(_, observed)| *observed)
        } else {
            Instant::now()
        };
        *held = Some((merged.clone(), observed));
        merged
    }

    /// Returns the held projection stamped with its whole-second age.
    pub(super) fn stale(
        &self,
    ) -> Option<plakous_host_protocol::views::presentation::UsageLimitsView> {
        let held = self.inner.lock().ok()?;
        let (view, observed) = held.as_ref()?;
        let mut view = view.clone();
        view.stale_age_seconds = Some(observed.elapsed().as_secs());
        Some(view)
    }
}

impl SessionLedger {
    pub(super) fn record_turn_usage(&self, usage: TurnUsage) {
        let Ok(mut totals) = self.inner.lock() else {
            return;
        };
        let model = usage.model();
        totals.input_tokens = totals.input_tokens.saturating_add(model.input_tokens());
        totals.cached_input_tokens = totals
            .cached_input_tokens
            .saturating_add(model.cached_input_tokens());
        totals.output_tokens = totals.output_tokens.saturating_add(model.output_tokens());
        totals.reasoning_tokens = totals
            .reasoning_tokens
            .saturating_add(model.reasoning_tokens());
        totals.model_calls = totals.model_calls.saturating_add(usage.model_operations());
        totals.reporting_model_calls = totals
            .reporting_model_calls
            .saturating_add(usage.model_operations_reporting_usage());
    }

    /// Records the measured wall time of one admitted turn dispatch. This is
    /// the host's own measurement, not a provider-reported figure.
    pub(super) fn record_dispatch(&self, dispatch: Duration) {
        let Ok(mut totals) = self.inner.lock() else {
            return;
        };
        totals.dispatch_millis = totals
            .dispatch_millis
            .saturating_add(u64::try_from(dispatch.as_millis()).unwrap_or(u64::MAX));
        totals.turns = totals.turns.saturating_add(1);
    }

    pub(super) fn project(&self) -> plakous_host_protocol::views::presentation::SessionLedgerView {
        let Ok(totals) = self.inner.lock() else {
            return plakous_host_protocol::views::presentation::SessionLedgerView::default();
        };
        crate::subscription_usage::project_session_ledger(
            totals.input_tokens,
            totals.cached_input_tokens,
            totals.output_tokens,
            totals.reasoning_tokens,
            totals.model_calls,
            totals.reporting_model_calls,
            (totals.turns > 0).then(|| Duration::from_millis(totals.dispatch_millis)),
        )
    }
}

impl<P: plakous_host_protocol::ProtocolExtension> SubscriptionUsageRefresh<P> {
    pub(super) fn new(
        events: &Sender<P>,
        credentials: Arc<dyn crate::openrouter::OpenRouterCredentials>,
        held: HeldUsage,
        ledger: SessionLedger,
    ) -> Self {
        let mut executor = None;
        Self::new_with_projector(events, move |subscriptions| {
            let executor = executor.get_or_insert_with(ApplicationExecutor::new);
            match executor {
                Ok(executor) => {
                    let native = refresh_native_subscription_usage(executor, subscriptions);
                    let projected = project_subscription_usage(native.clone());
                    // The complete four-vertical projection is the product
                    // surface; the legacy overlay view is derived alongside it.
                    let mut limits = project_usage_limits(native, credentials.as_ref());
                    limits.session = ledger.project();
                    let limits = held.preserve_and_store(limits);
                    (projected, Some(limits))
                }
                Err(_) => (
                    plakous_host_protocol::views::presentation::SubscriptionUsageView::Unavailable {
                        reason: "subscription usage worker failed".to_owned(),
                    },
                    None,
                ),
            }
        })
    }

    pub(super) fn new_with_projector(
        events: &Sender<P>,
        mut project: impl FnMut(
            Option<NativeSubscriptions>,
        ) -> (
            plakous_host_protocol::views::presentation::SubscriptionUsageView,
            Option<plakous_host_protocol::views::presentation::UsageLimitsView>,
        ) + Send
        + 'static,
    ) -> Self {
        let (requests, receiver) = std::sync::mpsc::sync_channel(1);
        let in_flight = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_in_flight = Arc::clone(&in_flight);
        let reads = super::surface::UsageReads::default();
        let worker_events = events.clone();
        let worker = thread::Builder::new()
            .name("plak-subscription-usage".to_owned())
            .spawn(move || {
                // The read's guard settles it once its projection is published.
                while let Ok((subscriptions, _read)) = receiver.recv() {
                    let (projected, limits) = project(subscriptions);
                    let _ = worker_events.send(HostEvent::SubscriptionUsage(projected));
                    if let Some(limits) = limits {
                        let _ = worker_events.send(HostEvent::UsageLimits(limits));
                    }
                    worker_in_flight.store(false, std::sync::atomic::Ordering::Release);
                }
            })
            .ok();
        Self {
            requests: worker.as_ref().map(|_| requests),
            worker,
            events: events.clone(),
            in_flight,
            reads,
            last_request: std::sync::Mutex::new(None),
        }
    }

    /// The reads this refresh has queued and not yet finished, for a surface
    /// that must not read subscription posture while one runs.
    pub(super) fn reads(&self) -> super::surface::UsageReads {
        self.reads.clone()
    }

    pub(super) fn poll_idle(
        &self,
        held: &HeldUsage,
        ledger: &SessionLedger,
        subscriptions: Option<NativeSubscriptions>,
    ) {
        let due = self
            .last_request
            .lock()
            .is_ok_and(|last| last.is_some_and(|at| at.elapsed() >= Duration::from_secs(300)));
        if due {
            request_subscription_usage_cached(held, ledger, &self.events, self, subscriptions);
        }
    }

    pub(super) fn request(&self, subscriptions: Option<NativeSubscriptions>) {
        self.request_inner(subscriptions, false);
    }

    /// An explicit credential change must be read even if an older read is in flight.
    /// The bounded request channel coalesces multiple explicit requests into one follow-up.
    pub(super) fn request_fresh(&self, subscriptions: Option<NativeSubscriptions>) {
        self.request_inner(subscriptions, true);
    }

    fn request_inner(&self, subscriptions: Option<NativeSubscriptions>, fresh: bool) {
        let Some(requests) = &self.requests else {
            let _ = self.events.send(HostEvent::SubscriptionUsage(
                plakous_host_protocol::views::presentation::SubscriptionUsageView::Unavailable {
                    reason: "subscription usage worker could not start".to_owned(),
                },
            ));
            return;
        };
        if self
            .in_flight
            .swap(true, std::sync::atomic::Ordering::AcqRel)
            && !fresh
        {
            return;
        }
        if let Ok(mut last) = self.last_request.lock() {
            *last = Some(Instant::now());
        }
        let _ = self.events.send(HostEvent::SubscriptionUsage(
            plakous_host_protocol::views::presentation::SubscriptionUsageView::Loading,
        ));
        // A refused send drops its guard; a queued read absorbs this request.
        match requests.try_send((subscriptions, self.reads.begin())) {
            Ok(()) | Err(std::sync::mpsc::TrySendError::Full(_)) => {}
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                self.in_flight
                    .store(false, std::sync::atomic::Ordering::Release);
                let _ = self.events.send(HostEvent::SubscriptionUsage(
                    plakous_host_protocol::views::presentation::SubscriptionUsageView::Unavailable {
                        reason: "subscription usage worker stopped".to_owned(),
                    },
                ));
            }
        }
    }
}

impl<P: plakous_host_protocol::ProtocolExtension> Drop for SubscriptionUsageRefresh<P> {
    fn drop(&mut self) {
        // Closing the request channel is what asks the worker to finish; a
        // parked worker observes it immediately.
        self.requests.take();
        let Some(worker) = self.worker.take() else {
            return;
        };
        let deadline = Instant::now() + SUBSCRIPTION_USAGE_JOIN_DEADLINE;
        while !worker.is_finished() {
            if Instant::now() >= deadline {
                // Detached: the process is tearing down and the worker can only
                // write into an event channel nothing reads any more.
                return;
            }
            thread::sleep(Duration::from_millis(2));
        }
        let _ = worker.join();
    }
}

impl NativeSubscriptionRefresh {
    /// The one operator-facing sentence for a refresh that produced no read.
    pub(super) fn unavailable_reason(&self) -> Option<&'static str> {
        match self {
            Self::Read(_) => None,
            Self::NoSubscriptionStore => {
                Some("no native subscription store is configured for this session")
            }
            Self::TimedOut => Some("provider allowance refresh timed out; retry to refresh"),
        }
    }
}

pub(super) fn refresh_native_subscription_usage(
    executor: &ApplicationExecutor,
    subscriptions: Option<NativeSubscriptions>,
) -> NativeSubscriptionRefresh {
    const REFRESH_TIMEOUT: Duration = Duration::from_secs(5);
    let Some(subscriptions) = subscriptions else {
        return NativeSubscriptionRefresh::NoSubscriptionStore;
    };
    let read = executor
        .block_on(async {
            bounded_subscription_refresh(
                REFRESH_TIMEOUT,
                Box::pin(async move {
                    // Sequential, not joined: every provider's compatibility lock
                    // shares one parent-directory `flock`, so concurrent reads
                    // would make the loser miss its non-blocking acquire and
                    // report `Busy` for what is only a sibling read.
                    let codex = subscription_usage_after_busy_retries(
                        &subscriptions,
                        NativeProvider::Codex,
                    )
                    .await;
                    let grok =
                        subscription_usage_after_busy_retries(&subscriptions, NativeProvider::Xai)
                            .await;
                    let claude = subscription_usage_after_busy_retries(
                        &subscriptions,
                        NativeProvider::Claude,
                    )
                    .await;
                    (codex, grok, claude)
                }),
            )
            .await
        })
        .ok()
        .flatten();
    read.map_or(NativeSubscriptionRefresh::TimedOut, |read| {
        NativeSubscriptionRefresh::Read(Box::new(read))
    })
}

pub(super) fn project_subscription_usage(
    native: NativeSubscriptionRefresh,
) -> plakous_host_protocol::views::presentation::SubscriptionUsageView {
    let reason = native.unavailable_reason();
    match (native, reason) {
        // The numbers reach the Limits section through `UsageLimitsView`; this
        // view says only that a terminal read landed.
        (NativeSubscriptionRefresh::Read(_), _) => {
            plakous_host_protocol::views::presentation::SubscriptionUsageView::Read
        }
        (_, Some(reason)) => {
            plakous_host_protocol::views::presentation::SubscriptionUsageView::Unavailable {
                reason: reason.to_owned(),
            }
        }
        (_, None) => {
            plakous_host_protocol::views::presentation::SubscriptionUsageView::Unavailable {
                reason: "provider allowance refresh produced no observation".to_owned(),
            }
        }
    }
}

/// Collects every Usage vertical into the one host-owned projection.
///
/// All four verticals are always collected, so all four are always listed: the
/// three native subscriptions concurrently, then `OpenRouter` on demand. The
/// `OpenRouter` key read is deliberately outside the turn path and is issued
/// only here, when the Usage surface asks for it.
pub(super) fn project_usage_limits(
    native: NativeSubscriptionRefresh,
    credentials: &dyn crate::openrouter::OpenRouterCredentials,
) -> plakous_host_protocol::views::presentation::UsageLimitsView {
    use crate::subscription_usage::{
        VerticalObservation, claude_vertical, codex_vertical, openrouter_vertical,
        product_verticals, supergrok_vertical,
    };

    let mut observations = Vec::with_capacity(product_verticals().len());
    match native {
        NativeSubscriptionRefresh::Read(read) => {
            let (codex, grok, claude) = *read;
            observations.push((codex_vertical(), VerticalObservation::Native(codex)));
            observations.push((supergrok_vertical(), VerticalObservation::Native(grok)));
            observations.push((claude_vertical(), VerticalObservation::Native(claude)));
        }
        other => {
            let reason = other
                .unavailable_reason()
                .unwrap_or("provider allowance refresh produced no observation");
            for vertical in [codex_vertical(), supergrok_vertical(), claude_vertical()] {
                observations.push((
                    vertical,
                    VerticalObservation::Unavailable(reason.to_owned()),
                ));
            }
        }
    }
    observations.push((
        openrouter_vertical(),
        match crate::openrouter::probe_openrouter_key_usage(credentials) {
            crate::openrouter::OpenRouterKeyUsageProbe::NotConfigured => {
                VerticalObservation::NotConnected
            }
            crate::openrouter::OpenRouterKeyUsageProbe::Ok(usage, label) => {
                VerticalObservation::OpenRouterKey(usage, label)
            }
            crate::openrouter::OpenRouterKeyUsageProbe::Failed(reason) => {
                VerticalObservation::Unavailable(reason)
            }
        },
    ));
    // The caller stamps the session ledger; collection here stays provider-only.
    crate::subscription_usage::project_limits(
        observations,
        plakous_host_protocol::views::presentation::SessionLedgerView::default(),
    )
}

const USAGE_BUSY_RETRIES: u32 = 8;
const USAGE_BUSY_RETRY_DELAY: Duration = Duration::from_millis(50);

fn usage_read_is_busy(read: &plakous::model::SubscriptionUsageRead) -> bool {
    matches!(
        read,
        plakous::model::SubscriptionUsageRead::Unavailable {
            reason: plakous::model::SubscriptionUsageUnavailable::Busy,
            ..
        }
    )
}

/// One observational read, retrying a brief lock miss.
///
/// Shared readers coexist with Ready leases; a writer's brief record
/// replacement or a refresh claim still in flight may contend. Those misses
/// are not a storage-safety fault.
async fn subscription_usage_after_busy_retries(
    subscriptions: &NativeSubscriptions,
    provider: NativeProvider,
) -> plakous::model::SubscriptionUsageRead {
    await_usage_not_busy(|| subscriptions.subscription_usage(provider)).await
}

async fn await_usage_not_busy<F, Fut>(mut read: F) -> plakous::model::SubscriptionUsageRead
where
    F: FnMut() -> Fut,
    Fut: Future<Output = plakous::model::SubscriptionUsageRead>,
{
    let mut last = read().await;
    for _ in 1..USAGE_BUSY_RETRIES {
        if !usage_read_is_busy(&last) {
            return last;
        }
        tokio::time::sleep(USAGE_BUSY_RETRY_DELAY).await;
        last = read().await;
    }
    last
}

pub(super) async fn bounded_subscription_refresh<T>(
    limit: Duration,
    refresh: impl Future<Output = T>,
) -> Option<T> {
    tokio::time::timeout(limit, refresh).await.ok()
}

pub(super) fn emit_capacity_recovery<P: plakous_host_protocol::ProtocolExtension>(
    events: &Sender<P>,
    disposition: ContextDisposition,
    native_compact_facet: bool,
) {
    let _ = events.send(HostEvent::ContextCapacityRecovery(live_capacity_recovery(
        disposition,
        native_compact_facet,
    )));
}

pub(super) const fn live_capacity_recovery(
    disposition: ContextDisposition,
    native_compact_facet: bool,
) -> Option<plakous_host_protocol::ContextCapacityRecovery> {
    match disposition {
        ContextDisposition::ProtectedOverflow if native_compact_facet => None,
        ContextDisposition::ProtectedOverflow => {
            Some(plakous_host_protocol::ContextCapacityRecovery::Protected)
        }
        _ => None,
    }
}

/// Which compact a typed disposition asks for.
///
/// Every disposition asks for one. This used to return `Option` and answer
/// `None` for `ProtectedOverflow` without the native facet, which made
/// `/compact` refuse before the ladder ran — while the capacity-recovery
/// guidance was already telling the operator that `/compact` may replace the
/// oldest tool-result bodies. The "only protected material" wording now
/// belongs to the `NoEligibleRange` outcome, which is the one case the ladder
/// really cannot rescue, so there is no disposition left to refuse up front.
pub(super) const fn compact_reason_for_disposition(
    disposition: ContextDisposition,
) -> sessions::ProductCompactReason {
    match disposition {
        ContextDisposition::WouldOverflow | ContextDisposition::ProtectedOverflow => {
            sessions::ProductCompactReason::CapacityRecovery
        }
        _ => sessions::ProductCompactReason::Manual,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use crate::runtime::test_kit::TestExtension;
    type HostEvent = crate::surface_protocol::HostEvent<TestExtension>;
    type Sender = crate::runtime::Sender<TestExtension>;
    type UserAction = crate::surface_protocol::UserAction<TestExtension>;
    use crate::runtime::test_kit::ProbeIntent;
    use crate::runtime::test_kit::ProbeObservation;

    use super::*;
    use crate::runtime::*;

    use crate::runtime::test_support::*;
    use crate::sessions::SessionRecord;
    use plakous::agent::TurnUsage;
    use plakous::application::ApplicationExecutor;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::thread;
    use std::time::{Duration, Instant};

    #[test]
    fn projected_usage_log_carries_its_authoritative_turn_id() {
        let root = tempfile::tempdir().unwrap();
        let home = SessionHome::new(root.path().join("sessions"));
        let session_id = uuid::Uuid::new_v4().to_string();
        let turn_id = plakous::model::TurnId::try_from(uuid::Uuid::new_v4()).unwrap();
        let log = SessionLog::open(&home, &session_id).unwrap();
        let (events, _receiver) = crate::surface_protocol::host_event_channel::<TestExtension>();
        emit_projected_turn_usage(
            &Sender::over(events),
            &session_id,
            turn_id,
            TurnUsage::default(),
            Some(&log),
            &SessionLedger::default(),
        );
        let rows = crate::session_log::read_tail(&home, &session_id, 10).unwrap();
        let row: serde_json::Value = serde_json::from_str(&rows[0]).unwrap();
        assert_eq!(row["kind"], "turn.usage");
        assert_eq!(row["turn_id"], turn_id.to_string());
    }

    /// A refresh that never ran is not reported as a timeout (spec 02 §7.2).
    #[test]
    fn a_missing_subscription_store_is_not_reported_as_a_timeout() {
        let no_store = NativeSubscriptionRefresh::NoSubscriptionStore;
        assert_eq!(
            no_store.unavailable_reason(),
            Some("no native subscription store is configured for this session")
        );
        let plakous_host_protocol::views::presentation::SubscriptionUsageView::Unavailable {
            reason,
        } = project_subscription_usage(NativeSubscriptionRefresh::NoSubscriptionStore)
        else {
            panic!("no store must project as unavailable");
        };
        assert!(!reason.contains("timed out"), "{reason}");

        let plakous_host_protocol::views::presentation::SubscriptionUsageView::Unavailable {
            reason,
        } = project_subscription_usage(NativeSubscriptionRefresh::TimedOut)
        else {
            panic!("a timed-out refresh must project as unavailable");
        };
        assert!(reason.contains("timed out"), "{reason}");
    }

    #[test]
    fn turn_result_usage_projection_preserves_counts_and_coverage() {
        let usage = match TurnUsage::new(plakous::model::Usage::new(21, 8, 5, 13, 0), 3, 2, 1) {
            Ok(usage) => usage,
            Err(error) => panic!("valid partial usage coverage rejected: {error}"),
        };
        assert_eq!(
            project_turn_usage(usage, None),
            HostEvent::TurnUsage {
                input_tokens: 21,
                output_tokens: 8,
                reasoning_tokens: 5,
                cached_input_tokens: 13,
                cache_write_tokens: 0,
                model_operations: Some(3),
                reporting_model_operations: Some(2),
                peak_prepared_input_tokens: None,
                terminal: true,
            }
        );
    }

    /// The peak is the largest single operation, never the running total.
    ///
    /// Measuring how far a turn grew against the window cannot use
    /// `input_tokens`: it sums over operations, so a many-round shell loop
    /// inflates it without any one prompt growing. A tool-growth turn
    /// recorded 534,462 summed tokens on a 400,000-token route
    /// while no single prompt came near the ceiling, and the gate correctly
    /// refused to read that as growth.
    #[test]
    fn the_prepared_input_peak_is_one_operations_prompt_not_the_running_sum() {
        let mut session = SessionRecord::new(std::env::temp_dir());
        let mut peak = 0_u64;
        for input in [12_000_u64, 31_500, 8_200, 27_000] {
            record_operation_prepared_input(
                &mut peak,
                &mut session,
                &plakous::model::UsageUpdate::new(plakous::model::Usage::new(input, 1, 0, 0, 0)),
                None,
            );
        }
        assert_eq!(peak, 31_500, "the peak is the largest single prompt");
        // A zero report is not an observation and must not move either.
        record_operation_prepared_input(
            &mut peak,
            &mut session,
            &plakous::model::UsageUpdate::new(plakous::model::Usage::new(0, 1, 0, 0, 0)),
            None,
        );
        assert_eq!(peak, 31_500);
    }

    /// The terminal observation carries the peak beside the provider sums.
    #[test]
    fn the_terminal_usage_projection_carries_the_growth_facts() {
        let usage = match TurnUsage::new(plakous::model::Usage::new(78_700, 8, 5, 13, 0), 4, 4, 1) {
            Ok(usage) => usage,
            Err(error) => panic!("valid usage coverage rejected: {error}"),
        };
        let HostEvent::TurnUsage {
            input_tokens,
            model_operations,
            peak_prepared_input_tokens,
            terminal,
            ..
        }: HostEvent = project_turn_usage(usage, Some(31_500))
        else {
            panic!("expected a turn-usage observation");
        };
        assert_eq!(input_tokens, 78_700, "the provider sum is unchanged");
        assert_eq!(model_operations, Some(4));
        assert_eq!(
            peak_prepared_input_tokens,
            Some(31_500),
            "one prompt's worth, which is what measures growth"
        );
        assert!(terminal);
    }

    /// The in-turn HUD applies the one Plakous formula: the held anchored
    /// measure, plus `bytes/4` of what streams, re-anchored on each provider
    /// report against the estimate of the request that report counted.
    #[test]
    fn live_occupancy_is_the_plakous_measure_advanced_through_the_turn() {
        let parts = plakous::model::ContextInputParts::measure_wire(br#"{"input":"x"}"#).unwrap();
        let projection = ContextProjection::test_fits(
            crate::context::PreparedInputCount::UpperBound(900_000),
            1_000_000,
            1_000_000,
            Some(parts),
        )
        .with_occupancy_measure(
            Some(851_715),
            plakous::model::ContextAnchor::new(298_468, 850_715),
        );
        assert_eq!(projection.occupied_tokens(), Some(299_468));
        let mut live = LiveOccupancy::from_projection(&projection);
        assert_eq!(live.tokens(), projection.occupied_tokens());
        live.appended(4_000);
        assert_eq!(live.tokens(), Some(300_468));
        live.round_started();
        assert_eq!(live.tokens(), Some(300_468));
        // The provider counted the request just sent; streamed text after
        // it is the only estimated suffix.
        live.provider_input(310_000);
        assert_eq!(live.tokens(), Some(310_000));
        live.appended(400);
        assert_eq!(live.tokens(), Some(310_100));
        live.provider_input(0);
        assert_eq!(live.tokens(), Some(310_100), "zero is not a report");
        live.round_started();
        live.compacted(Some(40_000), 90_000);
        assert_eq!(live.tokens(), Some(40_000));
        assert!(!live.anchored());
        live.provider_input(12_000);
        assert_eq!(live.tokens(), Some(12_000));
        assert!(live.anchored());

        let mut unavailable = LiveOccupancy::from_projection(&ContextProjection::unavailable());
        assert_eq!(unavailable.tokens(), None);
    }

    /// A long turn's rounds each publish a new request whose held projection
    /// still carries the turn-start anchor. The meter must stay on the newest
    /// provider count plus the estimated growth since, and never fall back to
    /// the older count between a round's request and its usage report.
    #[test]
    fn live_occupancy_is_monotone_across_rounds_of_one_turn() {
        let parts = plakous::model::ContextInputParts::measure_wire(br#"{"input":"x"}"#).unwrap();
        let base = ContextProjection::test_fits(
            crate::context::PreparedInputCount::UpperBound(900_000),
            1_000_000,
            1_000_000,
            Some(parts),
        );
        let turn_start_anchor = plakous::model::ContextAnchor::new(700_000, 700_000);
        let mut live = LiveOccupancy::from_projection(
            &base.with_occupancy_measure(Some(700_000), turn_start_anchor),
        );
        live.round_started();
        assert_eq!(live.tokens(), Some(700_000));
        let mut readings = Vec::new();
        for round in 1..=4_u64 {
            // The provider counts 5k more than `bytes/4` saw for each round.
            live.provider_input(700_000 + round * 15_000);
            readings.push(live.tokens().unwrap());
            live.appended(4_000);
            readings.push(live.tokens().unwrap());
            live = live.next_request(
                &base.with_occupancy_measure(Some(700_000 + round * 10_000), turn_start_anchor),
            );
            readings.push(live.tokens().unwrap());
            live.round_started();
            readings.push(live.tokens().unwrap());
        }
        assert!(
            readings.windows(2).all(|pair| pair[0] <= pair[1]),
            "meter fell within the turn: {readings:?}"
        );
        // The last request reads the newest count plus its estimated growth.
        assert_eq!(readings.last().copied(), Some(760_000 + 10_000));
    }

    /// A bound is not on the parts scale, so a provider count never anchors
    /// it: the HUD reads the bound plus the streamed suffix, unanchored, both
    /// for a parts-less projection and for a compaction with no measured
    /// successor.
    #[test]
    fn live_occupancy_never_anchors_a_bound() {
        let projection = ContextProjection::test_fits(
            crate::context::PreparedInputCount::UpperBound(900_000),
            1_000_000,
            1_000_000,
            None,
        )
        .with_occupied_tokens(Some(900_000));
        let mut live = LiveOccupancy::from_projection(&projection);
        live.round_started();
        live.provider_input(300_000);
        assert!(!live.anchored());
        assert_eq!(live.tokens(), Some(900_000));
        live.appended(400);
        assert_eq!(live.tokens(), Some(900_100));

        live.compacted(None, 50_000);
        live.round_started();
        live.provider_input(20_000);
        assert!(!live.anchored());
        assert_eq!(live.tokens(), Some(50_000));
    }

    #[test]
    fn two_live_usage_updates_refresh_context_before_turn_terminal() {
        let projection = ContextProjection::test_fits(
            crate::context::PreparedInputCount::UpperBound(8_000),
            100_000,
            100_000,
            None,
        );
        let session = SessionRecord::new(std::env::temp_dir());
        let (sender, receiver) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let events = Sender::over(sender);
        let mut views = Vec::new();
        for input_tokens in [15_000, 35_000] {
            emit_live_context_projection(
                &events,
                &projection,
                session.context_counters(),
                0,
                false,
                Some(input_tokens),
                false,
                &[],
            );
            // Usage is a coalesced view: consume each refresh as the UI does.
            views.extend(receiver.try_iter().filter_map(|event| match event {
                HostEvent::Usage(view) => Some(view),
                _ => None,
            }));
        }
        emit_live_context_projection(
            &events,
            &projection,
            session.context_counters(),
            0,
            true,
            None,
            false,
            &[],
        );
        assert!(receiver.try_iter().next().is_none());
        emit_live_context_projection(
            &events,
            &projection,
            session.context_counters(),
            0,
            true,
            Some(5_000),
            false,
            &[],
        );
        views.extend(receiver.try_iter().filter_map(|event| match event {
            HostEvent::Usage(view) => Some(view),
            _ => None,
        }));
        assert_eq!(
            views.len(),
            3,
            "no zero reading after compact without usage"
        );
        assert_eq!(views[0].breakdown.as_ref().unwrap().used_tokens, 15_000);
        assert_eq!(views[1].breakdown.as_ref().unwrap().used_tokens, 35_000);
        assert_eq!(views[0].display_percent, Some(15));
        assert_eq!(views[1].display_percent, Some(35));
        assert_eq!(views[1].total_window, Some(100_000));
        assert_eq!(views[2].display_percent, Some(5));
        assert_eq!(views[2].breakdown.as_ref().unwrap().used_tokens, 5_000);
        assert_eq!(views[2].breakdown.as_ref().unwrap().categories.len(), 1);
        assert_eq!(views[2].prepared_input, None);
    }

    #[test]
    fn context_breakdown_keeps_envelope_unattributed_under_an_anchored_measure() {
        let body = br#"{"model":"m","instructions":"core prompt\n\nrepo agents","tools":[],"input":[{"role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#;
        let parts = plakous::model::ContextInputParts::measure_wire(body).unwrap();
        let enrolled = plakous::prompts::instructions::inlined_instruction_source_bytes(
            "core prompt\n\nrepo agents",
            &["repo agents"],
        );
        let projection = ContextProjection::test_fits(
            crate::context::PreparedInputCount::UpperBound(20_000),
            200_000,
            200_000,
            Some(parts),
        );
        let estimate = projection.input_estimate_with_enrolled(enrolled).unwrap();
        let occupancy = estimate.occupancy_tokens();
        let projection = projection.with_occupied_tokens(Some(5_000));
        let breakdown =
            context_breakdown(&projection, sessions::ContextCounters::default(), enrolled).unwrap();
        assert_eq!(breakdown.used_tokens, 5_000);
        let unattributed = breakdown
            .categories
            .iter()
            .find(|category| category.label == UNATTRIBUTED_CATEGORY)
            .expect("envelope must occupy as unattributed");
        assert_eq!(unattributed.tokens, estimate.envelope_tokens);
        let observed = breakdown
            .categories
            .iter()
            .find(|category| category.label == PROVIDER_OBSERVED_CATEGORY)
            .expect("provider excess must not fold into envelope");
        assert_eq!(observed.tokens, 5_000 - occupancy);
        let attributed: u64 = breakdown
            .categories
            .iter()
            .map(|category| category.tokens)
            .sum();
        assert_eq!(attributed, breakdown.used_tokens);
    }

    #[test]
    fn prepared_request_updates_context_parts_and_splits_tool_names() {
        let old =
            plakous::model::ContextInputParts::measure_wire(br#"{"tools":[],"input":[]}"#).unwrap();
        let new = plakous::model::ContextInputParts::measure_wire(br#"{"tools":[{"name":"Bash"},{"name":"wait_agent"}],"input":[{"content":"a much newer request body"}]}"#).unwrap();
        let projection = ContextProjection::test_fits(
            crate::context::PreparedInputCount::UpperBound(20_000),
            200_000,
            200_000,
            Some(old),
        );
        let newer = projection.with_prepared_request(Some(new), Some(500), Some(700));
        let mut breakdown =
            context_breakdown(&newer, sessions::ContextCounters::default(), 0).unwrap();
        assert_eq!(breakdown.used_tokens, 500);
        assert!(
            breakdown
                .categories
                .iter()
                .any(|category| category.label == "2 tools on the last request; tokens est.")
        );
        append_request_tool_names(
            &mut breakdown,
            &["Bash".to_owned(), "wait_agent".to_owned()],
        );
        assert_eq!(
            breakdown
                .informational
                .iter()
                .find(|row| row.label.starts_with("Coding tools"))
                .unwrap()
                .detail
                .as_deref(),
            Some("Bash")
        );
        assert_eq!(
            breakdown
                .informational
                .iter()
                .find(|row| row.label.starts_with("Agent tools"))
                .unwrap()
                .detail
                .as_deref(),
            Some("wait_agent")
        );
    }

    #[test]
    fn tool_count_is_exact_and_only_tokens_are_estimated() {
        let body = br#"{"tools":[{"type":"function","name":"code"},{"type":"function","name":"wait_agent"}],"input":[]}"#;
        let parts = plakous::model::ContextInputParts::measure_wire(body).unwrap();
        let projection = ContextProjection::test_fits(
            crate::context::PreparedInputCount::UpperBound(20_000),
            200_000,
            200_000,
            Some(parts),
        );
        let breakdown =
            context_breakdown(&projection, sessions::ContextCounters::default(), 0).unwrap();
        assert!(
            breakdown
                .categories
                .iter()
                .any(|category| category.label == "2 tools on the last request; tokens est.")
        );
    }

    #[test]
    fn grok_envelope_names_an_eighty_percent_compact_fence() {
        let projection = ContextProjection::test_fits(
            crate::context::PreparedInputCount::UpperBound(223_000),
            500_000,
            500_000,
            None,
        )
        .with_auto_compact_input(Some(400_000));
        let breakdown =
            context_breakdown(&projection, sessions::ContextCounters::default(), 0).unwrap();
        assert_eq!(breakdown.auto_compact_trigger_tokens, Some(400_000));
        assert!(!breakdown.occupancy_anchored);
        let anchored = context_breakdown(
            &projection.with_anchored_occupied_tokens(450_000),
            sessions::ContextCounters::default(),
            0,
        )
        .unwrap();
        assert!(anchored.occupancy_anchored);
    }

    #[test]
    fn display_percent_is_the_compact_gauge_and_not_the_bound() {
        let body = br#"{"model":"m","instructions":"x","tools":[],"input":[{"role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#;
        let parts = plakous::model::ContextInputParts::measure_wire(body).unwrap();
        let projection = ContextProjection::test_fits(
            crate::context::PreparedInputCount::UpperBound(20_000),
            200_000,
            200_000,
            Some(parts),
        );
        let usage = context_usage(&projection, sessions::ContextCounters::default(), 0);
        let gauge = projection.actionable_utilization().unwrap().gauge_percent();
        assert_eq!(
            usage.display_percent,
            Some(u128::from(gauge)),
            "the composer and the compact gauge read one occupancy"
        );
        assert_eq!(
            usage.breakdown.unwrap().used_tokens,
            parts.occupancy_tokens(),
            "occupancy is the request's bytes÷4, not the admission bound"
        );
    }

    #[test]
    fn attached_one_shot_usage_reports_context_without_subscription_refresh() {
        let root = tempfile::tempdir().unwrap();
        let (mut config, _) = fresh_injected_attached_host(root.path());
        config.attached_one_shot = true;
        let events = run_attached_actions(
            config,
            vec![UserAction::Extension(ProbeIntent::InspectUsage)],
        );
        let statuses = usage_statuses(&events);
        assert!(
            !statuses.is_empty() && statuses.iter().all(|status| *status == "fits"),
            "attached usage must report fits, got {statuses:?}"
        );
        assert!(events.iter().all(|event| !matches!(
            event,
            HostEvent::SubscriptionUsage(_) | HostEvent::Extension(ProbeObservation::UsageObserved)
        )));
    }

    #[test]
    fn subscription_refresh_coalesces_bursts_into_one_in_flight_read() {
        let (tx, rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let events = Sender::over(tx);
        let calls = Arc::new(AtomicUsize::new(0));
        let worker_calls = Arc::clone(&calls);
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let refresh = SubscriptionUsageRefresh::new_with_projector(&events, move |_| {
            let call = worker_calls.fetch_add(1, Ordering::AcqRel);
            if call == 0 {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            }
            (
                plakous_host_protocol::views::presentation::SubscriptionUsageView::Unavailable {
                    reason: format!("test observation {call}"),
                },
                None,
            )
        });

        refresh.request(None);
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        for _ in 0..20 {
            refresh.request(None);
        }
        release_tx.send(()).unwrap();
        drop(refresh);

        assert_eq!(calls.load(Ordering::Acquire), 1);
        let terminal = rx
            .try_iter()
            .filter(|event| {
                matches!(
                    event,
                    HostEvent::SubscriptionUsage(
                        plakous_host_protocol::views::presentation::SubscriptionUsageView::Unavailable { .. }
                    )
                )
            })
            .count();
        assert_eq!(
            terminal, 1,
            "latest terminal observation replaces older state"
        );
    }

    #[test]
    fn explicit_post_credential_read_is_queued_behind_an_older_read() {
        let (tx, _rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let events = Sender::over(tx);
        let calls = Arc::new(AtomicUsize::new(0));
        let worker_calls = Arc::clone(&calls);
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let refresh = SubscriptionUsageRefresh::new_with_projector(&events, move |_| {
            if worker_calls.fetch_add(1, Ordering::AcqRel) == 0 {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            }
            (
                plakous_host_protocol::views::presentation::SubscriptionUsageView::Read,
                None,
            )
        });
        refresh.request(None);
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        for _ in 0..20 {
            refresh.request_fresh(None);
        }
        release_tx.send(()).unwrap();
        drop(refresh);
        assert_eq!(calls.load(Ordering::Acquire), 2);
    }

    /// A surface reading subscription posture waits out the running read and
    /// the explicit follow-up queued behind it, so it never holds a
    /// compatibility lock the read only try-locks.
    #[test]
    fn usage_reads_settle_only_after_the_running_and_queued_reads() {
        let (tx, _rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let events = Sender::over(tx);
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(2);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(2);
        let refresh = SubscriptionUsageRefresh::new_with_projector(&events, move |_| {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            (
                plakous_host_protocol::views::presentation::SubscriptionUsageView::Read,
                None,
            )
        });
        let reads = refresh.reads();
        assert!(reads.wait_settled(Duration::ZERO), "nothing was requested");
        refresh.request(None);
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        refresh.request_fresh(None);
        refresh.request(None);
        assert!(!reads.wait_settled(Duration::from_millis(20)));
        release_tx.send(()).unwrap();
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(
            !reads.wait_settled(Duration::from_millis(20)),
            "the queued follow-up read is still running"
        );
        release_tx.send(()).unwrap();
        assert!(reads.wait_settled(Duration::from_secs(5)));
    }

    #[test]
    fn idle_usage_serves_a_fresh_held_projection_without_another_read() {
        let (tx, rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let events = Sender::over(tx);
        let calls = Arc::new(AtomicUsize::new(0));
        let worker_calls = Arc::clone(&calls);
        let held = HeldUsage::default();
        let ledger = SessionLedger::default();
        let refresh = SubscriptionUsageRefresh::new_with_projector(&events, move |_| {
            worker_calls.fetch_add(1, Ordering::AcqRel);
            (
                plakous_host_protocol::views::presentation::SubscriptionUsageView::Read,
                Some(crate::subscription_usage::project_limits(
                    [],
                    plakous_host_protocol::views::presentation::SessionLedgerView::default(),
                )),
            )
        });
        let mut view = crate::subscription_usage::project_limits(
            [],
            plakous_host_protocol::views::presentation::SessionLedgerView::default(),
        );
        view.verticals[2].state =
            plakous_host_protocol::views::presentation::VerticalUsageState::Ready {
                meters: Vec::new(),
            };
        held.store(&view);

        request_subscription_usage_cached(&held, &ledger, &events, &refresh, None);
        let first: Vec<_> = rx.try_iter().collect();
        request_subscription_usage_cached(&held, &ledger, &events, &refresh, None);
        let second: Vec<_> = rx.try_iter().collect();
        thread::sleep(Duration::from_millis(50));
        drop(refresh);

        assert_eq!(
            calls.load(Ordering::Acquire),
            0,
            "a fresh held projection must not start another provider read"
        );
        for batch in [&first, &second] {
            let served = batch.iter().find_map(|event| match event {
                HostEvent::UsageLimits(view) => Some(view),
                _ => None,
            });
            let Some(served) = served else {
                panic!("cached projection is published as UsageLimits: {batch:?}");
            };
            assert_eq!(served.stale_age_seconds, None);
            assert!(matches!(
                served.verticals[2].state,
                plakous_host_protocol::views::presentation::VerticalUsageState::Ready { .. }
            ));
            assert!(batch.iter().all(|event| !matches!(
                event,
                HostEvent::SubscriptionUsage(
                    plakous_host_protocol::views::presentation::SubscriptionUsageView::Loading
                )
            )));
            assert!(batch.iter().any(|event| matches!(
                event,
                HostEvent::SubscriptionUsage(
                    plakous_host_protocol::views::presentation::SubscriptionUsageView::Read
                )
            )));
        }
    }

    #[test]
    fn idle_usage_reads_when_nothing_is_held() {
        let (tx, rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let events = Sender::over(tx);
        let calls = Arc::new(AtomicUsize::new(0));
        let worker_calls = Arc::clone(&calls);
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let held = HeldUsage::default();
        let ledger = SessionLedger::default();
        let refresh = SubscriptionUsageRefresh::new_with_projector(&events, move |_| {
            worker_calls.fetch_add(1, Ordering::AcqRel);
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            (
                plakous_host_protocol::views::presentation::SubscriptionUsageView::Read,
                Some(crate::subscription_usage::project_limits(
                    [],
                    plakous_host_protocol::views::presentation::SessionLedgerView::default(),
                )),
            )
        });

        request_subscription_usage_cached(&held, &ledger, &events, &refresh, None);
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let during: Vec<_> = rx.try_iter().collect();
        release_tx.send(()).unwrap();
        drop(refresh);

        assert_eq!(calls.load(Ordering::Acquire), 1);
        assert!(
            during.iter().any(|event| matches!(
                event,
                HostEvent::SubscriptionUsage(
                    plakous_host_protocol::views::presentation::SubscriptionUsageView::Loading
                )
            )),
            "a cache miss must start a provider read: {during:?}"
        );
    }

    #[test]
    fn a_rate_limited_claude_vertical_is_stored_as_the_last_ready_meters() {
        let held = HeldUsage::default();
        let mut ready = crate::subscription_usage::project_limits(
            [],
            plakous_host_protocol::views::presentation::SessionLedgerView::default(),
        );
        ready.verticals[2].observed_at = Some("2026-09-04 12:00".to_owned());
        ready.verticals[2].state =
            plakous_host_protocol::views::presentation::VerticalUsageState::Ready {
                meters: Vec::new(),
            };
        held.store(&ready);
        let observed = Instant::now()
            .checked_sub(Duration::from_secs(120))
            .unwrap();
        held.inner.lock().unwrap().as_mut().unwrap().1 = observed;

        let mut incoming = crate::subscription_usage::project_limits(
            [],
            plakous_host_protocol::views::presentation::SessionLedgerView::default(),
        );
        incoming.verticals[2].state =
            plakous_host_protocol::views::presentation::VerticalUsageState::Unavailable {
                reason: crate::subscription_usage::USAGE_RATE_LIMITED_REASON.to_owned(),
            };
        let merged = held.preserve_and_store(incoming);
        assert_eq!(merged.verticals[2], ready.verticals[2]);
        assert!(held.fresh().is_none(), "a 429 must not renew freshness");
        assert_eq!(held.inner.lock().unwrap().as_ref().unwrap().1, observed);
    }

    #[test]
    fn a_held_busy_projection_starts_another_read() {
        let (tx, rx) = crate::surface_protocol::host_event_channel();
        let events = Sender::over(tx);
        let calls = Arc::new(AtomicUsize::new(0));
        let worker_calls = Arc::clone(&calls);
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let held = HeldUsage::default();
        let ledger = SessionLedger::default();
        let refresh = SubscriptionUsageRefresh::new_with_projector(&events, move |_| {
            worker_calls.fetch_add(1, Ordering::AcqRel);
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            (
                plakous_host_protocol::views::presentation::SubscriptionUsageView::Read,
                Some(plakous_host_protocol::views::presentation::UsageLimitsView::default()),
            )
        });
        let mut view = crate::subscription_usage::project_limits(
            [],
            plakous_host_protocol::views::presentation::SessionLedgerView::default(),
        );
        for vertical in &mut view.verticals {
            vertical.state =
                plakous_host_protocol::views::presentation::VerticalUsageState::Unavailable {
                    reason: crate::subscription_usage::USAGE_BUSY_REASON.to_owned(),
                };
        }
        held.store(&view);

        request_subscription_usage_cached(&held, &ledger, &events, &refresh, None);
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        release_tx.send(()).unwrap();
        drop(refresh);
        let _ = rx.try_iter().count();
        assert_eq!(
            calls.load(Ordering::Acquire),
            1,
            "a Busy-only cache must not block the next idle Usage open"
        );
    }

    #[test]
    fn busy_usage_reads_are_retried_until_the_lock_clears() {
        let executor = ApplicationExecutor::new().unwrap();
        let attempts = AtomicUsize::new(0);
        let Ok(read) = executor.block_on(await_usage_not_busy(|| {
            let n = attempts.fetch_add(1, Ordering::SeqCst);
            async move {
                if n < 2 {
                    plakous::model::SubscriptionUsageRead::Unavailable {
                        observed_at: plakous::model::Timestamp::new(0, 0)
                            .unwrap_or_else(|_| unreachable!("zero is a valid timestamp")),
                        reason: plakous::model::SubscriptionUsageUnavailable::Busy,
                    }
                } else {
                    plakous::model::SubscriptionUsageRead::Unavailable {
                        observed_at: plakous::model::Timestamp::new(0, 0)
                            .unwrap_or_else(|_| unreachable!("zero is a valid timestamp")),
                        reason: plakous::model::SubscriptionUsageUnavailable::LocalState,
                    }
                }
            }
        })) else {
            panic!("executor is live");
        };
        assert!(matches!(
            read,
            plakous::model::SubscriptionUsageRead::Unavailable {
                reason: plakous::model::SubscriptionUsageUnavailable::LocalState,
                ..
            }
        ));
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn busy_store_fence_read_is_retried_after_writer_lease_clears()
    -> Result<(), Box<dyn std::error::Error>> {
        use plakous::auth::{
            AccountBinding, CredentialGeneration, CredentialRecord, CredentialSlotId,
            CredentialStatus, PrivateCredentialStore, SecretBytes,
            SubscriptionIntegrationProfileId,
        };
        use plakous::model::{SubscriptionUsageRead, SubscriptionUsageUnavailable, Timestamp};
        use std::os::unix::fs::PermissionsExt as _;
        let directory = tempfile::tempdir()?;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))?;
        let profile = SubscriptionIntegrationProfileId::new("usage-test-profile")?;
        let account = AccountBinding::new("usage-test-account")?;
        let store = PrivateCredentialStore::open_slot(
            directory.path().join("credential.json"),
            profile.clone(),
            CredentialSlotId::new("usage-test-slot")?,
        )?;
        store.initialize_selected(&CredentialRecord::new(
            profile,
            account,
            CredentialGeneration::new(1)?,
            CredentialStatus::Ready,
            SecretBytes::new(b"offline-fixture".to_vec()),
        )?)?;
        assert!(store.try_load()?.is_some());
        let file = std::fs::File::open(store.lock_path())?;
        file.lock()?;
        let mut writer = Some(file);
        let mut attempts = 0;
        let executor = ApplicationExecutor::new()?;
        let read = executor.block_on(await_usage_not_busy(|| {
            attempts += 1;
            let reason = match store.try_load() {
                Err(_) if writer.is_some() => SubscriptionUsageUnavailable::Busy,
                Ok(Some(fence)) => {
                    assert_eq!(fence.status(), CredentialStatus::Ready);
                    // The next control-plane step, not a local-state refusal.
                    SubscriptionUsageUnavailable::ProviderUnavailable
                }
                other => panic!("unexpected fence read: {other:?}"),
            };
            if attempts == 2 {
                drop(writer.take());
            }
            async move {
                SubscriptionUsageRead::Unavailable {
                    observed_at: Timestamp::new(0, 0).unwrap(),
                    reason,
                }
            }
        }))?;
        assert!(matches!(
            read,
            SubscriptionUsageRead::Unavailable {
                reason: SubscriptionUsageUnavailable::ProviderUnavailable,
                ..
            }
        ));
        assert_eq!(attempts, 3);
        Ok(())
    }

    #[test]
    fn subscription_refresh_timeout_has_a_terminal_outcome() {
        let executor = ApplicationExecutor::new().unwrap();
        let started = Instant::now();
        let outcome = executor
            .block_on(bounded_subscription_refresh(
                Duration::from_millis(20),
                std::future::pending::<()>(),
            ))
            .unwrap();
        assert_eq!(outcome, None);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    /// Quit must never wait out an in-flight subscription-usage read: the
    /// worker owns no durable state, so teardown is bounded and detaches.
    #[test]
    fn dropping_the_usage_refresh_is_bounded_even_while_a_read_is_running() {
        let (tx, _rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let events = Sender::over(tx);
        let refresh = SubscriptionUsageRefresh::new_with_projector(&events, |_| {
            thread::sleep(Duration::from_secs(10));
            (
                plakous_host_protocol::views::presentation::SubscriptionUsageView::Unavailable {
                    reason: "stuck".to_owned(),
                },
                None,
            )
        });
        refresh.request(None);
        // Let the worker pick the request up and park inside the projector.
        thread::sleep(Duration::from_millis(50));
        let started = Instant::now();
        drop(refresh);
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "teardown waited {elapsed:?} on a usage read"
        );
    }

    #[test]
    fn round_boundary_estimate_advances_without_becoming_provider_usage() {
        let projection = ContextProjection::test_fits(
            crate::context::PreparedInputCount::UpperBound(8_000),
            100_000,
            100_000,
            None,
        );
        let session = SessionRecord::new(std::env::temp_dir());
        let (sender, receiver) = crate::surface_protocol::host_event_channel::<TestExtension>();
        emit_live_context_projection(
            &Sender::over(sender),
            &projection,
            session.context_counters(),
            0,
            false,
            Some(12_000),
            false,
            &[],
        );

        let HostEvent::Usage(view) = receiver.try_iter().next().unwrap() else {
            panic!("expected context usage");
        };
        assert_eq!(view.display_percent, Some(12));
        let breakdown = view.breakdown.unwrap();
        assert_eq!(breakdown.used_tokens, 12_000);
        assert!(breakdown.categories.iter().any(|category| {
            category.label == "Live turn growth (est.)" && category.tokens == 4_000
        }));
    }

    #[test]
    fn post_compact_growth_does_not_freeze_at_an_earlier_provider_reading() {
        let projection = ContextProjection::test_fits(
            crate::context::PreparedInputCount::UpperBound(8_000),
            100_000,
            100_000,
            None,
        );
        let session = SessionRecord::new(std::env::temp_dir());
        let (sender, receiver) = crate::surface_protocol::host_event_channel::<TestExtension>();
        emit_live_context_projection(
            &Sender::over(sender),
            &projection,
            session.context_counters(),
            0,
            true,
            Some(13_000),
            false,
            &[],
        );
        let HostEvent::Usage(view) = receiver.try_iter().next().unwrap() else {
            panic!("expected context usage");
        };
        assert_eq!(view.display_percent, Some(13));
        let breakdown = view.breakdown.unwrap();
        assert_eq!(breakdown.used_tokens, 13_000);
        assert_eq!(breakdown.categories[0].label, "Live turn growth (est.)");
    }
}
