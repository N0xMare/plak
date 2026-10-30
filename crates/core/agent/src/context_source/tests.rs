use super::*;
use crate::ContextSourceSelector;
use plakous_model::DigestDomain;

#[test]
fn resolver_returns_content_addressed_facts() {
    let mut catalog = ContextSourceCatalog::new();
    catalog
        .insert(
            RegisteredContextSource::new("repo.agents", "prefer exact tests", "1", "workspace")
                .expect("source"),
        )
        .expect("insert");
    let resolver = ContextSourceResolver::new(catalog);
    let facts = resolver
        .resolve(&[ContextSourceSelector::new("repo.agents").expect("selector")])
        .expect("resolve");
    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0].source_id(), "repo.agents");
    assert_eq!(facts[0].content(), "prefer exact tests");
    assert!(facts[0].immutable_reference().is_none());
}

#[test]
fn verified_reference_is_preserved_after_exact_checks() {
    let content = "immutable repository policy";
    let digest = Digest::sha256(DigestDomain::Blob, 1, &[content.as_bytes()]).expect("digest");
    let reference =
        VerifiedContextSourceRef::new("object.repo-policy", digest, "rev-7").expect("ref");
    let source = RegisteredContextSource::from_verified_reference(
        "repo.policy",
        content,
        "rev-7",
        "snapshot",
        reference,
    )
    .expect("verified source");
    let mut catalog = ContextSourceCatalog::new();
    catalog.insert(source).expect("insert");
    let facts = ContextSourceResolver::new(catalog)
        .resolve(&[ContextSourceSelector::new("repo.policy").expect("selector")])
        .expect("resolve");
    assert_eq!(facts[0].content(), content);
    assert_eq!(
        facts[0]
            .immutable_reference()
            .expect("immutable reference")
            .reference_id(),
        "object.repo-policy"
    );
}

#[test]
fn verified_reference_digest_mismatch_fails_closed() {
    let wrong_digest =
        Digest::sha256(DigestDomain::Blob, 1, &[b"different bytes"]).expect("digest");
    let reference =
        VerifiedContextSourceRef::new("object.repo-policy", wrong_digest, "rev-7").expect("ref");
    let error = RegisteredContextSource::from_verified_reference(
        "repo.policy",
        "actual bytes",
        "rev-7",
        "snapshot",
        reference,
    )
    .expect_err("mismatch");
    assert!(matches!(
        error,
        ProfileError::ContextSourceDigestMismatch { .. }
    ));
}

#[test]
fn verified_reference_version_mismatch_fails_closed() {
    let content = "immutable repository policy";
    let digest = Digest::sha256(DigestDomain::Blob, 1, &[content.as_bytes()]).expect("digest");
    let reference =
        VerifiedContextSourceRef::new("object.repo-policy", digest, "rev-7").expect("ref");
    let error = RegisteredContextSource::from_verified_reference(
        "repo.policy",
        content,
        "rev-8",
        "snapshot",
        reference,
    )
    .expect_err("mismatch");
    assert!(matches!(
        error,
        ProfileError::ContextSourceVersionMismatch { .. }
    ));
}

#[test]
fn source_content_bound_fails_closed() {
    let oversized = "x".repeat(crate::MAX_INSTRUCTION_BYTES + 1);
    let error = RegisteredContextSource::new("repo.policy", oversized, "1", "snapshot")
        .expect_err("oversized");
    assert!(matches!(error, ProfileError::TextTooLarge { .. }));
}

#[test]
fn missing_source_fails_closed() {
    let resolver = ContextSourceResolver::new(ContextSourceCatalog::new());
    let error = resolver
        .resolve(&[ContextSourceSelector::new("missing").expect("selector")])
        .expect_err("missing");
    assert!(matches!(
        error,
        ProfileError::UnresolvedContextSource { .. }
    ));
}
