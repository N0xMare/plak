//! Product vocabulary → prompt vocabulary.
//!
//! `prompts/**` names only the neutral enums in `prompts::vocabulary`, so that
//! directory can move into `plakous-prompts` as a pure move. This
//! module is the one place the product's own enums are translated into them,
//! and the only place that has to change when either side does.

use plakous_host_protocol::topology::Tier;

use crate::binding::CompositionSelection as ProductCompositionSelection;
use plakous::prompts::vocabulary::{AgentTier, Composition};

/// How a configured composition reaches its tools.
#[must_use]
pub const fn prompt_composition(selection: ProductCompositionSelection) -> Composition {
    match selection {
        ProductCompositionSelection::Direct | ProductCompositionSelection::FollowCatalog => {
            Composition::Direct
        }
        ProductCompositionSelection::NativePtc => Composition::NativeCell,
        ProductCompositionSelection::JsPtc => Composition::JavaScriptCell,
        ProductCompositionSelection::PyPtc => Composition::PythonCell,
    }
}

/// The authority a spawn tier grants over the shared checkout.
#[must_use]
pub(crate) const fn prompt_agent_tier(tier: Tier) -> AgentTier {
    match tier {
        Tier::ReadOnly => AgentTier::ReadOnly,
        Tier::Execute => AgentTier::Execute,
        Tier::Mutate => AgentTier::Mutate,
    }
}
