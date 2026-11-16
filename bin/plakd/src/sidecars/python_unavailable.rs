pub fn locate() -> Result<std::path::PathBuf, String> {
    Err(plakous_hosted::capabilities::CapabilityUnavailable {
        id: "py-ptc",
        kind: plakous_hosted::capabilities::CapabilityKind::Evaluator,
        feature: "python",
    }
    .to_string())
}

#[must_use]
pub fn python_host_hello_status() -> (String, &'static str) {
    ("not in this build".to_owned(), "monty-1.2.0")
}
