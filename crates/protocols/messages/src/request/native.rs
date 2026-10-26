//! Exact native replay alongside semantic text and citation projections.

use plakous_model::{ContentBlock, OpaqueBlock};
use serde::{Deserialize, Serialize, Serializer, ser::SerializeSeq};
use serde_json::value::RawValue;

use super::{MessageBlock, MessagesRequestError, MessagesRequestPolicy, invalid};

pub(super) fn native_projections(
    blocks: &[ContentBlock],
    policy: &MessagesRequestPolicy,
    diagnostics: &mut Vec<String>,
) -> Result<std::collections::BTreeMap<usize, Option<MessageBlock>>, MessagesRequestError> {
    let mut projections = std::collections::BTreeMap::new();
    let mut targets = std::collections::BTreeSet::new();
    for (index, block) in blocks.iter().enumerate() {
        let ContentBlock::Opaque(opaque) = block else {
            continue;
        };
        if opaque.replay().protocol() != &policy.protocol
            || opaque.replay().kind().as_str() != NATIVE_REPLAY_KIND
        {
            continue;
        }
        let (native, mismatch) = native_replay_block(opaque, policy, blocks)?;
        if let Some(target) = native.target {
            let target =
                usize::try_from(target).map_err(|_| invalid("invalid native Messages target"))?;
            if !targets.insert(target) {
                return Err(invalid("duplicate native Messages citation target"));
            }
            if mismatch.is_some() {
                let notice =
                    "incompatible Messages citation replay was stripped; answer text was retained";
                if !diagnostics.iter().any(|diagnostic| diagnostic == notice) {
                    diagnostics.push(notice.to_owned());
                }
            } else {
                projections.insert(
                    target,
                    Some(MessageBlock::Native {
                        block: RawValue::from_string(native.block)
                            .map_err(|_| invalid("invalid native Messages JSON"))?,
                    }),
                );
            }
            projections.insert(index, None);
        } else {
            if mismatch.is_some() {
                return Err(invalid(
                    "native Messages block has no portable projection for this binding",
                ));
            }
            projections.insert(
                index,
                Some(MessageBlock::Native {
                    block: RawValue::from_string(native.block)
                        .map_err(|_| invalid("invalid native Messages JSON"))?,
                }),
            );
        }
    }
    Ok(projections)
}

pub(crate) const NATIVE_REPLAY_KIND: &str = "messages.native_block";

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeReplay {
    pub(crate) target: Option<u32>,
    // The surrounding replay binding is JCS-canonicalized. A string keeps the
    // native block's exact bytes out of that normalization.
    pub(crate) block: String,
}

pub(super) fn serialize_wire_blocks<S: Serializer>(
    blocks: &[MessageBlock],
    serializer: S,
) -> Result<S::Ok, S::Error> {
    let mut sequence = serializer.serialize_seq(Some(blocks.len()))?;
    for block in blocks {
        match block {
            MessageBlock::Native { block } => sequence.serialize_element(block)?,
            block => sequence.serialize_element(block)?,
        }
    }
    sequence.end()
}

pub(crate) fn native_replay_block(
    opaque: &OpaqueBlock,
    policy: &MessagesRequestPolicy,
    blocks: &[ContentBlock],
) -> Result<(NativeReplay, Option<&'static str>), MessagesRequestError> {
    let (raw, mismatch) = super::translate::replay_payload_for_projection(
        opaque.replay(),
        policy,
        NATIVE_REPLAY_KIND,
    )?;
    let native: NativeReplay =
        serde_json::from_str(raw.get()).map_err(|_| invalid("invalid native Messages replay"))?;
    let value: serde_json::Value = serde_json::from_str(&native.block)
        .map_err(|_| invalid("invalid native Messages block"))?;
    let kind = value["type"]
        .as_str()
        .ok_or_else(|| invalid("native Messages block lacks a type"))?;
    match native.target {
        Some(target) => {
            let text = usize::try_from(target)
                .ok()
                .and_then(|index| blocks.get(index))
                .and_then(|block| match block {
                    ContentBlock::Text(text) => Some(text.text()),
                    _ => None,
                })
                .ok_or_else(|| invalid("native Messages citation target is not text"))?;
            if kind != "text"
                || value["text"].as_str() != Some(text)
                || value["citations"].as_array().is_none_or(Vec::is_empty)
            {
                return Err(invalid(
                    "native Messages cited text does not match its target",
                ));
            }
        }
        None => {
            if matches!(kind, "text" | "thinking" | "redacted_thinking" | "refusal")
                || excluded_content_block(kind)
                || protected_tool_block(kind, &value)
            {
                return Err(invalid(
                    "native Messages replay cannot bypass typed block validation",
                ));
            }
        }
    }
    Ok((native, mismatch))
}

pub(crate) fn excluded_content_block(kind: &str) -> bool {
    matches!(
        kind,
        "fallback"
            | "model_fallback"
            | "multimodal"
            | "image"
            | "document"
            | "audio"
            | "video"
            | "structured_output"
            | "server_tools"
    )
}

pub(crate) fn protected_tool_block(kind: &str, value: &serde_json::Value) -> bool {
    kind.contains("tool")
        || value.get("tool_use_id").is_some()
        || value.get("caller").is_some()
        || value.get("container_id").is_some()
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "native replay fixtures fail fast on JSON construction"
)]
mod tests {
    use super::super::{MessageParam, MessageRole};
    use super::*;

    #[test]
    fn native_wire_bytes_are_unwrapped_once_but_artifact_tag_is_distinct() {
        let raw = r#"{ "type":"text", "text":"Synthetic cited answer.","citations":[{"type":"web_search_result_location","encrypted_index":"synthetic\u002dindex"}]}"#;
        let block = MessageBlock::Native {
            block: RawValue::from_string(raw.to_owned()).expect("sanitized fixture"),
        };
        let artifact = serde_json::to_string(&block).expect("sanitized fixture");
        assert_eq!(artifact, format!(r#"{{"type":"native","block":{raw}}}"#));
        let decoded: MessageBlock = serde_json::from_str(&artifact).expect("sanitized fixture");
        assert_eq!(
            serde_json::to_string(&decoded).expect("sanitized fixture"),
            artifact
        );
        let message = MessageParam {
            role: MessageRole::Assistant,
            content: vec![decoded],
        };
        let wire = serde_json::to_string(&message).expect("sanitized fixture");
        assert_eq!(wire, format!(r#"{{"role":"assistant","content":[{raw}]}}"#));
        assert_eq!(wire.matches("Synthetic cited answer.").count(), 1);
    }
}
