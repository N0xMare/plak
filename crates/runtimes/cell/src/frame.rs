//! Length-prefixed frames: a `u32` big-endian body length, then one UTF-8
//! JSON body. The length is checked against [`MAX_FRAME_BYTES`] before any
//! body byte is allocated or read.

use std::io::{Read, Write};

use tokio::io::{AsyncRead, AsyncReadExt};

/// Maximum encoded body of one frame, excluding its four-byte length.
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// Encodes one frame: length prefix and JSON body, bounded while encoding so
/// an oversized value never builds an oversized buffer.
///
/// # Errors
/// Refuses a body over [`MAX_FRAME_BYTES`] or a value JSON cannot encode.
pub fn encode_frame(value: &impl serde::Serialize) -> Result<Vec<u8>, String> {
    let mut encoded = vec![0; 4];
    serde_json::to_writer(
        BoundedWriter {
            bytes: &mut encoded,
            limit: MAX_FRAME_BYTES + 4,
        },
        value,
    )
    .map_err(|_| "resource limit: cell frame bytes".to_owned())?;
    let length = u32::try_from(encoded.len() - 4)
        .map_err(|_| "resource limit: cell frame bytes".to_owned())?;
    encoded[..4].copy_from_slice(&length.to_be_bytes());
    Ok(encoded)
}

/// Reads one frame body; `None` at a clean end of stream before a frame.
///
/// # Errors
/// Refuses a declared length over `limit` before reading the body, and a
/// stream that ends inside a frame.
pub async fn read_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
    limit: usize,
) -> Result<Option<Vec<u8>>, String> {
    let mut length = [0_u8; 4];
    let mut filled = 0;
    while filled < length.len() {
        let read = match reader.read(&mut length[filled..]).await {
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.to_string()),
        };
        if read == 0 {
            return if filled == 0 {
                Ok(None)
            } else {
                Err("cell frame truncated in its length".into())
            };
        }
        filled += read;
    }
    let length = declared_length(length, limit)?;
    let mut body = vec![0; length];
    reader
        .read_exact(&mut body)
        .await
        .map_err(|_| "cell frame truncated in its body".to_owned())?;
    Ok(Some(body))
}

/// Blocking [`read_frame`] for worker processes.
///
/// # Errors
/// As [`read_frame`].
pub fn read_frame_blocking(
    reader: &mut impl Read,
    limit: usize,
) -> Result<Option<Vec<u8>>, String> {
    let mut length = [0_u8; 4];
    let mut filled = 0;
    while filled < length.len() {
        let read = match reader.read(&mut length[filled..]) {
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.to_string()),
        };
        if read == 0 {
            return if filled == 0 {
                Ok(None)
            } else {
                Err("cell frame truncated in its length".into())
            };
        }
        filled += read;
    }
    let length = declared_length(length, limit)?;
    let mut body = vec![0; length];
    reader
        .read_exact(&mut body)
        .map_err(|_| "cell frame truncated in its body".to_owned())?;
    Ok(Some(body))
}

/// Encodes and writes one frame, then flushes.
///
/// # Errors
/// As [`encode_frame`], or the writer's I/O error.
pub fn write_frame_blocking(
    writer: &mut impl Write,
    value: &impl serde::Serialize,
) -> Result<(), String> {
    let frame = encode_frame(value)?;
    writer
        .write_all(&frame)
        .and_then(|()| writer.flush())
        .map_err(|error| error.to_string())
}

fn declared_length(length: [u8; 4], limit: usize) -> Result<usize, String> {
    let length = usize::try_from(u32::from_be_bytes(length))
        .map_err(|_| "resource limit: cell frame bytes".to_owned())?;
    if length > limit {
        return Err("resource limit: cell frame bytes".into());
    }
    Ok(length)
}

struct BoundedWriter<'a> {
    bytes: &'a mut Vec<u8>,
    limit: usize,
}

impl Write for BoundedWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(std::io::Error::other("frame limit"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "bounded framing tests"
)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn a_frame_round_trips() {
        let frame = encode_frame(&serde_json::json!({"a": [1, "b"]})).expect("encode");
        let mut reader = &frame[..];
        let body = read_frame(&mut reader, MAX_FRAME_BYTES)
            .await
            .expect("read")
            .expect("frame");
        assert_eq!(&frame[4..], &body[..]);
        assert_eq!(read_frame(&mut reader, MAX_FRAME_BYTES).await, Ok(None));
    }

    #[tokio::test]
    async fn an_oversized_length_is_refused_before_the_body_is_sent() {
        let (mut writer, mut reader) = tokio::io::duplex(16);
        writer
            .write_all(&9_u32.to_be_bytes())
            .await
            .expect("length");
        // No body follows: the refusal must not wait for one.
        let refused = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            read_frame(&mut reader, 8),
        )
        .await
        .expect("prompt refusal");
        assert!(refused.expect_err("over limit").contains("resource limit"));
    }

    #[tokio::test]
    async fn the_exact_limit_is_accepted() {
        let mut bytes = 8_u32.to_be_bytes().to_vec();
        bytes.extend_from_slice(b"12345678");
        let mut reader = &bytes[..];
        assert_eq!(
            read_frame(&mut reader, 8).await,
            Ok(Some(b"12345678".to_vec()))
        );
    }

    #[tokio::test]
    async fn truncated_length_and_body_are_refused() {
        let mut reader = &[0_u8, 0][..];
        assert!(read_frame(&mut reader, 8).await.is_err());
        let mut bytes = 4_u32.to_be_bytes().to_vec();
        bytes.extend_from_slice(b"ab");
        let mut reader = &bytes[..];
        assert!(
            read_frame(&mut reader, 8)
                .await
                .expect_err("short body")
                .contains("body")
        );
    }

    #[test]
    fn encoding_refuses_an_oversized_body() {
        let text = "x".repeat(MAX_FRAME_BYTES);
        assert!(encode_frame(&text).is_err());
    }

    #[test]
    fn blocking_reads_match_async_reads() {
        let frame = encode_frame(&"hello").expect("encode");
        let mut reader = &frame[..];
        assert_eq!(
            read_frame_blocking(&mut reader, MAX_FRAME_BYTES).expect("read"),
            Some(b"\"hello\"".to_vec())
        );
        assert_eq!(read_frame_blocking(&mut reader, MAX_FRAME_BYTES), Ok(None));
    }
}
