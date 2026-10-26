use super::{Arc, CliFailure, CliResult, Credentials};
use crate::platform::UnavailableCredentials;
impl Credentials for UnavailableCredentials {
    fn openai(&self) -> CliResult<Arc<dyn plakous::openai::PlatformCredentialSource>> {
        Err(CliFailure::new("OpenAI API key is unavailable"))
    }
}
