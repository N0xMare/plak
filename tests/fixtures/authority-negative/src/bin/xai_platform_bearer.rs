use plakous::xai::XaiPlatformApiKey;

fn forbidden(value: &XaiPlatformApiKey) {
    let _ = value.bearer();
}

fn main() {}
