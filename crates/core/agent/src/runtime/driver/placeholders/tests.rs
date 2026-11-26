use super::{AcceptedPlaceholders, PlaceholderPreference, SourceAudit};
use crate::ContextEvaluationAudit;
use plakous_model::{Digest, DigestDomain};

fn choose(
    before: u64,
    probes: impl IntoIterator<Item = (usize, bool, bool, u64)>,
) -> (Option<usize>, usize) {
    let mut preference = PlaceholderPreference::new(before);
    let mut prepared = 0;
    for (count, fits, clears, after) in probes {
        prepared += 1;
        if preference.observe(count, fits, clears, after) {
            return (Some(count), prepared);
        }
    }
    (preference.smallest(), prepared)
}

#[test]
fn near_null_placeholder_is_not_an_eligible_fold() {
    for clears in [false, true] {
        assert_eq!(choose(1_000, [(1, true, clears, 951)]), (None, 1));
        assert_eq!(choose(1_000, [(1, true, clears, 950)]), (Some(1), 1));
    }
    assert_eq!(choose(1_001, [(1, true, false, 951)]), (None, 1));
    assert_eq!(choose(1_001, [(1, true, false, 950)]), (Some(1), 1));
}

#[test]
fn fresh_source_audit_enforces_the_minimum_even_below_the_landing_fence()
-> Result<(), Box<dyn std::error::Error>> {
    let digest = Digest::sha256(DigestDomain::ContextAdmission, 1, &[b"boundary"])?;
    let measurement = Digest::sha256(DigestDomain::TokenMeasurement, 1, &[b"boundary"])?;
    let source = SourceAudit::Fresh {
        audit: ContextEvaluationAudit::new(measurement, digest, 1_000)?,
        occupancy: Some(1_000),
    };
    assert_eq!(
        source.no_eligible_detail(false),
        "no consumed tool-result candidates or replay-omission successor"
    );
    assert_eq!(
        source.no_eligible_detail(true),
        "no offline successor meets the 5% reduction minimum and fits"
    );
    let rescue = SourceAudit::UnboundRescue {
        audit: ContextEvaluationAudit::new(measurement, digest, 1_000)?,
        occupancy: Some(1_000),
    };
    for has_candidates in [false, true] {
        assert_eq!(
            rescue.no_eligible_detail(has_candidates),
            "no unbound-replay rescue successor prepares below the landing fence"
        );
    }
    for (after, eligible) in [(951, false), (950, true)] {
        let candidate = AcceptedPlaceholders {
            count: 1,
            after: ContextEvaluationAudit::new(measurement, digest, after)?,
            occupancy_after: Some(after),
            wire_digest: Some(digest),
            clears_fence: true,
        };
        assert_eq!(source.accepts(&candidate), eligible);
    }
    Ok(())
}

#[test]
fn anchored_fence_does_not_make_expansion_useful_and_depths_are_non_monotone() {
    assert_eq!(
        choose(
            100,
            [
                (1, true, false, 104),
                (2, true, true, 90),
                (3, true, false, 96),
            ],
        ),
        (Some(2), 2),
        "the shallowest fence-clearing successor stops later preparations"
    );
    assert_eq!(
        // `clears_fence` can be true because publication would discard the
        // source anchor. Admission pressure still compares the two
        // independently measured requests and rejects net expansion.
        choose(100, [(1, true, true, 104), (2, true, true, 101)]),
        (None, 2),
        "fitting or fence-clearing expansion is not useful shrink"
    );
    assert_eq!(
        choose(
            100,
            [
                (1, true, false, 90),
                (2, true, false, 80),
                (3, true, false, 85)
            ],
        ),
        (Some(2), 3),
        "when no depth clears the fence, every non-monotone depth is prepared and the smallest fitting successor wins"
    );
}
