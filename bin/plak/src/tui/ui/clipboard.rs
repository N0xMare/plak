//! An image on the system clipboard (Ctrl+V, or the empty paste a terminal
//! sends for an image-only clipboard) as a PNG attachment.

use plakous_host_protocol::DraftImage;

/// Name the chip shows for a pasted clipboard image.
const CLIPBOARD_IMAGE_NAME: &str = "clipboard.png";

/// Reads the clipboard's image and normalizes it like every attachment: a
/// PNG, downscaled when it exceeds the bounds.
pub(super) fn read_image() -> Result<DraftImage, String> {
    let mut clipboard =
        arboard::Clipboard::new().map_err(|error| format!("clipboard unavailable: {error}"))?;
    let image = clipboard.get_image().map_err(|error| match error {
        arboard::Error::ContentNotAvailable => "the clipboard holds no image".to_owned(),
        other => format!("clipboard image unreadable: {other}"),
    })?;
    let dimension = |value: usize| {
        u32::try_from(value).map_err(|_| {
            format!(
                "clipboard image is too large ({}x{})",
                image.width, image.height
            )
        })
    };
    crate::config::image_attachments::normalize_rgba(
        CLIPBOARD_IMAGE_NAME.to_owned(),
        dimension(image.width)?,
        dimension(image.height)?,
        image.bytes.into_owned(),
    )
    .map_err(|error| format!("clipboard image: {error}"))
}
