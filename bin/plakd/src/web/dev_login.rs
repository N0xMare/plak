//! Unsupported loopback development access, never linked into the default release.

use crate::web::WebAccess;
use plakous_hosted::error::CliResult;

pub(crate) fn access(address: &std::net::SocketAddr, login: String) -> CliResult<WebAccess> {
    Ok(WebAccess::Dev {
        login,
        origin: crate::web::WebOrigin::parse(&format!("http://{address}"))?,
    })
}
