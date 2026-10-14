//! Exact supported inventory and workflows for the root facade.
//!
//! Transitional public nameability outside these modules is not certified as
//! supported by this fixture.

mod configuration;
mod control_observation;
mod custom_model;
mod custom_tool;
mod lifecycle;
mod local_composition;
mod providers;
mod sessions;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    assert_eq!(plakous::VERSION, "0.1.0");
    configuration::surface_is_nameable();
    control_observation::surface_is_nameable();
    custom_model::surface_is_nameable();
    let _registered = custom_model::register_custom_model().expect("custom model registers");
    custom_tool::surface_is_nameable();
    let _tools = custom_tool::register_custom_tool().expect("custom tool registers");
    local_composition::surface_is_nameable();
    sessions::surface_is_nameable();
    providers::surface_is_nameable();
    lifecycle::run().await?;
    Ok(())
}
