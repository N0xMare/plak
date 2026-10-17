//! An image on the system clipboard (Ctrl+V, or the empty paste a terminal
//! sends for an image-only clipboard) as a PNG attachment.

use plakous_host_protocol::DraftImage;

/// Name the chip shows for a pasted clipboard image.
const CLIPBOARD_IMAGE_NAME: &str = "clipboard.png";

/// Reads the clipboard's image and normalizes it like every attachment: a
/// PNG, downscaled when it exceeds the bounds. Runs on a worker thread.
pub(super) fn read_image() -> Result<DraftImage, String> {
    // A macOS screenshot carries a PNG flavour beside a much larger TIFF:
    // the PNG is used as is when it is within the bounds.
    #[cfg(target_os = "macos")]
    if let Some(png) = png_flavour() {
        return crate::config::image_attachments::normalize(CLIPBOARD_IMAGE_NAME.to_owned(), png)
            .map_err(|error| format!("clipboard image: {error}"));
    }
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

/// The general pasteboard's `public.png` data, when it has one.
#[cfg(target_os = "macos")]
fn png_flavour() -> Option<Vec<u8>> {
    objc2::rc::autoreleasepool(|_| {
        let pasteboard = objc2_app_kit::NSPasteboard::generalPasteboard();
        let png = objc2_foundation::NSString::from_str("public.png");
        pasteboard.dataForType(&png).map(|data| data.to_vec())
    })
}
