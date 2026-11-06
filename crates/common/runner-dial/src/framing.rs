use crate::Error;
use plakous_runner_protocol::frame::MAX_FRAME_BYTES;
use std::fmt;

/// Noise's 65,535-byte ciphertext bound less the 16-byte authentication tag.
pub const MAX_PLAINTEXT_BYTES: usize = 65_519;

/// Splits exactly one length-plus-JSON frame without copying its contents.
/// # Errors
/// Refuses a missing prefix, mismatched length or a body over 16 MiB.
pub fn split_frame(frame: &[u8]) -> Result<impl Iterator<Item = &[u8]>, Error> {
    let prefix: [u8; 4] = frame
        .get(..4)
        .ok_or(Error::Invalid)?
        .try_into()
        .map_err(|_| Error::Invalid)?;
    let length = usize::try_from(u32::from_be_bytes(prefix)).map_err(|_| Error::Invalid)?;
    if length > MAX_FRAME_BYTES || frame.len() != length + 4 {
        return Err(Error::Invalid);
    }
    Ok(frame.chunks(MAX_PLAINTEXT_BYTES))
}

/// Incremental length-prefix reassembly. It checks the cap before retaining body bytes.
/// A failure poisons the assembler; drop it with the failed link.
#[derive(Default)]
pub struct FrameAssembler {
    buffered: Vec<u8>,
    failed: bool,
}
impl fmt::Debug for FrameAssembler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("FrameAssembler([REDACTED])")
    }
}
impl FrameAssembler {
    /// An empty assembler.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
    /// Reassembles decrypted messages into complete frame bodies in stream order.
    /// # Errors
    /// Refuses oversized chunks or declared bodies, including on split prefixes.
    pub fn push(&mut self, mut plaintext: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
        if self.failed {
            return Err(Error::Invalid);
        }
        self.failed = true;
        if plaintext.len() > MAX_PLAINTEXT_BYTES {
            return Err(Error::Invalid);
        }
        let mut frames = Vec::new();
        while !plaintext.is_empty() {
            if self.buffered.len() < 4 {
                let count = (4 - self.buffered.len()).min(plaintext.len());
                self.buffered.extend_from_slice(&plaintext[..count]);
                plaintext = &plaintext[count..];
                if self.buffered.len() < 4 {
                    break;
                }
            }
            let prefix: [u8; 4] = self.buffered[..4].try_into().map_err(|_| Error::Invalid)?;
            let length = usize::try_from(u32::from_be_bytes(prefix)).map_err(|_| Error::Invalid)?;
            if length > MAX_FRAME_BYTES {
                self.buffered.clear();
                return Err(Error::Invalid);
            }
            let count = (length + 4 - self.buffered.len()).min(plaintext.len());
            self.buffered.extend_from_slice(&plaintext[..count]);
            plaintext = &plaintext[count..];
            if self.buffered.len() == length + 4 {
                let body = self.buffered.split_off(4);
                self.buffered.clear();
                frames.push(body);
            }
        }
        self.failed = false;
        Ok(frames)
    }
    /// Checks EOF is at a frame boundary.
    /// # Errors
    /// Refuses truncated frames and poisoned assemblers.
    pub fn finish(self) -> Result<(), Error> {
        if self.failed || !self.buffered.is_empty() {
            return Err(Error::Invalid);
        }
        Ok(())
    }
}
