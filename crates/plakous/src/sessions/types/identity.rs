use super::{
    AgentSessionsError, AgentSessionsErrorKind, BranchId, CheckpointId, FromStr,
    MAX_OPERATION_ID_BYTES, SessionId, SessionLocator, SessionOperationId, fmt, invalid_locator,
    parse_locator_part,
};

impl SessionOperationId {
    /// Creates a bounded portable workflow identity.
    ///
    /// # Errors
    ///
    /// Returns invalid input unless the identity contains 1–128 portable ASCII
    /// identifier characters.
    pub fn new(value: impl Into<String>) -> Result<Self, AgentSessionsError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > MAX_OPERATION_ID_BYTES
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._:-".contains(&byte))
        {
            return Err(AgentSessionsError::new(
                AgentSessionsErrorKind::InvalidInput,
                "session operation identity is not a bounded portable identifier",
            ));
        }
        Ok(Self(value))
    }

    /// Returns the stable portable value.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SessionOperationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("SessionOperationId")
            .field(&self.0)
            .finish()
    }
}

impl fmt::Display for SessionOperationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for SessionOperationId {
    type Err = AgentSessionsError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl SessionLocator {
    /// Creates an exact semantic locator.
    #[must_use]
    pub const fn new(
        session_id: SessionId,
        branch_id: BranchId,
        checkpoint_id: CheckpointId,
    ) -> Self {
        Self {
            session_id,
            branch_id,
            checkpoint_id,
        }
    }
    /// Returns the session identity.
    #[must_use]
    pub const fn session_id(self) -> SessionId {
        self.session_id
    }
    /// Returns the independent branch identity.
    #[must_use]
    pub const fn branch_id(self) -> BranchId {
        self.branch_id
    }
    /// Returns the exact immutable checkpoint identity.
    #[must_use]
    pub const fn checkpoint_id(self) -> CheckpointId {
        self.checkpoint_id
    }
}

impl fmt::Display for SessionLocator {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}/{}/{}",
            self.session_id, self.branch_id, self.checkpoint_id
        )
    }
}

impl FromStr for SessionLocator {
    type Err = AgentSessionsError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let mut parts = value.split('/');
        let session_id = parse_locator_part(parts.next(), "session")?;
        let branch_id = parse_locator_part(parts.next(), "branch")?;
        let checkpoint_id = parse_locator_part(parts.next(), "checkpoint")?;
        if parts.next().is_some() {
            return Err(invalid_locator());
        }
        Ok(Self::new(session_id, branch_id, checkpoint_id))
    }
}
