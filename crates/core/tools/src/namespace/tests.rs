use super::*;

fn name(value: &str) -> ToolName {
    ToolName::new(value).expect("valid tool name")
}

fn pack(id: &'static str, exposed: &str, slot: &str) -> NamespacePresentation {
    NamespacePresentation::new(PresentationProvider::Pack(id), name(exposed), [name(slot)])
}

fn names(resolved: &[ToolName]) -> Vec<&str> {
    resolved.iter().map(ToolName::as_str).collect()
}

#[test]
fn distinct_names_resolve_unchanged_in_declaration_order() {
    let resolved = resolve_namespace(&[
        pack("files", "read", "plakous_read"),
        pack("process", "bash", "plakous_exec"),
    ])
    .expect("no clash");
    assert_eq!(names(&resolved), ["read", "bash"]);
}

#[test]
fn a_different_slot_clash_takes_the_declared_alternate() {
    let resolved = resolve_namespace(&[
        NamespacePresentation::new(
            PresentationProvider::Runtime("native-ptc"),
            name("wait"),
            [name("wait")],
        ),
        pack("pi", "wait", "plakous_wait_jobs").with_alternate(name("wait_jobs")),
    ])
    .expect("the alternate resolves the clash");
    assert_eq!(names(&resolved), ["wait", "wait_jobs"]);
}

#[test]
fn an_undeclared_clash_fails_with_a_typed_error_naming_both_providers() {
    let clash = resolve_namespace(&[
        pack("plak.product-extensions.v1", "wait", "wait_agent"),
        pack("pi", "wait", "plakous_wait_jobs"),
    ])
    .expect_err("nothing is shadowed");
    assert_eq!(clash.name().as_str(), "wait");
    assert_eq!(
        clash.first(),
        PresentationProvider::Pack("plak.product-extensions.v1")
    );
    assert_eq!(clash.second(), PresentationProvider::Pack("pi"));
    assert_eq!(clash.reason(), NamespaceClashReason::NoAlternate);
    assert_eq!(
        clash.to_string(),
        "tool name `wait` is presented by pack `plak.product-extensions.v1` and pack `pi` in \
             one namespace; neither declares an alternate spelling"
    );
    let error = ToolRuntimeError::from(clash.clone());
    assert_eq!(error.kind(), ToolRuntimeErrorKind::Registration);
    assert_eq!(error.namespace_clash(), Some(&clash));
    assert!(
        std::error::Error::source(&error)
            .and_then(|source| source.downcast_ref::<NamespaceClash>())
            .is_some_and(|source| source == &clash)
    );
    assert!(error.to_string().contains("pack `pi`"));
}

#[test]
fn one_capability_presented_twice_is_refused_even_with_an_alternate() {
    let clash = resolve_namespace(&[
        pack("a", "wait", "plakous_wait_jobs"),
        pack("b", "wait", "plakous_wait_jobs").with_alternate(name("wait_jobs")),
    ])
    .expect_err("same slot");
    assert_eq!(clash.reason(), NamespaceClashReason::SameSlot);
}

#[test]
fn two_alternates_leave_no_owner_and_fail() {
    let clash = resolve_namespace(&[
        pack("a", "wait", "one").with_alternate(name("wait_one")),
        pack("b", "wait", "two").with_alternate(name("wait_two")),
    ])
    .expect_err("ambiguous");
    assert_eq!(clash.reason(), NamespaceClashReason::AmbiguousAlternate);
}

#[test]
fn alternates_do_not_chain() {
    let clash = resolve_namespace(&[
        pack("a", "wait", "one"),
        pack("b", "wait", "two").with_alternate(name("wait_jobs")),
        pack("c", "wait_jobs", "three"),
    ])
    .expect_err("the alternate is taken");
    assert_eq!(clash.name().as_str(), "wait_jobs");
    assert_eq!(clash.first(), PresentationProvider::Pack("b"));
    assert_eq!(clash.second(), PresentationProvider::Pack("c"));
    assert_eq!(clash.reason(), NamespaceClashReason::AlternateTaken);
}

#[test]
fn the_first_clash_is_reported_in_declaration_order() {
    let clash = resolve_namespace(&[
        pack("z1", "zz", "s1"),
        pack("z2", "zz", "s2"),
        pack("a1", "aa", "s3"),
        pack("a2", "aa", "s4"),
    ])
    .expect_err("two clashes");
    assert_eq!(clash.name().as_str(), "zz");
    assert_eq!(clash.first(), PresentationProvider::Pack("z1"));
}
