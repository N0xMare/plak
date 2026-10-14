use plakous::xai::XaiPlatformCredentialGrant;

fn forbidden(value: &XaiPlatformCredentialGrant) {
    let _ = value.execution_authority();
}

fn main() {}
