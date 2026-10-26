//! Native cited-text replay, semantic citations, and bounded unfamiliar blocks.

use plakous_model::{
    BlockStart, BlockStarted, CitationAdded, CitationBlock, CitationSource, CitationTarget,
    ContentBlock, ModelDiagnostic, ModelEvent, OpaqueBlock, OpaqueFallback, OutputLocation,
};
use serde_json::{Value, value::RawValue};

use super::{MessagesStreamError, MessagesStreamProjector, OpenBlock, invalid_response};
use crate::request::{NATIVE_REPLAY_KIND, NativeReplay, protected_tool_block};

pub(super) fn parse_citations(
    raw: Option<&RawValue>,
) -> Result<Vec<Box<RawValue>>, MessagesStreamError> {
    match raw {
        None => Ok(Vec::new()),
        Some(raw) if raw.get() == "null" => Ok(Vec::new()),
        Some(raw) => serde_json::from_str(raw.get())
            .map_err(|_| invalid_response("Messages citations must be an array")),
    }
}

pub(super) fn final_text_block(
    native: Box<RawValue>,
    text: &str,
    citations: &[Box<RawValue>],
) -> Result<Box<RawValue>, MessagesStreamError> {
    let mut fields: std::collections::BTreeMap<String, Box<RawValue>> =
        serde_json::from_str(native.get())
            .map_err(|_| invalid_response("invalid cited text block"))?;
    let original_text: String = serde_json::from_str(fields["text"].get())
        .map_err(|_| invalid_response("invalid cited text"))?;
    let original_citations = parse_citations(fields.get("citations").map(AsRef::as_ref))?;
    if original_text == text
        && original_citations
            .iter()
            .map(|v| v.get())
            .eq(citations.iter().map(|v| v.get()))
    {
        return Ok(native);
    }
    fields.insert(
        "text".into(),
        serde_json::value::to_raw_value(text)
            .map_err(|_| invalid_response("invalid cited text"))?,
    );
    fields.insert(
        "citations".into(),
        serde_json::value::to_raw_value(citations)
            .map_err(|_| invalid_response("invalid citations"))?,
    );
    serde_json::value::to_raw_value(&fields)
        .map_err(|_| invalid_response("invalid cited text block"))
}

fn has_protected_material(value: &Value) -> bool {
    match value {
        Value::Object(fields) => fields.iter().any(|(key, value)| {
            matches!(
                key.as_str(),
                "signature" | "encrypted_content" | "encrypted_index" | "data"
            ) || has_protected_material(value)
        }),
        Value::Array(values) => values.iter().any(has_protected_material),
        _ => false,
    }
}

pub(super) fn validate_skipped_delta(raw: &str) -> Result<(), MessagesStreamError> {
    let value: Value =
        serde_json::from_str(raw).map_err(|_| invalid_response("invalid unknown block delta"))?;
    let kind = value["type"].as_str().unwrap_or_default();
    if has_protected_material(&value)
        || protected_tool_block(kind, &value)
        || matches!(
            kind,
            "signature_delta" | "input_json_delta" | "citations_delta"
        )
    {
        return Err(invalid_response(
            "unknown Messages block delta requires unsupported exact replay",
        ));
    }
    Ok(())
}

impl MessagesStreamProjector {
    pub(super) fn reserve_output_index(&mut self) -> Result<u32, MessagesStreamError> {
        if self.next_output_index as usize >= self.limits.max_blocks() {
            return Err(self.fail("Messages canonical output exceeded the block limit"));
        }
        let index = self.next_output_index;
        self.next_output_index += 1;
        Ok(index)
    }

    pub(super) fn output_index(&self, wire: u32) -> Result<u32, MessagesStreamError> {
        self.output_indices
            .get(&wire)
            .copied()
            .ok_or_else(|| invalid_response("Messages wire block has no canonical output location"))
    }

    pub(super) fn open_unknown_block(
        &mut self,
        wire: u32,
        raw: Box<RawValue>,
    ) -> Result<(), MessagesStreamError> {
        let value: Value = serde_json::from_str(raw.get())
            .map_err(|_| invalid_response("invalid unknown Messages block"))?;
        let kind = value["type"].as_str().unwrap_or_default();
        if protected_tool_block(kind, &value) || self.server_lineage.next_unresolved_id().is_some()
        {
            return Err(self.fail("unknown Messages block cannot preserve hosted tool lineage"));
        }
        if raw.get().len()
            > self
                .limits
                .max_unknown_event_bytes()
                .min(self.limits.max_block_bytes())
        {
            return Err(self.fail("unknown Messages block exceeded the byte limit"));
        }
        let block = if has_protected_material(&value)
            || ["signed", "encrypted", "redacted", "thinking"]
                .iter()
                .any(|marker| kind.contains(marker))
        {
            let index = self.reserve_output_index()?;
            self.output_indices.insert(wire, index);
            OpenBlock::Native { block: raw }
        } else {
            self.skipped_blocks += 1;
            OpenBlock::Skipped {
                bytes: raw.get().len(),
            }
        };
        self.open_blocks.insert(wire, block);
        self.next_block_index += 1;
        Ok(())
    }

    pub(super) fn native_content(
        &self,
        target: Option<u32>,
        block: &RawValue,
    ) -> Result<ContentBlock, MessagesStreamError> {
        let raw = serde_json::value::to_raw_value(&NativeReplay {
            target,
            block: block.get().to_owned(),
        })
        .map_err(|_| invalid_response("invalid Messages native replay"))?;
        let model = self
            .reported_model
            .clone()
            .ok_or_else(|| invalid_response("missing reported model"))?;
        let replay = self.prepared.bind_replay(model, NATIVE_REPLAY_KIND, raw)?;
        Ok(ContentBlock::Opaque(
            OpaqueBlock::new(replay, OpaqueFallback::CompatibleReplay)
                .map_err(|_| invalid_response("invalid Messages native replay"))?,
        ))
    }

    pub(super) fn complete_citations(
        &mut self,
        output: &mut Vec<ModelEvent>,
    ) -> Result<(), MessagesStreamError> {
        let item = self.item_id()?;
        for super::CitedText {
            target,
            native,
            citations,
        } in std::mem::take(&mut self.cited_text)
        {
            for (annotation, raw) in citations.iter().enumerate() {
                let value: Value = serde_json::from_str(raw.get())
                    .map_err(|_| invalid_response("invalid Messages citation"))?;
                if !value.is_object() || !value["type"].is_string() {
                    return Err(self.fail("Messages citation must be a typed object"));
                }
                let uri = value["url"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned);
                let source_id = uri.as_ref().map_or_else(
                    || {
                        value.get("document_index").map_or_else(
                            || format!("messages:source:{target}:{annotation}"),
                            |index| format!("messages:document:{index}"),
                        )
                    },
                    |uri| {
                        format!(
                            "messages:url:{}",
                            plakous_model::sha256_hex(&[uri.as_bytes()])
                        )
                    },
                );
                let title = value
                    .get("title")
                    .or_else(|| value.get("document_title"))
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned);
                let source = CitationSource::new(
                    source_id,
                    format!("{item}:{target}:{annotation}"),
                    uri,
                    title,
                )
                .map_err(|_| invalid_response("invalid Messages citation source"))?;
                // Native page/character/block offsets locate the source document,
                // not the generated answer. Do not invent an answer-text range.
                let citation = CitationBlock::new(CitationTarget::new(item, target), source, None);
                let index = self.reserve_output_index()?;
                self.blocks
                    .insert(index, ContentBlock::Citation(citation.clone()));
                output.push(ModelEvent::CitationAdded(CitationAdded::new(
                    item,
                    OutputLocation::new(0, index),
                    citation,
                )));
            }
            let block = self.native_content(Some(target), &native)?;
            let index = self.reserve_output_index()?;
            self.blocks.insert(index, block.clone());
            let location = OutputLocation::new(0, index);
            output.push(ModelEvent::BlockStarted(BlockStarted::new(
                item,
                location,
                BlockStart::Complete(block),
            )));
        }
        Ok(())
    }

    pub(super) fn skipped_diagnostic(&self, output: &mut Vec<ModelEvent>) {
        if self.skipped_blocks != 0 {
            output.push(ModelEvent::Diagnostic(ModelDiagnostic::new(format!(
                "Messages skipped {} unfamiliar informational content block(s).",
                self.skipped_blocks
            ))));
        }
    }

    pub(super) fn retain_empty_skip_marker(
        &mut self,
        output: &mut Vec<ModelEvent>,
    ) -> Result<(), MessagesStreamError> {
        if !self.blocks.is_empty() || self.skipped_blocks == 0 {
            return Ok(());
        }
        // A successful response needs canonical output, but skipped provider
        // content must not become invented answer text or native request input.
        let payload =
            serde_json::value::to_raw_value(&serde_json::json!({"count":self.skipped_blocks}))
                .map_err(|_| invalid_response("invalid Messages skipped-block count"))?;
        let model = self
            .reported_model
            .clone()
            .ok_or_else(|| invalid_response("missing reported model"))?;
        let replay = self.prepared.bind_replay(
            model,
            crate::request::SKIPPED_BLOCKS_REPLAY_KIND,
            payload,
        )?;
        let block = ContentBlock::Opaque(
            OpaqueBlock::new(replay, OpaqueFallback::Omit)
                .map_err(|_| invalid_response("invalid Messages skipped-block marker"))?,
        );
        let index = self.reserve_output_index()?;
        self.blocks.insert(index, block.clone());
        output.push(ModelEvent::BlockStarted(BlockStarted::new(
            self.item_id()?,
            OutputLocation::new(0, index),
            BlockStart::Complete(block),
        )));
        Ok(())
    }
}
