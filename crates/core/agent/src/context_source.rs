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
mod tests;
