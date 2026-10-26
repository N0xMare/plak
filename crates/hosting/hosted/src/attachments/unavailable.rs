use super::NormalizeError;

pub fn normalize(
    _name: String,
    _bytes: Vec<u8>,
) -> Result<plakous_host_protocol::DraftImage, NormalizeError> {
    Err(unavailable())
}

pub fn normalize_rgba(
    _name: String,
    _width: u32,
    _height: u32,
    _bytes: Vec<u8>,
) -> Result<plakous_host_protocol::DraftImage, NormalizeError> {
    Err(unavailable())
}

fn unavailable() -> NormalizeError {
    NormalizeError::Unavailable(crate::capabilities::CapabilityUnavailable {
        id: "images",
        kind: crate::capabilities::CapabilityKind::Surface,
        feature: "images",
    })
}
