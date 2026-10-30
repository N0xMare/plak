//! Pure revision-one image selection and source-bound deterministic rewriting.

pub(in crate::runtime) mod receipt;
pub(in crate::runtime) mod record;
pub(super) mod rewrite;
pub(in crate::runtime) use rewrite::{ResidencySuccessor, residency_successor};

use crate::{AgentError, AgentErrorKind};
use plakous_model::{
    Actor, CallId, CanonicalHistory, ContentBlock, ConversationItem, Digest, DigestDomain,
    HistorySegment, HistorySegmentKind, ImageBlock, ResidentImageBudget, ToolResultPart,
    sha256_bytes,
};
use serde::{Deserialize, Serialize};

pub(in crate::runtime) const POLICY_REVISION: u32 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::runtime) struct ImageTotals {
    pub count: u32,
    pub encoded_bytes: u64,
    pub unknown_bytes: u32,
}

impl ImageTotals {
    pub(in crate::runtime) const ZERO: Self = Self {
        count: 0,
        encoded_bytes: 0,
        unknown_bytes: 0,
    };

    pub(in crate::runtime) fn add_image(&mut self, image: &ImageBlock) {
        self.count = self.count.saturating_add(1);
        match image.source().encoded_image_bytes() {
            Some(bytes) => self.encoded_bytes = self.encoded_bytes.saturating_add(bytes),
            None => self.unknown_bytes = self.unknown_bytes.saturating_add(1),
        }
    }

    pub(in crate::runtime) fn fits(self, budget: ResidentImageBudget) -> bool {
        self.count <= budget.max_images() && self.encoded_bytes <= budget.max_encoded_bytes()
    }

    fn at_target(self, budget: ResidentImageBudget) -> bool {
        u128::from(self.count) * 4 <= u128::from(budget.max_images()) * 3
            && u128::from(self.encoded_bytes) * 4 <= u128::from(budget.max_encoded_bytes()) * 3
    }

    fn remove(&mut self, selected: &ImageSelection) {
        self.count -= 1;
        match selected.encoded_bytes {
            Some(bytes) => self.encoded_bytes -= bytes,
            None => self.unknown_bytes -= 1,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(in crate::runtime) enum ResidencyTrigger {
    ImageBudget,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(in crate::runtime) enum ImageSelectionKind {
    UserImage,
    ToolResultImage,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::runtime) struct ImageSelection {
    pub segment: u32,
    pub item: u32,
    pub block: u32,
    pub part: Option<u32>,
    pub kind: ImageSelectionKind,
    pub call_id: Option<CallId>,
    pub raw_bytes: Option<u64>,
    pub sha256: Option<[u8; 32]>,
    pub encoded_bytes: Option<u64>,
}

impl ImageSelection {
    fn new(
        location: (usize, usize, usize, Option<usize>),
        image: &ImageBlock,
        call_id: Option<CallId>,
    ) -> Result<Self, AgentError> {
        let (segment, item, block, part) = location;
        Ok(Self {
            segment: checked_count(segment)?,
            item: checked_count(item)?,
            block: checked_count(block)?,
            part: part.map(checked_count).transpose()?,
            kind: if part.is_some() {
                ImageSelectionKind::ToolResultImage
            } else {
                ImageSelectionKind::UserImage
            },
            call_id,
            raw_bytes: image.source().as_bytes().map(|bytes| bytes.len() as u64),
            sha256: image
                .source()
                .as_bytes()
                .map(|bytes| sha256_bytes(&[bytes])),
            encoded_bytes: image.source().encoded_image_bytes(),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::runtime) struct ResidencyPlan {
    pub policy_revision: u32,
    pub trigger: ResidencyTrigger,
    pub max_images: u32,
    pub max_encoded_bytes: u64,
    /// Pending admission attachments are protected and not yet in source history.
    pub incoming: ImageTotals,
    pub before: ImageTotals,
    pub after: ImageTotals,
    pub selection: Vec<ImageSelection>,
}

impl ResidencyPlan {
    pub(in crate::runtime) fn identity(&self) -> Result<Digest, AgentError> {
        let bytes = serde_json::to_vec(self)
            .map_err(|_| invalid("image residency plan encoding failed"))?;
        Digest::sha256(
            DigestDomain::ContextPlan,
            1,
            &[b"plakous-agent/image-residency-selection-v1", &bytes],
        )
        .map_err(|_| invalid("image residency selection identity failed"))
    }

    pub(in crate::runtime) fn validate_source(
        &self,
        source: &CanonicalHistory,
    ) -> Result<(), AgentError> {
        let budget = ResidentImageBudget::new(self.max_images, self.max_encoded_bytes)
            .map_err(|error| invalid(&error.to_string()))?;
        let recomputed = evaluate(source, self.incoming, budget)?;
        if recomputed.as_ref() != Some(self) {
            return Err(invalid(
                "image residency selection does not match its source policy",
            ));
        }
        Ok(())
    }
}

pub(in crate::runtime) fn image_totals(
    history: &CanonicalHistory,
    incoming: ImageTotals,
) -> ImageTotals {
    let mut totals = incoming;
    for block in history
        .segments()
        .iter()
        .flat_map(HistorySegment::items)
        .flat_map(ConversationItem::blocks)
    {
        match block {
            ContentBlock::Image(image) => totals.add_image(image),
            ContentBlock::ToolResult(result) => {
                for part in result.parts() {
                    if let ToolResultPart::Image(image) = part {
                        totals.add_image(image);
                    }
                }
            }
            _ => {}
        }
    }
    totals
}

/// The 75% threshold is a batching target, not a validity bound. If it is
/// unreachable, the maximum protected-policy-legal batch must still fit 100%.
pub(in crate::runtime) fn evaluate(
    history: &CanonicalHistory,
    incoming: ImageTotals,
    budget: ResidentImageBudget,
) -> Result<Option<ResidencyPlan>, AgentError> {
    let before = image_totals(history, incoming);
    if before.fits(budget) {
        return Ok(None);
    }
    let mut candidates = eligible_images(history)?;
    candidates.truncate(candidates.len().saturating_sub(2));
    let mut after = before;
    let mut selection = Vec::new();
    for candidate in candidates {
        if after.at_target(budget) {
            break;
        }
        after.remove(&candidate);
        selection.push(candidate);
    }
    if !after.fits(budget) || selection.is_empty() {
        return Err(AgentError::new(
            AgentErrorKind::RequestBudget,
            format!(
                "resident image request budget exceeded: {} images / {} known encoded bytes remain; limits {} images / {} encoded bytes; protected images were not removed",
                after.count,
                after.encoded_bytes,
                budget.max_images(),
                budget.max_encoded_bytes()
            ),
        ));
    }
    Ok(Some(ResidencyPlan {
        policy_revision: POLICY_REVISION,
        trigger: ResidencyTrigger::ImageBudget,
        max_images: budget.max_images(),
        max_encoded_bytes: budget.max_encoded_bytes(),
        incoming,
        before,
        after,
        selection,
    }))
}

fn eligible_images(history: &CanonicalHistory) -> Result<Vec<ImageSelection>, AgentError> {
    let latest_intent = history
        .segments()
        .iter()
        .enumerate()
        .filter(|(_, segment)| is_intent(segment))
        .map(|(index, _)| index)
        .last();
    let unanswered = current_round_batch(history);
    let mut candidates = Vec::new();
    for (segment_index, segment) in history.segments().iter().enumerate() {
        if Some(segment_index) == latest_intent
            || Some(segment_index) == unanswered
            || !matches!(
                segment.kind(),
                HistorySegmentKind::Conversation
                    | HistorySegmentKind::RecentIntent
                    | HistorySegmentKind::ToolExchange
            )
        {
            continue;
        }
        for (item_index, item) in segment.items().iter().enumerate() {
            for (block_index, block) in item.blocks().iter().enumerate() {
                match block {
                    ContentBlock::Image(image) if item.actor() == Actor::User => {
                        candidates.push(ImageSelection::new(
                            (segment_index, item_index, block_index, None),
                            image,
                            None,
                        )?);
                    }
                    ContentBlock::ToolResult(result) => {
                        for (part_index, part) in result.parts().iter().enumerate() {
                            if let ToolResultPart::Image(image) = part {
                                candidates.push(ImageSelection::new(
                                    (segment_index, item_index, block_index, Some(part_index)),
                                    image,
                                    Some(result.call_id()),
                                )?);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(candidates)
}

fn is_intent(segment: &HistorySegment) -> bool {
    segment.kind() == HistorySegmentKind::RecentIntent
        || (segment.kind() == HistorySegmentKind::Conversation
            && segment
                .items()
                .iter()
                .any(|item| item.actor() == Actor::User))
}

fn current_round_batch(history: &CanonicalHistory) -> Option<usize> {
    let mut batch = None;
    for (index, segment) in history.segments().iter().enumerate() {
        if segment.kind() == HistorySegmentKind::ToolExchange {
            batch = Some(index);
        } else if matches!(
            segment.kind(),
            HistorySegmentKind::Conversation | HistorySegmentKind::ReasoningReplay
        ) && segment
            .items()
            .iter()
            .any(|item| item.actor() == Actor::Assistant)
        {
            batch = None;
        }
    }
    batch
}

pub(super) fn invalid(message: &str) -> AgentError {
    AgentError::new(AgentErrorKind::Session, message)
}

pub(in crate::runtime) fn checked_count(value: usize) -> Result<u32, AgentError> {
    u32::try_from(value).map_err(|_| invalid("image residency position or count exceeds its bound"))
}

#[cfg(test)]
mod tests;
