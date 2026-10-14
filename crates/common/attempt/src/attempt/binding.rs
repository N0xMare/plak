//! Shared attempt / physical-request identity bindings for Responses dialects.

use std::fmt;

/// Exact immutable identity for one logical Responses-family service attempt.
///
/// `O` is the dialect operation binding; `I` is the physical attempt identity
/// (typically `plakous_model::AttemptId`).
#[derive(Clone, Eq, PartialEq)]
pub struct AttemptBinding<O, I> {
    operation: O,
    attempt_id: I,
}

impl<O, I> AttemptBinding<O, I> {
    /// Creates a complete attempt binding.
    #[must_use]
    pub const fn new(operation: O, attempt_id: I) -> Self {
        Self {
            operation,
            attempt_id,
        }
    }

    /// Returns the complete prepared-operation binding.
    #[must_use]
    pub const fn operation(&self) -> &O {
        &self.operation
    }

    /// Returns the unique physical attempt identity.
    #[must_use]
    pub const fn attempt_id(&self) -> I
    where
        I: Copy,
    {
        self.attempt_id
    }

    /// Returns a reference to the unique physical attempt identity.
    #[must_use]
    pub const fn attempt_id_ref(&self) -> &I {
        &self.attempt_id
    }
}

impl<O: fmt::Debug, I: fmt::Debug> fmt::Debug for AttemptBinding<O, I> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AttemptBinding")
            .field("operation", &self.operation)
            .field("attempt_id", &self.attempt_id)
            .finish()
    }
}

/// Exact identity of one crate-owned physical request generation.
///
/// A credential authorization cannot be reused after this generation finishes
/// or against another attempt.
#[derive(Clone, Eq, PartialEq)]
pub struct PhysicalRequestBinding<A> {
    attempt: A,
    generation: u32,
}

impl<A> PhysicalRequestBinding<A> {
    /// Creates a physical-request binding for `attempt` generation.
    #[must_use]
    pub const fn new(attempt: A, generation: u32) -> Self {
        Self {
            attempt,
            generation,
        }
    }

    /// Returns the complete logical-attempt binding.
    #[must_use]
    pub const fn attempt(&self) -> &A {
        &self.attempt
    }

    /// Returns the monotonically increasing physical-request generation.
    #[must_use]
    pub const fn generation(&self) -> u32 {
        self.generation
    }
}

impl<A: fmt::Debug> fmt::Debug for PhysicalRequestBinding<A> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PhysicalRequestBinding")
            .field("attempt", &self.attempt)
            .field("generation", &self.generation)
            .finish()
    }
}
