use super::RetentionDisposition;
pub(super) fn exact_route_retention(
    selected: &plakous::model::SelectedProviderConfiguration,
) -> plakous::model::ProviderBehaviorConfiguration {
    let mut behavior = selected.behavior().clone();
    behavior.retention = RetentionDisposition::RouteContract {
        profile: selected.route().integration_profile().clone(),
        revision: selected.route().integration_revision().clone(),
    };
    behavior
}
