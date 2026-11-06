//! `residency_dormant_materialize`: a session served by `plak host` goes
//! dormant once its last connection leaves and is materialized again by the
//! next one (spec 02 §4.4). Its lifecycle is the web face's
//! (`web_face::dormant_lifecycle`); every cut kills the endpoint at one of
//! its commits, including the idle release's and the wake's, and the reopen
//! after it is the ordinary `plak agent` relaunch.

use super::*;

/// The shape's name; the driver gives it the web face.
pub(in super::super) const NAME: &str = "residency_dormant_materialize";

pub(super) fn configure(fixture: &Fixture) {
    super::super::web_face::configure(fixture);
}

pub(super) fn assert_dormant(fixture: &Fixture, outcome: &AgentOutcome, state: &summary::Summary) {
    assert_live(fixture, outcome, state, 0, 1);
    assert_eq!(
        outcome.kinds(),
        ["assistant", "released", "woke"],
        "the web face's milestones"
    );
    // Opened once for the first connection and once more for the wake.
    for said in ["plak host: opening session ", "plak host: opened session "] {
        assert_eq!(
            outcome
                .stderr
                .lines()
                .filter(|line| line.starts_with(said))
                .count(),
            2,
            "{said:?} twice: {}",
            outcome.stderr
        );
    }
}

#[test]
fn residency_dormant_materialize_pilot_releases_and_wakes() {
    let (seed, session) = super::super::driver::setup(NAME);
    super::super::driver::pilot(&seed, &session, NAME);
}
