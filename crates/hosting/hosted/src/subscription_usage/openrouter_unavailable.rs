use super::{OpenRouterKeyUsage, UsageVertical, VerticalUsageView};
pub(super) fn project(_: UsageVertical, usage: &OpenRouterKeyUsage, _: &str) -> VerticalUsageView {
    match *usage {}
}
