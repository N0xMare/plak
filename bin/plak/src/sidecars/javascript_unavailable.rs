pub fn locate() -> Result<std::path::PathBuf, String> {
    Err(plakous_hosted::capabilities::CapabilityUnavailable {
        id: "js-ptc",
        kind: plakous_hosted::capabilities::CapabilityKind::Evaluator,
        feature: "js",
    }
    .to_string())
}

#[must_use]
pub fn hello_status() -> (String, &'static str) {
    ("not in this build".to_owned(), "rquickjs-host")
}
