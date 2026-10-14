//! Read-only completed root evidence, independent of scripted answer markers.

use plakous::sessions::SessionLocator;
use plakous_agent::internal::{
    DurableCommandDisposition, DurableOperationKind, DurableOperationState, SessionRef,
};
use plakous_model::ModelResponse;
use plakous_store::SqliteSessionStore;
use serde_json::Value;
use std::path::Path;

/// Reads accepted command bindings, their operation states and the working root.
///
/// # Errors
/// Returns read-only store lookup or integrity failures.
pub fn inspect_root_lifecycles(
    database: &Path,
    head: SessionLocator,
) -> Result<(Option<String>, Vec<Value>), plakous_agent::internal::SessionStoreError> {
    use plakous_agent::internal::{RuntimeLifecycle, RuntimeWorkingRoot};
    let loaded = SqliteSessionStore::inspect_branch(
        database,
        SessionRef::new(head.session_id(), head.branch_id()),
    )?;
    let manifest = loaded.checkpoint().checkpoint().manifest();
    let head_turn = match manifest.lifecycle() {
        RuntimeLifecycle::Working {
            root: RuntimeWorkingRoot::Turn { turn_id, .. },
            ..
        } => Some(turn_id.to_string()),
        _ => None,
    };
    let ledger = manifest.ledger();
    let operations = ledger.operations();
    let mut roots = Vec::new();
    for command in ledger.commands() {
        if command.disposition() != DurableCommandDisposition::Accepted {
            continue;
        }
        let Some(turn) = command.turn_id() else {
            continue;
        };
        let Some(operation) = operations.iter().find(|operation| {
            matches!(operation.kind(), DurableOperationKind::Command { turn_id: Some(id) } if *id == turn)
        }) else { continue };
        let state = match operation.state() {
            DurableOperationState::Dispatched => {
                let provider = operations.iter().filter(|operation| {
                    matches!(operation.kind(), DurableOperationKind::Provider { root: plakous_model::ModelOperationRoot::Turn(id), .. } if *id == turn)
                }).max_by_key(|operation| operation.operation_id());
                let terminal = match provider.map(|provider| provider.state()) {
                    Some(
                        DurableOperationState::Completed(evidence)
                        | DurableOperationState::ProvenNotDispatched(evidence)
                        | DurableOperationState::OutcomeUnknown(evidence),
                    ) => recorded_terminal_decision(evidence.bytes())?,
                    _ => false,
                };
                if terminal {
                    "saved_terminal"
                } else {
                    "running"
                }
            }
            DurableOperationState::Prepared => "running",
            DurableOperationState::Completed(_) => "completed",
            DurableOperationState::Finalized(_) => "finalized",
            _ => "unsupported",
        };
        roots.push(serde_json::json!({"command": command.command_id(), "turn": turn.to_string(), "sequence": command.sequence(), "state": state}));
    }
    Ok((head_turn, roots))
}

fn recorded_terminal_decision(
    bytes: &[u8],
) -> Result<bool, plakous_agent::internal::SessionStoreError> {
    let corrupt = plakous_agent::internal::SessionStoreError::corrupt;
    let (tag, value): (String, Value) = serde_json::from_slice(bytes).map_err(|_| corrupt())?;
    if tag != "provider-outcome-v2" {
        return Err(corrupt());
    }
    let decision = &value["decision"];
    match decision.as_str() {
        Some("Terminal") => Ok(true),
        Some(
            "Continue" | "ToolBatch" | "RetryOutputLimit" | "RetryContentFilter" | "OverflowFold",
        ) => Ok(false),
        None if decision
            .as_object()
            .is_some_and(|object| object.len() == 1 && object.contains_key("FailTurn")) =>
        {
            Ok(true)
        }
        _ => Err(corrupt()),
    }
}

/// Reads completed root command identities and their final visible answer.
/// Intermediate provider rounds and compaction summaries are not terminals.
///
/// # Errors
/// Returns read-only store failures or malformed completed-terminal evidence.
pub fn inspect_completed_root_answers(
    database: &Path,
    head: SessionLocator,
) -> Result<Vec<(String, String)>, Box<dyn std::error::Error>> {
    let loaded = SqliteSessionStore::inspect_branch(
        database,
        SessionRef::new(head.session_id(), head.branch_id()),
    )?;
    let ledger = loaded.checkpoint().checkpoint().manifest().ledger();
    let mut answers = Vec::new();
    for operation in ledger.operations() {
        let (
            DurableOperationKind::Command {
                turn_id: Some(turn),
            },
            DurableOperationState::Finalized(evidence),
        ) = (operation.kind(), operation.state())
        else {
            continue;
        };
        let Some(terminal) = finalized_terminal_document(evidence.bytes()) else {
            continue;
        };
        let Some(completed) = terminal["outcome"].get("Completed") else {
            continue;
        };
        let command = terminal["command_id"]
            .as_str()
            .ok_or("terminal command id")?;
        if terminal["operation_id"] != operation.operation_id()
            || terminal["turn_id"] != serde_json::to_value(turn)?
            || !ledger.commands().into_iter().any(|record| {
                record.command_id() == command
                    && record.turn_id() == Some(*turn)
                    && record.disposition() == DurableCommandDisposition::Accepted
            })
        {
            return Err("completed terminal lost its exact root command binding".into());
        }
        let response: ModelResponse = serde_json::from_value(completed.clone())?;
        answers.push((command.to_owned(), joined_assistant_text(&response)));
    }
    Ok(answers)
}

fn joined_assistant_text(response: &ModelResponse) -> String {
    joined_text_blocks(
        response
            .output()
            .iter()
            .flat_map(plakous_model::ConversationItem::blocks)
            .filter_map(|block| {
                if let plakous_model::ContentBlock::Text(text) = block {
                    Some(text.text())
                } else {
                    None
                }
            }),
    )
}

fn joined_text_blocks<'a>(texts: impl IntoIterator<Item = &'a str>) -> String {
    let mut out = String::new();
    for text in texts {
        if text.is_empty() {
            continue;
        }
        if !out.is_empty() && !out.ends_with('\n') {
            out.push_str("\n\n");
        }
        out.push_str(text);
    }
    out
}

fn finalized_terminal_document(bytes: &[u8]) -> Option<Value> {
    let (tag, terminal): (String, Value) = serde_json::from_slice(bytes).ok()?;
    (tag == "plakous-terminal-result").then_some(terminal)
}

#[cfg(test)]
mod tests {
    use super::finalized_terminal_document;
    use serde_json::json;

    #[test]
    fn saved_provider_decision_distinguishes_terminal_from_running_rounds()
    -> Result<(), Box<dyn std::error::Error>> {
        for decision in [json!("Terminal"), json!({"FailTurn": {}})] {
            let bytes =
                serde_json::to_vec(&("provider-outcome-v2", json!({"decision": decision})))?;
            assert!(super::recorded_terminal_decision(&bytes)?);
        }
        for decision in [
            "Continue",
            "ToolBatch",
            "RetryOutputLimit",
            "RetryContentFilter",
            "OverflowFold",
        ] {
            let bytes =
                serde_json::to_vec(&("provider-outcome-v2", json!({"decision": decision})))?;
            assert!(!super::recorded_terminal_decision(&bytes)?);
        }
        assert!(super::recorded_terminal_decision(b"corrupt").is_err());
        assert!(
            super::recorded_terminal_decision(&serde_json::to_vec(&(
                "provider-outcome-v2",
                json!({"decision": "unknown"})
            ))?)
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn a2_completed_root_answer_joins_nonempty_text_blocks_with_blank_lines()
    -> Result<(), Box<dyn std::error::Error>> {
        use plakous_model::{
            Actor, ContentBlock, ConversationItem, Digest, DigestDomain, EntryId, FinishDetails,
            FinishReason, Provenance, TextBlock,
        };
        let response = plakous_model::ModelResponse::new(
            vec![ConversationItem::new(
                EntryId::try_from(uuid::Uuid::from_u128(0xA2_333))?,
                Actor::Assistant,
                ["first", "second"]
                    .into_iter()
                    .map(|text| TextBlock::new(text, None).map(ContentBlock::Text))
                    .collect::<Result<Vec<_>, _>>()?,
                Provenance::ModelOutput,
            )?],
            FinishDetails::new(FinishReason::Stop, None),
            None,
            None,
            Digest::sha256(DigestDomain::RequestPlan, 1, &[b"test"])?,
        );
        assert_eq!(super::joined_assistant_text(&response), "first\n\nsecond");
        assert_eq!(
            super::joined_text_blocks(["first", "", "second", ""]),
            "first\n\nsecond"
        );
        Ok(())
    }

    #[test]
    fn completed_root_reader_uses_the_finalized_terminal_codec_envelope()
    -> Result<(), Box<dyn std::error::Error>> {
        let terminal = json!({"command_id":"original","outcome":{"Completed":{}}});
        let finalized = serde_json::to_vec(&("plakous-terminal-result", &terminal))?;
        let pending = serde_json::to_vec(&("plakous-terminal-result-pending", &terminal))?;
        assert_eq!(
            finalized_terminal_document(&finalized),
            Some(terminal.clone())
        );
        assert!(finalized_terminal_document(&pending).is_none());
        assert!(finalized_terminal_document(&serde_json::to_vec(&terminal)?).is_none());
        Ok(())
    }
}
