//! Exact supported inventory and workflows for the root facade.
//!
//! Transitional public nameability outside these modules is not certified as
//! supported by this fixture.

mod configuration;
mod control_observation;
mod custom_model;
mod custom_tool;
mod inventory;
mod lifecycle;
mod local_composition;
mod providers;
mod sessions;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    assert_eq!(plakous::VERSION, "0.0.2");
    configuration::surface_is_nameable();
    control_observation::surface_is_nameable();
    custom_model::surface_is_nameable();
    let _registered = custom_model::register_custom_model().expect("custom model registers");
    custom_tool::surface_is_nameable();
    let _tools = custom_tool::register_custom_tool().expect("custom tool registers");
    local_composition::surface_is_nameable();
    sessions::surface_is_nameable();
    providers::surface_is_nameable();
    inventory_workflows().await?;
    lifecycle::run().await?;
    Ok(())
}

async fn inventory_workflows() -> Result<(), Box<dyn std::error::Error>> {
    use inventory::codex_local_composition_reconstruction as reconstruction;

    inventory::construction_helpers_are_supported_facade_workflows();
    inventory::application_accepts_caller_supplied_cancellation_without_subscription_features();
    inventory::local_composition_attachment_is_a_capability_poor_facade_workflow();
    reconstruction::four_and_seven_tool_profiles_reconstruct_exactly_and_refuse_cross_profile_substitution()?;
    reconstruction::rquickjs_sidecar_attaches_minimal_code_mode_profile()?;
    inventory::exact_supported_root_inventory_is_importable();
    inventory::exact_context_source_workflow_is_nameable_through_the_facade();
    inventory::agent_workflow_is_nameable_through_the_facade();
    inventory::exact_session_workflow_is_nameable_through_the_facade();
    inventory::public_memory_session_identities_are_exact_and_portable();
    inventory::change_feed_replaces_receipt_reconciliation_in_supported_inventory().await;
    inventory::sqlite_persistence_admission_distinguishes_unsupported_from_corrupt();
    inventory::transcript_cursor_types_remain_on_curated_agent_surface();
    inventory::agent_event_and_cursor_serde_round_trip();
    inventory::auth_module_exists_without_existing_login();
    inventory::curated_agent_telemetry_surface_is_importable();
    inventory::curated_provider_namespaces_are_nameable_without_unqualified_lanes();
    inventory::openrouter_facade_is_custom_bounded_and_redacted()?;
    inventory::curated_subscription_models_are_nameable();
    Ok(())
}
