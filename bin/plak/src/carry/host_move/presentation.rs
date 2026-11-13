//! Export-v13 binding-family data. Never a registry descriptor or resolved plan.
use plakous::model::{ConfiguredTool, Digest, DigestDomain};
use plakous::sessions::host_move::MoveToolNesting as NestedToolEligibility;
use plakous::sessions::{FrozenSessionPresentation, SemanticSessionPresentation};
use plakous::tools::{SemanticToolRegistration, ToolExecutorBindingIdentity};
use serde::{Deserialize, Serialize};

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Presentation {
    registrations: Vec<Registration>,
    selected: Vec<definition::Exposure>,
    effective: Vec<definition::Exposure>,
}

mod definition;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Registration {
    capability: String,
    executor: String,
    binding: Digest,
    definition: definition::Definition,
    nesting: NestedToolEligibility,
}

impl Presentation {
    pub(crate) fn capture(data: SemanticSessionPresentation) -> Result<Self, String> {
        Ok(Self {
            registrations: data
                .registrations
                .into_iter()
                .map(|record| {
                    Ok(Registration {
                        capability: record.registered_id,
                        executor: record.executor_id,
                        binding: record.executor_binding.digest(),
                        definition: definition::Definition::capture(&record.definition)?,
                        nesting: record.nesting,
                    })
                })
                .collect::<Result<_, String>>()?,
            selected: data
                .selected
                .iter()
                .map(definition::Exposure::capture)
                .collect::<Result<_, _>>()?,
            effective: data
                .effective
                .iter()
                .map(definition::Exposure::capture)
                .collect::<Result<_, _>>()?,
        })
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        self.clone().decode().map(|_| ())
    }

    pub(crate) fn decode(self) -> Result<FrozenSessionPresentation, String> {
        let registrations = self
            .registrations
            .into_iter()
            .map(|record| {
                if record.binding.domain() != DigestDomain::ToolDefinition {
                    return Err("invalid move executable binding domain".to_owned());
                }
                Ok(SemanticToolRegistration {
                    registered_id: record.capability,
                    executor_id: record.executor,
                    executor_binding: ToolExecutorBindingIdentity::from_digest(record.binding),
                    definition: record.definition.decode()?,
                    nesting: record.nesting,
                })
            })
            .collect::<Result<_, _>>()?;
        FrozenSessionPresentation::from_semantic(SemanticSessionPresentation {
            registrations,
            selected: self
                .selected
                .into_iter()
                .map(definition::Exposure::decode)
                .collect::<Result<Vec<ConfiguredTool>, _>>()?,
            effective: self
                .effective
                .into_iter()
                .map(definition::Exposure::decode)
                .collect::<Result<_, _>>()?,
        })
        .map_err(|error| error.to_string())
    }
}
