use super::{Arc, CliResult, Credentials};
use crate::CliFailure;
use crate::platform::UnavailableCredentials;
impl Credentials for UnavailableCredentials {
    fn xai(&self) -> CliResult<Arc<dyn plakous::xai::XaiPlatformApiKeySource>> {
        Err(CliFailure::new("xAI API key is unavailable"))
    }
}
