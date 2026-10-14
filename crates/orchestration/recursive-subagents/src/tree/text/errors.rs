//! Model-visible topology error messages returned in tool failure JSON.

use crate::protocol::{
    MAX_MESSAGE_BYTES, MAX_OUTPUT_JSON_BYTES, MAX_WAIT_TARGETS, MessagePurpose,
    SendAgentMessageRequest, SubmitResultRequest, TopologyErrorCode, WaitAgentRequest,
};

const MAX_FAILURE_MESSAGE_BYTES: usize = 512;

fn truncate_message(message: String) -> String {
    if message.len() <= MAX_FAILURE_MESSAGE_BYTES {
        return message;
    }
    message
        .chars()
        .take(MAX_FAILURE_MESSAGE_BYTES)
        .collect::<String>()
}

/// `wait_agent` request validation failure surfaced to the calling model.
#[must_use]
pub fn topology_wait_request_invalid_message(
    code: TopologyErrorCode,
    request: &WaitAgentRequest,
) -> String {
    let ids = request.agent_ids.as_deref();
    let message = match code {
        TopologyErrorCode::SchemaMismatch if ids.is_some_and(<[String]>::is_empty) => {
            "agent_ids, when given, must contain at least one id".to_owned()
        }
        TopologyErrorCode::SchemaMismatch
            if ids.is_some_and(|ids| ids.len() > MAX_WAIT_TARGETS) =>
        {
            format!("agent_ids must contain at most {MAX_WAIT_TARGETS} ids")
        }
        TopologyErrorCode::SchemaMismatch if request.timeout_ms == Some(0) => {
            "timeout_ms must be >= 1".to_owned()
        }
        TopologyErrorCode::SchemaMismatch => "agent_ids must contain unique ids".to_owned(),
        TopologyErrorCode::BoundExceeded => "timeout_ms must be >= 1".to_owned(),
        other => format!("wait request is invalid ({other:?})"),
    };
    truncate_message(message)
}

/// `send_agent_message` request validation failure surfaced to the calling model.
#[must_use]
pub fn topology_mail_request_invalid_message(
    code: TopologyErrorCode,
    request: &SendAgentMessageRequest,
    caller_id: &str,
) -> String {
    let message = match code {
        TopologyErrorCode::SelfSend => "agent_id must not equal the caller".to_owned(),
        TopologyErrorCode::BoundExceeded => {
            format!("message must be 1..={MAX_MESSAGE_BYTES} bytes")
        }
        TopologyErrorCode::ReplyInvalid if request.purpose == Some(MessagePurpose::Reply) => {
            "purpose=reply requires in_reply_to set to an inbox message_id".to_owned()
        }
        TopologyErrorCode::ReplyInvalid => {
            "in_reply_to is allowed only when purpose=reply".to_owned()
        }
        other => format!(
            "mail request is invalid ({other:?}); caller={caller_id}, agent_id={}",
            request.agent_id
        ),
    };
    truncate_message(message)
}

/// `submit_result` request validation failure surfaced to the calling model.
#[must_use]
pub fn topology_submit_request_invalid_message(
    code: TopologyErrorCode,
    request: &SubmitResultRequest,
) -> String {
    let message = match code {
        TopologyErrorCode::SubmitRootForbidden => {
            "submit_result is forbidden on the tree root".to_owned()
        }
        TopologyErrorCode::SchemaMismatch if request.turn_token < 1 => {
            "turn_token must be >= 1".to_owned()
        }
        TopologyErrorCode::BoundExceeded => {
            format!("output_json must be <= {MAX_OUTPUT_JSON_BYTES} bytes")
        }
        other => format!("submit request is invalid ({other:?})"),
    };
    truncate_message(message)
}

/// Child `submit_result` output_json schema mismatch surfaced to the submitting model.
#[must_use]
pub fn topology_output_schema_mismatch_message(detail: &str) -> String {
    truncate_message(format!(
        "output_json does not match frozen schema: {detail}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One rendered sample, pinned so the template and its substitutions stay exact.
    const WAIT_SAMPLE_DIGEST: &str =
        "d0395e87eaff78020cedddce19b7d596c30c0be391e9f09b35bd404c3c2b243a";
    use plakous::tools::PromptDigest;

    #[test]
    fn topology_error_messages_are_repairable() {
        let wait = topology_wait_request_invalid_message(
            TopologyErrorCode::SchemaMismatch,
            &WaitAgentRequest {
                agent_ids: Some(vec![]),
                timeout_ms: None,
            },
        );
        assert!(wait.contains("agent_ids"));
        assert!(wait.len() <= 512);

        let mail = topology_mail_request_invalid_message(
            TopologyErrorCode::ReplyInvalid,
            &SendAgentMessageRequest {
                agent_id: "c1".into(),
                message: "hi".into(),
                purpose: Some(MessagePurpose::Reply),
                priority: None,
                in_reply_to: None,
                output_schema_json: None,
            },
            "root",
        );
        assert!(mail.contains("purpose=reply"));
        assert!(mail.contains("in_reply_to"));

        let submit = topology_submit_request_invalid_message(
            TopologyErrorCode::SchemaMismatch,
            &SubmitResultRequest {
                turn_token: 0,
                output_json: "{}".into(),
            },
        );
        assert!(submit.contains("turn_token"));

        let output = topology_output_schema_mismatch_message("required property 'ok' is missing");
        assert!(output.contains("output_json"));
        assert!(output.contains("ok"));
    }

    #[test]
    fn topology_error_message_samples_are_stable() {
        let wait = topology_wait_request_invalid_message(
            TopologyErrorCode::SchemaMismatch,
            &WaitAgentRequest {
                agent_ids: Some(vec![]),
                timeout_ms: None,
            },
        );
        assert_eq!(wait.len(), 51);
        assert_eq!(
            PromptDigest::of(&wait),
            PromptDigest::from_hex(WAIT_SAMPLE_DIGEST)
        );
    }
}
