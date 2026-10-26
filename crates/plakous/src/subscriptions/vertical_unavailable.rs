use super::{CompatibilityNotice, NativeProvider, NativeSubscriptionError, NativeToolMode};
pub(super) trait NativeVertical {
    fn enrolled_models(&self) -> Vec<&'static str>;
    fn enrolled_tool_mode(&self, model: &str) -> Option<NativeToolMode>;
    fn enrolls(&self, model: &str) -> bool;
    fn route_ids(
        &self,
    ) -> (
        &'static str,
        &'static str,
        &'static str,
        &'static str,
        &'static str,
        &'static str,
    );
    fn notice(&self) -> CompatibilityNotice;
}
pub(super) fn vertical(_provider: NativeProvider) -> Option<&'static dyn NativeVertical> {
    None
}
pub(super) fn require_vertical(
    provider: NativeProvider,
) -> Result<&'static dyn NativeVertical, NativeSubscriptionError> {
    Err(NativeSubscriptionError::NotCompiled(provider))
}
