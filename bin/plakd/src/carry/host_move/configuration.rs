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
    pub(super) fn capture_child(
        record: &SessionRecord,
        tree: &plakous_recursive_subagents::tree::ProductAgentTree,
        agent: &str,
    ) -> Result<Self, String> {
        use plakous_hosted::binding::{OpenRouterRoutingSelection, ProtocolSelection, WireBinding};
        let node = tree.node(agent).map_err(|error| error.message)?;
        let binding = node
            .template
            .as_ref()
            .and_then(|template| tree.epoch_of(agent)?.binding(template))
            .ok_or("move child has no frozen template binding")?;
        let account: plakous_hosted::account::AccountRef = node
            .account
            .parse()
            .map_err(|_| "move child account is invalid")?;
        let mut configuration = Self::capture(record)?;
        configuration.account = Some(node.account.clone());
        configuration.model = Some(node.tuple_id.clone());
        configuration.effort.clone_from(&node.effort);
        configuration.profile = None;
        configuration.frozen_tools = None;
        let selected = binding.composition.map(Into::into);
        if let Some(provider) = account.vertical().capability().native_provider() {
            configuration.composition = plakous_hosted::agent::build::spawn_child_composition(
                node.parent_composition
                    .map_or(record.composition, Into::into),
                provider,
                selected,
            )?;
            configuration.target = match &binding.processing_tier {
                plakous::model::ProcessingTierDisposition::Standard => TargetSelection::Native,
                plakous::model::ProcessingTierDisposition::ProviderNamed(tier)
                    if tier.as_str() == "priority" =>
                {
                    TargetSelection::NativePriority
                }
                plakous::model::ProcessingTierDisposition::ProviderNamed(_) => {
                    return Err("move native child has an unsupported processing tier".into());
                }
            };
        } else if let Some(provider) = account.vertical().capability().platform_provider() {
            configuration.composition = plakous_hosted::platform::api_composition(
                provider,
                &node.tuple_id,
                selected.unwrap_or(CompositionSelection::FollowCatalog),
            )
            .map_err(str::to_owned)?;
            configuration.target = TargetSelection::Platform { provider };
        } else {
            configuration.composition = selected.unwrap_or(CompositionSelection::FollowCatalog);
            if configuration.composition == CompositionSelection::NativePtc {
                return Err("move API child has an invalid native composition".into());
            }
            configuration.target = TargetSelection::OpenRouter {
                wire: WireBinding::resolve(
                    binding.protocol.map_or(ProtocolSelection::Auto, Into::into),
                ),
                routing: OpenRouterRoutingSelection::GatewayManaged,
            };
        }
        Ok(configuration)
    }

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
