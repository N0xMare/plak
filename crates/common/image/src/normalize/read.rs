use std::path::Path;

use super::normalize;
use crate::{ImageFileError, MAX_SOURCE_BYTES, NormalizedImage};

/// Reads one image file, sniffs its type from its bytes (the extension is
/// never trusted) and [`normalize`]s it into the bounds. Returns the file
/// name the image is attached under with the normalized image.
///
/// # Errors
///
/// Returns [`ImageFileError`] naming the file when it cannot be read, is not
/// a file, exceeds [`MAX_SOURCE_BYTES`] (refused unread), or is not an image
/// within reach of the bounds.
pub fn read_image(path: &Path) -> Result<(String, NormalizedImage), ImageFileError> {
    let unreadable = |source| ImageFileError::Unreadable {
        path: path.to_owned(),
        source,
    };
    let metadata = std::fs::metadata(path).map_err(unreadable)?;
    if !metadata.is_file() {
        return Err(ImageFileError::NotAFile {
            path: path.to_owned(),
        });
    }
    let too_large = |bytes| ImageFileError::TooLarge {
        path: path.to_owned(),
        bytes,
    };
    if metadata.len() > MAX_SOURCE_BYTES {
        return Err(too_large(metadata.len()));
    }
    let bytes = std::fs::read(path).map_err(unreadable)?;
    if bytes.len() as u64 > MAX_SOURCE_BYTES {
        return Err(too_large(bytes.len() as u64));
    }
    let name = path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    );
    normalize(bytes)
        .map(|image| (name, image))
        .map_err(|source| ImageFileError::Unusable {
            path: path.to_owned(),
            source,
        })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;
    use crate::MAX_IMAGE_BYTES;

    fn png_header(width: u32, height: u32, extra: usize) -> Vec<u8> {
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend_from_slice(&[0, 0, 0, 13]);
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&width.to_be_bytes());
        png.extend_from_slice(&height.to_be_bytes());
        png.resize(png.len() + extra, 0);
        png
    }

    #[test]
    fn reads_sniffs_and_names_the_file_in_every_refusal() {
        let dir = tempfile::tempdir().unwrap();
        let shot = dir.path().join("shot.dat");
        let bytes = png_header(1280, 720, 412 * 1024);
        std::fs::write(&shot, &bytes).unwrap();
        let (name, image) = read_image(&shot).unwrap();
        assert_eq!(name, "shot.dat");
        assert_eq!(image.media_type, "image/png");
        assert_eq!(image.bytes, bytes);

        let missing = dir.path().join("missing.png");
        let error = read_image(&missing).unwrap_err();
        assert!(matches!(error, ImageFileError::Unreadable { .. }));
        assert!(error.to_string().contains("missing.png"));

        let error = read_image(dir.path()).unwrap_err();
        assert!(matches!(error, ImageFileError::NotAFile { .. }));
        assert!(error.to_string().ends_with(": not a file"), "{error}");

        let text = dir.path().join("notes.png");
        std::fs::write(&text, "not an image").unwrap();
        let error = read_image(&text).unwrap_err();
        assert!(matches!(error, ImageFileError::Unusable { .. }));
        assert!(error.to_string().contains("notes.png"));

        // Over the bound, so decoded to be downscaled: a header alone fails.
        let big = dir.path().join("big.png");
        std::fs::write(&big, png_header(2, 2, MAX_IMAGE_BYTES)).unwrap();
        let error = read_image(&big).unwrap_err();
        assert!(matches!(error, ImageFileError::Unusable { .. }));
        assert!(error.to_string().contains("big.png") && error.to_string().contains("decoded"));
    }

    #[test]
    fn a_file_over_the_source_bound_is_refused_unread() {
        let dir = tempfile::tempdir().unwrap();
        let huge = dir.path().join("huge.png");
        let file = std::fs::File::create(&huge).unwrap();
        file.set_len(MAX_SOURCE_BYTES + 1).unwrap();
        let error = read_image(&huge).unwrap_err();
        assert!(
            matches!(error, ImageFileError::TooLarge { bytes, .. } if bytes == MAX_SOURCE_BYTES + 1)
        );
        assert_eq!(
            error.to_string(),
            format!(
                "image {}: 67108865 bytes exceeds the 67108864-byte (64 MiB) limit for an image file",
                huge.display()
            )
        );
    }

    #[test]
    fn a_dropped_path_is_an_image_by_its_header_alone() {
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("drop.bin");
        std::fs::write(&png, png_header(4, 3, 0)).unwrap();
        assert!(crate::is_image_file(&png));
        let text = dir.path().join("notes.png");
        std::fs::write(&text, "not an image").unwrap();
        assert!(!crate::is_image_file(&text));
        assert!(!crate::is_image_file(Path::new("drop.bin")));
        assert!(!crate::is_image_file(dir.path()));
    }
}
