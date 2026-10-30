use super::{
    DEFAULT_FRAME_CAP_FPS, FrameWork, IDLE_INTERVAL, RedrawDemand, RedrawReason, RedrawSchedule,
    RedrawState, RedrawWait, SCHEDULE_TABLE, SPINNER_INTERVAL, WHEEL_DRAIN_INTERVAL,
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

/// Spec 02 §5.8's true idle: a park has no polling ceiling, only a
/// deadline something armed, and a cadence is cut short by one.
#[test]
fn a_parked_wait_is_unbounded_unless_a_deadline_is_armed() {
    assert_eq!(RedrawWait::Park.bound(None), None);
    let soon = Duration::from_millis(30);
    assert_eq!(RedrawWait::Park.bound(Some(soon)), Some(soon));
    let spinner = RedrawWait::Until(SPINNER_INTERVAL);
    assert_eq!(spinner.bound(None), Some(SPINNER_INTERVAL));
    assert_eq!(spinner.bound(Some(soon)), Some(soon));
    assert_eq!(
        spinner.bound(Some(Duration::from_secs(5))),
        Some(SPINNER_INTERVAL)
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
