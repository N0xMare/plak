//! Length-prefixed frames: a `u32` big-endian body length, then one UTF-8
//! JSON body, as the cell worker protocol frames its messages. A declared
//! length over [`MAX_FRAME_BYTES`] is refused before any body byte is held.
//!
//! [`FrameReader`] is incremental and does no I/O, so either end can feed it
//! from whatever byte stream it reads.

use std::fmt;

/// The largest body one frame may carry, excluding its four-byte length.
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// Why a frame could not be encoded or decoded.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum FrameError {
    /// The body exceeds [`MAX_FRAME_BYTES`].
    TooLarge,
    /// The value could not be encoded as JSON.
    Encode,
    /// The body is not a valid frame of the expected type.
    Decode(String),
}

impl fmt::Display for FrameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge => formatter.write_str("runner frame exceeds its size limit"),
            Self::Encode => formatter.write_str("runner frame could not be encoded"),
            Self::Decode(detail) => write!(formatter, "runner frame is malformed: {detail}"),
        }
    }
}

impl std::error::Error for FrameError {}

/// Encodes one frame: the length prefix, then the JSON body.
///
/// # Errors
/// Refuses a value JSON cannot encode or a body over [`MAX_FRAME_BYTES`].
pub fn encode(value: &impl serde::Serialize) -> Result<Vec<u8>, FrameError> {
    let body = serde_json::to_vec(value).map_err(|_| FrameError::Encode)?;
    if body.len() > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge);
    }
    let length = u32::try_from(body.len()).map_err(|_| FrameError::TooLarge)?;
    let mut framed = Vec::with_capacity(body.len() + 4);
    framed.extend_from_slice(&length.to_be_bytes());
    framed.extend_from_slice(&body);
    Ok(framed)
}

/// Decodes one frame body.
///
/// # Errors
/// Refuses a body that is not a valid `T`.
pub fn decode<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, FrameError> {
    serde_json::from_slice(body).map_err(|error| FrameError::Decode(error.to_string()))
}

/// Splits a byte stream into frame bodies.
#[derive(Debug, Default)]
pub struct FrameReader {
    buffered: Vec<u8>,
}

impl FrameReader {
    /// An empty reader.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends bytes read from the stream.
    pub fn push(&mut self, bytes: &[u8]) {
        self.buffered.extend_from_slice(bytes);
    }

    /// The next complete frame body, if one has arrived.
    ///
    /// # Errors
    /// Refuses a declared length over [`MAX_FRAME_BYTES`]; the stream is then
    /// unusable.
    pub fn next_frame(&mut self) -> Result<Option<Vec<u8>>, FrameError> {
        let Some(prefix) = self.buffered.get(..4) else {
            return Ok(None);
        };
        let mut length = [0_u8; 4];
        length.copy_from_slice(prefix);
        let length =
            usize::try_from(u32::from_be_bytes(length)).map_err(|_| FrameError::TooLarge)?;
        if length > MAX_FRAME_BYTES {
            return Err(FrameError::TooLarge);
        }
        if self.buffered.len() < 4 + length {
            return Ok(None);
        }
        let body = self.buffered[4..4 + length].to_vec();
        self.buffered.drain(..4 + length);
        Ok(Some(body))
    }

    /// Whether bytes of an incomplete frame are held: a stream that ends
    /// here ended inside a frame.
    #[must_use]
    pub fn has_partial_frame(&self) -> bool {
        !self.buffered.is_empty()
    }
}
