//! State-driven redraw cadence for the TUI event loop.
//!
//! The frame timer is armed by demand, not by a fixed tick. This module is
//! pure: it maps the renderer's current demand onto one cadence state, the
//! wait that state implies, and the reason a frame was painted. The event
//! loop owns the clock and the terminal; nothing here reads either, so every
//! transition is unit-testable.
//!
//! Deep idle returns [`RedrawWait::Park`]: no timer is required and a frame
//! must not be painted. The synchronous loop has exactly one wait point
//! (`crossterm::event::poll`), which cannot also wait on the host queue, so it
//! clamps a park to [`PARK_POLL_CEILING`]. That ceiling exists only to notice
//! an unsolicited host observation while the session is idle; it never paints
//! on its own, because [`RedrawState::periodic_redraw_required`] is false for
//! both idle states.

use std::time::Duration;

/// Default streaming frame cap. 60 fps lands inside the 16–33 ms coalescing
/// band the spec requires for stream deltas.
pub(crate) const DEFAULT_FRAME_CAP_FPS: u32 = 60;

/// Cadence while a slow animation (a fading activity wave, an armed key
/// timer) is the only thing outstanding.
pub(crate) const IDLE_INTERVAL: Duration = Duration::from_millis(250);

/// Cadence while a spinner glyph is on screen.
pub(crate) const SPINNER_INTERVAL: Duration = Duration::from_millis(80);

/// Cadence while a coalesced wheel queue drains into scroll offsets.
pub(crate) const WHEEL_DRAIN_INTERVAL: Duration = Duration::from_millis(8);

/// Longest a parked loop may block on stdin before re-checking the host
/// queue. Not a redraw cadence: a park tick paints nothing.
pub(crate) const PARK_POLL_CEILING: Duration = Duration::from_millis(250);

/// What the renderer currently needs from the clock.
///
/// Every field is O(1) to compute from `App` state; the event loop must never
/// walk the transcript to answer one.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "an independent demand flag per animation source, not a state machine"
)]
pub(crate) struct RedrawDemand {
    /// An assistant stream is open and deltas are landing.
    pub(crate) streaming: bool,
    /// A drag-select pointer is auto-scrolling the transcript.
    pub(crate) pointer_drag: bool,
    /// A spinner glyph is visible (activity wave, running tool, session open).
    pub(crate) spinner_visible: bool,
    /// A slow animation still needs frames (for example a fading wave).
    pub(crate) animating: bool,
    /// A key-repeat or confirmation timer will expire on its own.
    pub(crate) timer_armed: bool,
    /// A coalesced mouse-wheel queue is still draining.
    pub(crate) wheel_draining: bool,
}

impl RedrawDemand {
    /// Nothing animates, nothing streams, no timer is armed.
    #[must_use]
    pub(crate) const fn is_deep_idle(self) -> bool {
        !self.streaming
            && !self.pointer_drag
            && !self.spinner_visible
            && !self.animating
            && !self.timer_armed
            && !self.wheel_draining
    }
}

/// The cadence state one demand resolves to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RedrawState {
    /// No timer at all: park until input or a host observation arrives.
    DeepIdle,
    /// Something animates slowly; a tick may paint nothing.
    Idle,
    /// A spinner glyph advances.
    Spinner,
    /// Frame-capped: stream deltas or pointer drag.
    Streaming,
    /// A wheel queue drains faster than a frame.
    WheelDrain,
}

impl RedrawState {
    /// Whether a tick in this state is expected to paint.
    ///
    /// False for both idle states: the tick exists to observe, not to repaint.
    #[must_use]
    pub(crate) const fn periodic_redraw_required(self) -> bool {
        match self {
            Self::DeepIdle | Self::Idle => false,
            Self::Spinner | Self::Streaming | Self::WheelDrain => true,
        }
    }

    /// Stable name for tests and telemetry.
    #[must_use]
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::DeepIdle => "deep-idle",
            Self::Idle => "idle",
            Self::Spinner => "spinner",
            Self::Streaming => "streaming",
            Self::WheelDrain => "wheel-drain",
        }
    }
}

/// How long the loop waits before the next iteration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RedrawWait {
    /// No timer is required.
    Park,
    /// Wait at most this long.
    Until(Duration),
}

impl RedrawWait {
    /// The wait a synchronous single-wait-point loop actually uses.
    #[must_use]
    pub(crate) const fn bounded(self, park_ceiling: Duration) -> Duration {
        match self {
            Self::Park => park_ceiling,
            Self::Until(interval) => interval,
        }
    }
}

/// Why a frame was painted. `Nothing` records a tick that painted nothing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RedrawReason {
    /// Input, a host state change, or a layout change marked the view dirty.
    StateChange,
    /// Coalesced host observations landed in this frame.
    HostBatch,
    /// Stream deltas were appended to the tail block in this frame.
    Stream,
    /// A spinner glyph advanced.
    Spinner,
    /// A slow animation advanced.
    Animation,
    /// A wheel queue drained into a scroll offset.
    WheelDrain,
    /// The tick fired and there was nothing to paint.
    Nothing,
}

impl RedrawReason {
    /// Whether this reason paints a frame.
    #[must_use]
    pub(crate) const fn paints(self) -> bool {
        !matches!(self, Self::Nothing)
    }

    /// Stable name for tests and telemetry.
    #[must_use]
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::StateChange => "state-change",
            Self::HostBatch => "host-batch",
            Self::Stream => "stream",
            Self::Spinner => "spinner",
            Self::Animation => "animation",
            Self::WheelDrain => "wheel-drain",
            Self::Nothing => "nothing",
        }
    }
}

/// What actually happened in one loop iteration, before the draw decision.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "an independent record per work source; the reason table collapses them"
)]
pub(crate) struct FrameWork {
    /// The view was marked dirty by input or a state change.
    pub(crate) dirty: bool,
    /// At least one host observation was applied.
    pub(crate) host_batch: bool,
    /// At least one assistant delta was appended.
    pub(crate) stream_delta: bool,
    /// A spinner glyph advanced.
    pub(crate) spinner_advanced: bool,
    /// A slow animation advanced.
    pub(crate) animation_advanced: bool,
    /// A wheel notch was applied to the scroll offset.
    pub(crate) wheel_drained: bool,
}

/// The redraw cadence policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RedrawSchedule {
    frame_interval: Duration,
}

impl Default for RedrawSchedule {
    fn default() -> Self {
        Self::with_frame_cap(DEFAULT_FRAME_CAP_FPS)
    }
}

impl RedrawSchedule {
    /// Build a schedule with an explicit streaming frame cap in frames/second.
    ///
    /// A cap of zero is refused and clamped to one frame per second, so the
    /// streaming state always has a real interval.
    #[must_use]
    pub(crate) const fn with_frame_cap(fps: u32) -> Self {
        let fps = if fps == 0 { 1 } else { fps };
        Self {
            frame_interval: Duration::from_nanos(1_000_000_000 / fps as u64),
        }
    }

    /// The streaming frame interval this schedule enforces.
    #[must_use]
    pub(crate) const fn frame_interval(self) -> Duration {
        self.frame_interval
    }

    /// Resolve a demand to exactly one cadence state.
    ///
    /// Ordered shortest-interval first, so the most urgent live demand wins.
    #[must_use]
    pub(crate) const fn state(demand: RedrawDemand) -> RedrawState {
        if demand.wheel_draining {
            RedrawState::WheelDrain
        } else if demand.streaming || demand.pointer_drag {
            RedrawState::Streaming
        } else if demand.spinner_visible {
            RedrawState::Spinner
        } else if demand.animating || demand.timer_armed {
            RedrawState::Idle
        } else {
            RedrawState::DeepIdle
        }
    }

    /// The interval a cadence state ticks at.
    #[must_use]
    pub(crate) const fn interval(self, state: RedrawState) -> RedrawWait {
        match state {
            RedrawState::DeepIdle => RedrawWait::Park,
            RedrawState::Idle => RedrawWait::Until(IDLE_INTERVAL),
            RedrawState::Spinner => RedrawWait::Until(SPINNER_INTERVAL),
            RedrawState::Streaming => RedrawWait::Until(self.frame_interval),
            RedrawState::WheelDrain => RedrawWait::Until(WHEEL_DRAIN_INTERVAL),
        }
    }

    /// The wait one demand implies.
    #[must_use]
    pub(crate) const fn wait(self, demand: RedrawDemand) -> RedrawWait {
        self.interval(Self::state(demand))
    }

    /// Whether a tick under this demand is expected to paint.
    #[must_use]
    pub(crate) const fn periodic_redraw_required(demand: RedrawDemand) -> bool {
        Self::state(demand).periodic_redraw_required()
    }

    /// Why this frame paints, or [`RedrawReason::Nothing`] when it must not.
    ///
    /// Ordered by what a reader most wants to see attributed: an explicit
    /// state change outranks the batch that carried it, and both outrank the
    /// animation that would have ticked anyway.
    #[must_use]
    pub(crate) const fn reason(work: FrameWork) -> RedrawReason {
        if work.dirty {
            RedrawReason::StateChange
        } else if work.stream_delta {
            RedrawReason::Stream
        } else if work.host_batch {
            RedrawReason::HostBatch
        } else if work.wheel_drained {
            RedrawReason::WheelDrain
        } else if work.spinner_advanced {
            RedrawReason::Spinner
        } else if work.animation_advanced {
            RedrawReason::Animation
        } else {
            RedrawReason::Nothing
        }
    }
}

/// The full cadence table: state, tick interval, and whether a tick paints.
///
/// Exposed so a test can assert the published schedule rather than restate it.
/// [`RedrawState::Streaming`] is absent because its interval is the
/// per-instance frame cap rather than a constant.
pub(crate) const SCHEDULE_TABLE: &[(RedrawState, RedrawWait, bool)] = &[
    (RedrawState::DeepIdle, RedrawWait::Park, false),
    (RedrawState::Idle, RedrawWait::Until(IDLE_INTERVAL), false),
    (
        RedrawState::Spinner,
        RedrawWait::Until(SPINNER_INTERVAL),
        true,
    ),
    (
        RedrawState::WheelDrain,
        RedrawWait::Until(WHEEL_DRAIN_INTERVAL),
        true,
    ),
];

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_FRAME_CAP_FPS, FrameWork, IDLE_INTERVAL, PARK_POLL_CEILING, RedrawDemand,
        RedrawReason, RedrawSchedule, RedrawState, RedrawWait, SCHEDULE_TABLE, SPINNER_INTERVAL,
        WHEEL_DRAIN_INTERVAL,
    };
    use std::time::Duration;

    fn schedule() -> RedrawSchedule {
        RedrawSchedule::default()
    }

    #[test]
    fn default_frame_cap_lands_in_the_coalescing_band() {
        let interval = schedule().frame_interval();
        assert_eq!(DEFAULT_FRAME_CAP_FPS, 60);
        assert!(
            interval >= Duration::from_millis(16) && interval <= Duration::from_millis(33),
            "streaming cadence {interval:?} left the 16-33 ms band"
        );
    }

    #[test]
    fn frame_cap_is_configurable_and_never_zero() {
        assert_eq!(
            RedrawSchedule::with_frame_cap(30).frame_interval(),
            Duration::from_nanos(33_333_333)
        );
        assert_eq!(
            RedrawSchedule::with_frame_cap(0).frame_interval(),
            Duration::from_secs(1)
        );
    }

    #[test]
    fn nothing_outstanding_parks_with_no_timer() {
        let demand = RedrawDemand::default();
        assert!(demand.is_deep_idle());
        assert_eq!(RedrawSchedule::state(demand), RedrawState::DeepIdle);
        assert_eq!(schedule().wait(demand), RedrawWait::Park);
        assert!(!RedrawSchedule::periodic_redraw_required(demand));
    }

    #[test]
    fn a_parked_wait_is_bounded_only_by_the_ceiling() {
        assert_eq!(
            RedrawWait::Park.bounded(PARK_POLL_CEILING),
            PARK_POLL_CEILING
        );
        assert_eq!(
            RedrawWait::Until(SPINNER_INTERVAL).bounded(PARK_POLL_CEILING),
            SPINNER_INTERVAL
        );
    }

    #[test]
    fn an_animation_alone_ticks_at_the_idle_interval() {
        let demand = RedrawDemand {
            animating: true,
            ..RedrawDemand::default()
        };
        assert!(!demand.is_deep_idle());
        assert_eq!(RedrawSchedule::state(demand), RedrawState::Idle);
        assert_eq!(schedule().wait(demand), RedrawWait::Until(IDLE_INTERVAL));
        assert!(!RedrawSchedule::periodic_redraw_required(demand));
    }

    #[test]
    fn an_armed_timer_alone_ticks_at_the_idle_interval() {
        let demand = RedrawDemand {
            timer_armed: true,
            ..RedrawDemand::default()
        };
        assert_eq!(RedrawSchedule::state(demand), RedrawState::Idle);
        assert!(!RedrawSchedule::periodic_redraw_required(demand));
    }

    #[test]
    fn a_visible_spinner_ticks_at_the_spinner_interval() {
        let demand = RedrawDemand {
            spinner_visible: true,
            ..RedrawDemand::default()
        };
        assert_eq!(RedrawSchedule::state(demand), RedrawState::Spinner);
        assert_eq!(schedule().wait(demand), RedrawWait::Until(SPINNER_INTERVAL));
        assert!(RedrawSchedule::periodic_redraw_required(demand));
    }

    #[test]
    fn streaming_ticks_at_the_frame_cap() {
        let demand = RedrawDemand {
            streaming: true,
            spinner_visible: true,
            animating: true,
            ..RedrawDemand::default()
        };
        assert_eq!(RedrawSchedule::state(demand), RedrawState::Streaming);
        assert_eq!(
            schedule().wait(demand),
            RedrawWait::Until(schedule().frame_interval())
        );
        assert!(RedrawSchedule::periodic_redraw_required(demand));
    }

    #[test]
    fn pointer_drag_is_frame_paced() {
        let demand = RedrawDemand {
            pointer_drag: true,
            ..RedrawDemand::default()
        };
        assert_eq!(RedrawSchedule::state(demand), RedrawState::Streaming);
    }

    #[test]
    fn a_draining_wheel_outranks_every_other_demand() {
        let demand = RedrawDemand {
            streaming: true,
            pointer_drag: true,
            spinner_visible: true,
            animating: true,
            timer_armed: true,
            wheel_draining: true,
        };
        assert_eq!(RedrawSchedule::state(demand), RedrawState::WheelDrain);
        assert_eq!(
            schedule().wait(demand),
            RedrawWait::Until(WHEEL_DRAIN_INTERVAL)
        );
    }

    #[test]
    fn streaming_outranks_spinner_and_idle() {
        let streaming_over_spinner = RedrawDemand {
            streaming: true,
            spinner_visible: true,
            ..RedrawDemand::default()
        };
        assert_eq!(
            RedrawSchedule::state(streaming_over_spinner),
            RedrawState::Streaming
        );
        let spinner_over_idle = RedrawDemand {
            spinner_visible: true,
            animating: true,
            timer_armed: true,
            ..RedrawDemand::default()
        };
        assert_eq!(
            RedrawSchedule::state(spinner_over_idle),
            RedrawState::Spinner
        );
    }

    #[test]
    fn every_state_names_itself() {
        for (state, _, _) in SCHEDULE_TABLE {
            assert_ne!(state.name(), "");
        }
        assert_eq!(RedrawState::Streaming.name(), "streaming");
    }

    #[test]
    fn the_published_table_matches_the_policy() {
        let schedule = schedule();
        for (state, wait, paints) in SCHEDULE_TABLE {
            assert_eq!(schedule.interval(*state), *wait, "{}", state.name());
            assert_eq!(
                state.periodic_redraw_required(),
                *paints,
                "{}",
                state.name()
            );
        }
    }

    #[test]
    fn an_empty_frame_paints_nothing() {
        let reason = RedrawSchedule::reason(FrameWork::default());
        assert_eq!(reason, RedrawReason::Nothing);
        assert!(!reason.paints());
        assert_eq!(reason.name(), "nothing");
    }

    #[test]
    fn frame_reasons_are_ordered_by_attribution() {
        let all = FrameWork {
            dirty: true,
            host_batch: true,
            stream_delta: true,
            spinner_advanced: true,
            animation_advanced: true,
            wheel_drained: true,
        };
        assert_eq!(RedrawSchedule::reason(all), RedrawReason::StateChange);
        assert_eq!(
            RedrawSchedule::reason(FrameWork {
                dirty: false,
                ..all
            }),
            RedrawReason::Stream
        );
        assert_eq!(
            RedrawSchedule::reason(FrameWork {
                dirty: false,
                stream_delta: false,
                ..all
            }),
            RedrawReason::HostBatch
        );
        assert_eq!(
            RedrawSchedule::reason(FrameWork {
                dirty: false,
                stream_delta: false,
                host_batch: false,
                ..all
            }),
            RedrawReason::WheelDrain
        );
        assert_eq!(
            RedrawSchedule::reason(FrameWork {
                dirty: false,
                stream_delta: false,
                host_batch: false,
                wheel_drained: false,
                ..all
            }),
            RedrawReason::Spinner
        );
        assert_eq!(
            RedrawSchedule::reason(FrameWork {
                animation_advanced: true,
                ..FrameWork::default()
            }),
            RedrawReason::Animation
        );
    }

    #[test]
    fn every_painting_reason_reports_that_it_paints() {
        for reason in [
            RedrawReason::StateChange,
            RedrawReason::HostBatch,
            RedrawReason::Stream,
            RedrawReason::Spinner,
            RedrawReason::Animation,
            RedrawReason::WheelDrain,
        ] {
            assert!(reason.paints(), "{}", reason.name());
        }
    }

    #[test]
    fn leaving_a_live_state_returns_to_deep_idle() {
        let live = RedrawDemand {
            streaming: true,
            spinner_visible: true,
            wheel_draining: true,
            ..RedrawDemand::default()
        };
        assert_eq!(RedrawSchedule::state(live), RedrawState::WheelDrain);
        let settled = RedrawDemand {
            wheel_draining: false,
            ..live
        };
        assert_eq!(RedrawSchedule::state(settled), RedrawState::Streaming);
        let done = RedrawDemand {
            streaming: false,
            ..settled
        };
        assert_eq!(RedrawSchedule::state(done), RedrawState::Spinner);
        assert_eq!(
            RedrawSchedule::state(RedrawDemand::default()),
            RedrawState::DeepIdle
        );
    }
}
