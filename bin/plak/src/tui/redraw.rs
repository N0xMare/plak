//! State-driven redraw cadence for the TUI event loop.
//!
//! The frame timer is armed by demand, not by a fixed tick. This module is
//! pure: it maps the renderer's current demand onto one cadence state, the
//! wait that state implies, and the reason a frame was painted. The event
//! loop owns the clock and the terminal; nothing here reads either, so every
//! transition is unit-testable.
//!
//! Deep idle returns [`RedrawWait::Park`]: no timer is required and a frame
//! must not be painted. The loop's one wait point (`tui::wake::wait`) wakes on
//! terminal input, a host observation or a signal, so a park has no bound
//! except a one-shot deadline the application armed itself.

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
    /// The bound of the loop's wait: none while parked, and the cadence
    /// interval otherwise, either cut short by an armed one-shot `deadline`.
    #[must_use]
    pub(crate) fn bound(self, deadline: Option<Duration>) -> Option<Duration> {
        match (self, deadline) {
            (Self::Park, deadline) => deadline,
            (Self::Until(interval), None) => Some(interval),
            (Self::Until(interval), Some(deadline)) => Some(interval.min(deadline)),
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
mod tests;
