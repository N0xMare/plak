use super::{ImageFileError, NormalizeError};

pub fn normalize(
    _name: String,
    _bytes: Vec<u8>,
) -> Result<plakous_host_protocol::DraftImage, NormalizeError> {
    Err(unavailable())
}

pub fn normalize_tool(_bytes: &[u8]) -> Result<(String, Vec<u8>), NormalizeError> {
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

pub(super) fn read_image(
    _path: &std::path::Path,
) -> Result<plakous_host_protocol::DraftImage, ImageFileError> {
    Err(ImageFileError::Unavailable(capability()))
}

fn unavailable() -> NormalizeError {
    NormalizeError::Unavailable(capability())
}

fn capability() -> crate::capabilities::CapabilityUnavailable {
    crate::capabilities::CapabilityUnavailable {
        id: "images",
        kind: crate::capabilities::CapabilityKind::Surface,
        feature: "images",
    }
}
