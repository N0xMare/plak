use super::{Arc, CliFailure, CliResult, Credentials};
use crate::platform::UnavailableCredentials;
impl Credentials for UnavailableCredentials {
    fn openai(&self) -> CliResult<Arc<plakous::openai::OpenAiKeySource>> {
        Err(CliFailure::new("OpenAI API key is unavailable"))
    }
}
