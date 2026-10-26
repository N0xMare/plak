use crate::{CliFailure, CliResult};
use plakous::tools::ToolRegistration;

pub(super) fn registrations(
    _: std::sync::Arc<dyn plakous::tools::web::ImageGenPort>,
) -> CliResult<Vec<ToolRegistration>> {
    Err(CliFailure::new("image_gen has no compiled provider route"))
}
