use super::{Arc, CliFailure, CliResult, Credentials};
use crate::platform::UnavailableCredentials;
impl Credentials for UnavailableCredentials {
    fn anthropic(
        &self,
        _account: &crate::account::AccountRef,
    ) -> CliResult<Arc<plakous::anthropic::AnthropicKeySource>> {
        Err(CliFailure::new("Anthropic API key is unavailable"))
    }
}
