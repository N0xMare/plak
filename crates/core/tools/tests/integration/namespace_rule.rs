//! Completeness and determinism of the namespace clash rule.

#![allow(clippy::expect_used, reason = "integration tests")]

use plakous_model::ToolName;
use plakous_tools::{
    CompositionRuntime, NamespaceClashReason, NamespacePresentation, PresentationProvider,
    resolve_namespace,
};

fn name(value: &str) -> ToolName {
    ToolName::new(value).expect("valid tool name")
}

fn p(id: &'static str, exposed: &str, slots: &[&str]) -> NamespacePresentation {
    NamespacePresentation::new(
        PresentationProvider::Pack(id),
        name(exposed),
        slots.iter().map(|slot| name(slot)),
    )
}

fn strs(resolved: &[ToolName]) -> Vec<&str> {
    resolved.iter().map(ToolName::as_str).collect()
}

/// The first clash in declaration order is the one reported, whatever the
/// names sort to.
#[test]
fn the_first_reported_clash_is_in_declaration_order() {
    let clash = resolve_namespace(&[
        p("z1", "zz", &["s1"]),
        p("z2", "zz", &["s2"]),
        p("a1", "aa", &["s3"]),
        p("a2", "aa", &["s4"]),
    ])
    .expect_err("two clashes");
    assert_eq!(clash.name().as_str(), "zz");
    assert_eq!(clash.first(), PresentationProvider::Pack("z1"));
    assert_eq!(clash.second(), PresentationProvider::Pack("z2"));
}

#[test]
fn alternate_without_a_clash_is_inert() {
    let resolved = resolve_namespace(&[
        p("a", "wait", &["s1"]).with_alternate(name("wait_jobs")),
        p("b", "read", &["s2"]),
    ])
    .expect("no clash");
    assert_eq!(strs(&resolved), ["wait", "read"]);
}

#[test]
fn three_way_one_keeper_two_distinct_alternates_resolves() {
    let resolved = resolve_namespace(&[
        p("a", "wait", &["s1"]).with_alternate(name("wait_a")),
        p("b", "wait", &["s2"]),
        p("c", "wait", &["s3"]).with_alternate(name("wait_c")),
    ])
    .expect("one keeper");
    assert_eq!(strs(&resolved), ["wait_a", "wait", "wait_c"]);
}

#[test]
fn three_way_two_keepers_fails_naming_the_keepers() {
    let clash = resolve_namespace(&[
        p("a", "wait", &["s1"]).with_alternate(name("wait_a")),
        p("b", "wait", &["s2"]),
        p("c", "wait", &["s3"]),
    ])
    .expect_err("two keepers");
    assert_eq!(clash.reason(), NamespaceClashReason::NoAlternate);
    assert_eq!(clash.first(), PresentationProvider::Pack("b"));
    assert_eq!(clash.second(), PresentationProvider::Pack("c"));
}

#[test]
fn two_movers_sharing_one_alternate_fail_as_taken() {
    let clash = resolve_namespace(&[
        p("a", "wait", &["s1"]).with_alternate(name("wait_x")),
        p("b", "wait", &["s2"]),
        p("c", "wait", &["s3"]).with_alternate(name("wait_x")),
    ])
    .expect_err("shared alternate");
    assert_eq!(clash.reason(), NamespaceClashReason::AlternateTaken);
    assert_eq!(clash.name().as_str(), "wait_x");
}

#[test]
fn alternate_equal_to_own_name_fails_as_taken() {
    let clash = resolve_namespace(&[
        p("a", "wait", &["s1"]).with_alternate(name("wait")),
        p("b", "wait", &["s2"]),
    ])
    .expect_err("self alternate");
    assert_eq!(clash.reason(), NamespaceClashReason::AlternateTaken);
}

/// An alternate equal to a name whose own holder moved away is still taken by
/// that name's keeper; and a name vacated by a mover is not refilled (no chains).
#[test]
fn cross_name_alternates_do_not_chain() {
    // a: x->y, b keeps x; c: y->z, d keeps y. a lands on y held by d: taken.
    let clash = resolve_namespace(&[
        p("a", "x", &["s1"]).with_alternate(name("y")),
        p("b", "x", &["s2"]),
        p("c", "y", &["s3"]).with_alternate(name("z")),
        p("d", "y", &["s4"]),
    ])
    .expect_err("taken");
    assert_eq!(clash.reason(), NamespaceClashReason::AlternateTaken);
    assert_eq!(clash.name().as_str(), "y");
    // Swap: a: x->y, b keeps x; c: y->z, d: y->w (no keeper at y): ambiguous.
    let clash = resolve_namespace(&[
        p("a", "x", &["s1"]).with_alternate(name("y")),
        p("b", "x", &["s2"]),
        p("c", "y", &["s3"]).with_alternate(name("z")),
        p("d", "y", &["s4"]).with_alternate(name("w")),
    ])
    .expect_err("ambiguous at y");
    assert_eq!(clash.reason(), NamespaceClashReason::AmbiguousAlternate);
}

#[test]
fn partially_shared_slots_are_same_slot_even_with_alternates() {
    let clash = resolve_namespace(&[
        p("a", "run", &["s1", "s2"]).with_alternate(name("run_a")),
        p("b", "run", &["s2", "s3"]),
    ])
    .expect_err("shared slot s2");
    assert_eq!(clash.reason(), NamespaceClashReason::SameSlot);
}

/// The resolved spelling of each presentation does not depend on declaration
/// order (only the returned order does).
#[test]
fn resolution_is_permutation_invariant_per_presentation() {
    let set = [
        p("a", "wait", &["s1"]).with_alternate(name("wait_a")),
        p("b", "wait", &["s2"]),
        p("c", "read", &["s3"]),
        p("d", "read", &["s4"]).with_alternate(name("read_d")),
    ];
    let reference = resolve_namespace(&set).expect("resolves");
    let mut index = [0usize, 1, 2, 3];
    // All 24 permutations (Heap's algorithm, iterative).
    let mut c = [0usize; 4];
    let check = |order: &[usize; 4]| {
        let permuted = order.iter().map(|&i| set[i].clone()).collect::<Vec<_>>();
        let resolved = resolve_namespace(&permuted).expect("resolves");
        for (position, &i) in order.iter().enumerate() {
            assert_eq!(resolved[position], reference[i]);
        }
    };
    check(&index);
    let mut i = 0;
    while i < 4 {
        if c[i] < i {
            if i % 2 == 0 {
                index.swap(0, i);
            } else {
                index.swap(c[i], i);
            }
            check(&index);
            c[i] += 1;
            i = 0;
        } else {
            c[i] = 0;
            i += 1;
        }
    }
}

/// A runtime's markers hold their names in the nested namespace too: a nested
/// `wait` of another slot beside the native-ptc `wait` marker takes its
/// declared alternate, and without one the construction fails.
#[test]
fn a_nested_wait_beside_a_wait_marker_takes_its_alternate() {
    let markers = CompositionRuntime::CodexNative
        .marker_presentations()
        .expect("markers");
    let pi_wait = p("pi", "wait", &["plakous_wait_jobs"]);
    let resolved = resolve_namespace(
        &[
            markers.clone(),
            vec![pi_wait.clone().with_alternate(name("wait_jobs"))],
        ]
        .concat(),
    )
    .expect("resolves");
    assert_eq!(strs(&resolved), ["exec", "wait", "wait_jobs"]);
    let clash = resolve_namespace(&[markers, vec![pi_wait]].concat()).expect_err("clash");
    assert_eq!(clash.reason(), NamespaceClashReason::NoAlternate);
    assert_eq!(clash.first(), PresentationProvider::Runtime("native-ptc"));
    assert_eq!(clash.second(), PresentationProvider::Pack("pi"));
}

/// Every runtime's markers resolve among themselves, and two runtimes' markers
/// never coexist in one construction (no runtime x runtime clash path).
#[test]
fn runtime_marker_sets_are_internally_disjoint() {
    for runtime in [
        CompositionRuntime::CodexNative,
        CompositionRuntime::ProductJavaScript,
        CompositionRuntime::PythonPtc,
    ] {
        let markers = runtime.marker_presentations().expect("markers");
        let resolved = resolve_namespace(&markers).expect("markers resolve");
        assert_eq!(strs(&resolved), runtime.marker_names());
    }
}
