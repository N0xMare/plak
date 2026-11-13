//! Per-head source configuration, including predecessors retained by a continued line.
use plakous_hosted::binding::{CompositionSelection, TargetSelection};
use plakous_hosted::sessions::{RootProfile, SessionRecord};
use serde::{Deserialize, Serialize};

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Configuration {
    account: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    composition: CompositionSelection,
    target: TargetSelection,
    profile: Option<RootProfile>,
    frozen_tools: Option<String>,
}

impl Configuration {
    #[cfg(test)]
    pub(super) fn capture(record: &SessionRecord) -> Result<Self, String> {
        Ok(Self {
            account: record.account.clone(),
            model: record.model.clone(),
            effort: record.effort.clone(),
            composition: record.composition,
            target: record.target_selection.clone(),
            profile: record.root_profile.clone(),
            frozen_tools: record
                .frozen_tools
                .as_ref()
                .map(plakous_hosted::frozen_tools::FrozenRootTools::encode)
                .transpose()?,
        })
    }

    pub(super) fn apply(self, record: &mut SessionRecord) -> Result<(), String> {
        record.account = self.account;
        record.model = self.model;
        record.effort = self.effort;
        record.composition = self.composition;
        record.target_selection = self.target;
        record.root_profile = self.profile;
        record.frozen_tools = self
            .frozen_tools
            .map(|text| plakous_hosted::frozen_tools::FrozenRootTools::decode(&text))
            .transpose()?;
        Ok(())
    }
}
