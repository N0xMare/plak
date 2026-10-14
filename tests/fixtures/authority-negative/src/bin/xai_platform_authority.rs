use plakous::xai::XaiPlatformApiKeyAuthority;

fn forbidden(value: &XaiPlatformApiKeyAuthority) {
    let _ = value.for_key_generation(&[]);
}

fn main() {}
