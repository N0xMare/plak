//! Data-only harness profile construction through the component.

use plakous_agent::{
    HarnessProfileId, HarnessProfileSpec, HarnessProfileSpecBuilder, ProfileError,
};

pub(crate) fn surface_is_nameable() {
    let _ = std::any::type_name::<HarnessProfileSpec>();
}

#[allow(dead_code)]
pub(crate) fn profile() -> Result<HarnessProfileSpec, ProfileError> {
    HarnessProfileSpecBuilder::new()
        .id(HarnessProfileId::new("fixture.component")?)
        .base_instructions("component fixture")
        .build()
}
