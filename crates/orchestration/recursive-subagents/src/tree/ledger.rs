use super::{
    DeserializeOwned, OperationRecord, ProductAgentTree, Serialize, TopologyError,
    TopologyErrorCode, canonical_json, digest_text, operation_request,
};

impl ProductAgentTree {
    fn replay_success<T: Serialize>(
        &self,
        operation_id: &str,
        tool_name: &str,
        request: &T,
    ) -> Result<Option<String>, TopologyError> {
        let Some(prior) = self.operations.get(operation_id) else {
            return Ok(None);
        };
        let canonical = canonical_json(request)?;
        if prior.tool_name != tool_name || prior.request_digest != digest_text(&canonical) {
            return Err(TopologyError::new(
                TopologyErrorCode::OperationRecoveryRequired,
                "retry content conflicts with the prepared operation",
            ));
        }
        if let Some(code) = prior.terminal_code {
            return Err(TopologyError::new(
                code,
                "retry of a terminal failed receipt",
            ));
        }
        match prior.terminal_response.as_ref() {
            Some(response) => Ok(Some(response.clone())),
            None => Ok(None),
        }
    }

    pub(super) fn replay_typed<T: Serialize, R: DeserializeOwned>(
        &self,
        operation_id: Option<&str>,
        tool_name: &str,
        request: &T,
    ) -> Result<Option<R>, TopologyError> {
        let Some(id) = operation_id else {
            return Ok(None);
        };
        let Some(prior) = self.replay_success(id, tool_name, request)? else {
            return Ok(None);
        };
        serde_json::from_str(&prior).map(Some).map_err(|_| {
            TopologyError::new(
                TopologyErrorCode::OperationRecoveryRequired,
                "prior topology receipt could not be reconstructed",
            )
        })
    }

    /// Opens one idempotent call: returns the recorded response
    /// when this `operation_id` already answered, and otherwise records the
    /// request so a repeat can replay it.
    ///
    /// # Errors
    ///
    /// Refuses when the id was used for a different tool or a different
    /// request — a conflict, not a replay.
    pub fn prepare_operation<T: Serialize>(
        &mut self,
        operation_id: &str,
        caller_id: &str,
        tool_name: &str,
        request: &T,
    ) -> Result<Option<String>, TopologyError> {
        if let Some(prior) = self.replay_success(operation_id, tool_name, request)? {
            return Ok(Some(prior));
        }
        if self.operations.contains_key(operation_id) {
            return Ok(None);
        }
        let (request_digest, canonical) = operation_request(tool_name, request)?;
        self.operations.insert(
            operation_id.to_owned(),
            OperationRecord {
                fold: None,
                operation_id: operation_id.to_owned(),
                caller_agent_id: caller_id.to_owned(),
                tool_name: tool_name.to_owned(),
                request_digest,
                canonical_request: canonical,
                terminal_code: None,
                terminal_response: None,
            },
        );
        Ok(None)
    }

    /// Records the exact success body a replay of this operation returns.
    ///
    /// # Errors
    ///
    /// Refuses when the response cannot be canonically encoded.
    pub fn remember_success<T: Serialize, R: Serialize>(
        &mut self,
        operation_id: &str,
        caller_id: &str,
        tool_name: &str,
        request: &T,
        response: &R,
    ) -> Result<(), TopologyError> {
        let (request_digest, canonical) = operation_request(tool_name, request)?;
        let terminal_response = serde_json::to_string(response).map_err(|_| {
            TopologyError::new(
                TopologyErrorCode::OperationRecoveryRequired,
                "spawn receipt could not be encoded",
            )
        })?;
        self.operations.insert(
            operation_id.to_owned(),
            OperationRecord {
                fold: None,
                operation_id: operation_id.to_owned(),
                caller_agent_id: caller_id.to_owned(),
                tool_name: tool_name.to_owned(),
                request_digest,
                canonical_request: canonical,
                terminal_code: None,
                terminal_response: Some(terminal_response),
            },
        );
        Ok(())
    }

    /// Records a refusal as this operation's terminal answer, so a repeat is
    /// refused the same way instead of being retried.
    ///
    /// # Errors
    ///
    /// Refuses when the request cannot be canonically encoded.
    pub fn remember_failure<T: Serialize>(
        &mut self,
        operation_id: &str,
        caller_id: &str,
        tool_name: &str,
        request: &T,
        code: TopologyErrorCode,
    ) -> Result<(), TopologyError> {
        let (request_digest, canonical) = operation_request(tool_name, request)?;
        self.operations.insert(
            operation_id.to_owned(),
            OperationRecord {
                fold: None,
                operation_id: operation_id.to_owned(),
                caller_agent_id: caller_id.to_owned(),
                tool_name: tool_name.to_owned(),
                request_digest,
                canonical_request: canonical,
                terminal_code: Some(code),
                terminal_response: None,
            },
        );
        Ok(())
    }
}
