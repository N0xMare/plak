use crate::{CliFailure, CliResult};
use plakous::tools::{ToolPack as _, ToolRegistration};

pub(super) fn registrations(
    port: std::sync::Arc<dyn plakous::tools::web::ImageGenPort>,
) -> CliResult<Vec<ToolRegistration>> {
    plakous::tools::web::ImageGenPack::new(port)
        .registrations()
        .map_err(|error| CliFailure::new(format!("image_gen: {error}")))
}
