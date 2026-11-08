//! Default releases have no development-login access path.

use plakous_hosted::error::{CliFailure, CliResult};

pub(crate) fn access(_: &std::net::SocketAddr, _: String) -> CliResult<crate::web::WebAccess> {
    Err(CliFailure::new(
        "plak host: --dev-login requires a maintainer or test-hooks build; it is not supported in the release product",
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "default-release capability fixture")]
mod tests {
    #[test]
    fn release_dev_login_is_refused_with_a_build_capability_remedy() {
        let failure = super::access(&"127.0.0.1:0".parse().unwrap(), "login".into())
            .err()
            .unwrap();
        assert_eq!(
            failure.message(),
            "plak host: --dev-login requires a maintainer or test-hooks build; it is not supported in the release product"
        );
    }
}
