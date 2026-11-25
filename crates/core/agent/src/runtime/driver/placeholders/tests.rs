use super::{
    AcceptedPlaceholders, PlaceholderBinarySearch, PlaceholderPreference, PlaceholdersProbe,
    SourceAudit,
};
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

#[test]
fn binary_search_finds_exact_minimal_clearing_depth_in_logarithmic_probes()
-> Result<(), Box<dyn std::error::Error>> {
    let digest = Digest::sha256(DigestDomain::ContextAdmission, 1, &[b"bsearch"])?;
    let measurement = Digest::sha256(DigestDomain::TokenMeasurement, 1, &[b"bsearch"])?;
    let fresh = SourceAudit::Fresh {
        audit: ContextEvaluationAudit::new(measurement, digest, 100_000)?,
        occupancy: Some(100_000),
    };
    let probe = |fits: bool,
                 clears_fence: bool,
                 after: u64|
     -> Result<PlaceholdersProbe, Box<dyn std::error::Error>> {
        Ok(PlaceholdersProbe {
            fits,
            clears_fence,
            after: Some(ContextEvaluationAudit::new(measurement, digest, after)?),
            occupancy: Some(after),
            wire_digest: Some(digest),
        })
    };

    // Exact minimal clearing depth for every target in 1..=32, within ceil(log2(N+1)) probes.
    for target in 1..=32_usize {
        let mut search = PlaceholderBinarySearch::new(32, fresh);
        let mut probes = 0;
        while let Some(depth) = search.next_depth() {
            probes += 1;
            let clears = depth >= target;
            let after = 95_000_u64.saturating_sub(u64::try_from(depth)? * 500);
            search.observe(depth, probe(true, clears, after)?);
        }
        let chosen = search.finish().ok_or("clearing depth")?;
        assert_eq!(chosen.count, target);
        assert!(probes <= 6, "expected <= 6 probes for N=32, got {probes}");
    }

    // Depths that do not meet the 5% reduction minimum are skipped even when
    // clearing the fence.
    let mut min_reduction = PlaceholderBinarySearch::new(32, fresh);
    while let Some(depth) = min_reduction.next_depth() {
        let after = 100_000_u64.saturating_sub(u64::try_from(depth)? * 1_000);
        min_reduction.observe(depth, probe(true, true, after)?);
    }
    assert_eq!(min_reduction.finish().map(|a| a.count), Some(5));

    // For N = 64, verify O(log N) <= 7 probes across boundary and interior targets.
    for target in [1_usize, 2, 17, 32, 63, 64] {
        let mut search = PlaceholderBinarySearch::new(64, fresh);
        let mut probes = 0;
        while let Some(depth) = search.next_depth() {
            probes += 1;
            let clears = depth >= target;
            // Depth 1 expands slightly (non-monotone notice overhead) when target > 1.
            let after = if depth == 1 && target > 1 {
                100_500
            } else {
                95_000_u64.saturating_sub(u64::try_from(depth)? * 500)
            };
            search.observe(depth, probe(true, clears, after)?);
        }
        let chosen = search.finish().ok_or("clearing depth")?;
        assert_eq!(chosen.count, target);
        assert!(probes <= 7, "expected <= 7 probes for N=64, got {probes}");
    }

    // When no depth clears the fence, return the smallest qualifying fitting depth.
    let mut no_clear = PlaceholderBinarySearch::new(16, fresh);
    let mut probes = 0;
    while let Some(depth) = no_clear.next_depth() {
        probes += 1;
        let after = 95_000_u64.saturating_sub(u64::try_from(depth)? * 500);
        no_clear.observe(depth, probe(true, false, after)?);
    }
    let chosen = no_clear.finish().ok_or("fallback fitting depth")?;
    assert_eq!(chosen.count, 16);
    assert!(probes <= 5);

    // UnboundRescue requires clearing the fence and a wire digest.
    let rescue = SourceAudit::UnboundRescue {
        audit: ContextEvaluationAudit::new(measurement, digest, 100_000)?,
        occupancy: Some(100_000),
    };
    let mut rescue_search = PlaceholderBinarySearch::new(16, rescue);
    while let Some(depth) = rescue_search.next_depth() {
        let clears = depth >= 11;
        rescue_search.observe(depth, probe(true, clears, 60_000)?);
    }
    assert_eq!(rescue_search.finish().map(|a| a.count), Some(11));

    let mut rescue_none = PlaceholderBinarySearch::new(16, rescue);
    while let Some(depth) = rescue_none.next_depth() {
        rescue_none.observe(depth, probe(true, false, 60_000)?);
    }
    assert!(rescue_none.finish().is_none());
    Ok(())
}
