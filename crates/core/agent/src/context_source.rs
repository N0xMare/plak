//! Pre-compilation context-source resolution.
//!
//! The resolver performs only policy-controlled read-only I/O against a
//! registered source catalog. It never accepts credentials, ambient paths with
//! live meaning inside a profile, or deferred handles. Resolved facts are pure
//! inputs to [`crate::HarnessProfileCompiler`].

use plakous_model::Digest;
use std::collections::BTreeMap;

use crate::{
    ContextSourceSelector, ProfileError, ResolvedContextSourceFact, VerifiedContextSourceRef,
};

/// One registered, content-addressed instruction/context source.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegisteredContextSource {
    fact: ResolvedContextSourceFact,
}

impl RegisteredContextSource {
    /// Registers immutable source content under a portable identity.
    ///
    /// # Errors
    ///
    /// Returns a profile error for invalid identifiers or oversized content.
    pub fn new(
        source_id: impl Into<String>,
        content: impl Into<String>,
        version: impl Into<String>,
        provenance: impl Into<String>,
    ) -> Result<Self, ProfileError> {
        Ok(Self {
            fact: ResolvedContextSourceFact::new(source_id, content, version, provenance)?,
        })
    }

    /// Registers exact content after checking it against an immutable reference.
    ///
    /// # Errors
    ///
    /// Returns a profile error for invalid fields, oversized content, or a
    /// digest/version mismatch with `immutable_reference`.
    pub fn from_verified_reference(
        source_id: impl Into<String>,
        content: impl Into<String>,
        observed_version: impl Into<String>,
        provenance: impl Into<String>,
        immutable_reference: VerifiedContextSourceRef,
    ) -> Result<Self, ProfileError> {
        Ok(Self {
            fact: ResolvedContextSourceFact::from_verified_reference(
                source_id,
                content,
                observed_version,
                provenance,
                immutable_reference,
            )?,
        })
    }

    /// Returns the registered identity.
    #[must_use]
    pub fn source_id(&self) -> &str {
        self.fact.source_id()
    }

    /// Returns the immutable content.
    #[must_use]
    pub fn content(&self) -> &str {
        self.fact.content()
    }

    /// Returns the content digest.
    #[must_use]
    pub const fn content_digest(&self) -> &Digest {
        self.fact.content_digest()
    }

    /// Returns the source version label.
    #[must_use]
    pub fn version(&self) -> &str {
        self.fact.version()
    }

    /// Returns the source provenance label.
    #[must_use]
    pub fn provenance(&self) -> &str {
        self.fact.provenance()
    }

    /// Returns immutable-reference metadata when one was verified at registration.
    #[must_use]
    pub const fn immutable_reference(&self) -> Option<&VerifiedContextSourceRef> {
        self.fact.immutable_reference()
    }
}

/// Immutable catalog of registered context sources.
#[derive(Clone, Debug, Default)]
pub struct ContextSourceCatalog {
    sources: BTreeMap<String, RegisteredContextSource>,
}

impl ContextSourceCatalog {
    /// Creates an empty catalog.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts one registered source.
    ///
    /// # Errors
    ///
    /// Returns a profile error when the identity is already present.
    pub fn insert(&mut self, source: RegisteredContextSource) -> Result<(), ProfileError> {
        let id = source.source_id().to_owned();
        if self.sources.contains_key(&id) {
            return Err(ProfileError::DuplicateContextSource { source_id: id });
        }
        self.sources.insert(id, source);
        Ok(())
    }

    /// Returns a source by identity.
    #[must_use]
    pub fn get(&self, source_id: &str) -> Option<&RegisteredContextSource> {
        self.sources.get(source_id)
    }
}

/// Bounded, credential-free context-source resolver.
#[derive(Clone, Debug, Default)]
pub struct ContextSourceResolver {
    catalog: ContextSourceCatalog,
}

impl ContextSourceResolver {
    /// Creates a resolver over an immutable catalog snapshot.
    #[must_use]
    pub const fn new(catalog: ContextSourceCatalog) -> Self {
        Self { catalog }
    }

    /// Resolves selected sources into pure compiler facts.
    ///
    /// # Errors
    ///
    /// Returns a profile error when a selector is unknown or facts cannot be built.
    pub fn resolve(
        &self,
        selectors: &[ContextSourceSelector],
    ) -> Result<Vec<ResolvedContextSourceFact>, ProfileError> {
        let mut facts = Vec::with_capacity(selectors.len());
        for selector in selectors {
            let source = self.catalog.get(selector.source_id()).ok_or_else(|| {
                ProfileError::UnresolvedContextSource {
                    source_id: selector.source_id().to_owned(),
                }
            })?;
            facts.push(source.fact.clone());
        }
        Ok(facts)
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests {
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
        let reference = VerifiedContextSourceRef::new("object.repo-policy", wrong_digest, "rev-7")
            .expect("ref");
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
}
