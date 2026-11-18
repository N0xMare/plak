//! Relay response frames.
//!
//! Once a request passes validation the gateway answers `200` and streams
//! frames, each a type byte, a `u32` big-endian payload length and the
//! payload:
//!
//! ```text
//! 0x01 Head      JSON   the upstream status and headers, unchanged
//! 0x02 Data      binary seq u64be | offset u64be | upstream body bytes
//! 0x03 Trailers  JSON   upstream trailers, when there are any
//! 0x04 End       JSON   how the exchange ended
//! ```
//!
//! `Head` or `End` comes first; `Data` only after `Head`, with `seq` dense
//! from 0 and `offset` the upstream body offset; `Trailers` at most once
//! after `Head`; `End` last. For an event stream the gateway sends one data
//! frame per event, splitting only an event over [`MAX_DATA_BYTES`]. There is
//! no keepalive frame and no empty data frame, so silence on the relay is
//! silence upstream.
//! [`FrameDecoder`] enforces this order and every bound; it does no I/O.

use std::fmt;

use crate::header::{WireHeader, within_limits};

/// The most upstream body bytes one data frame carries.
pub const MAX_DATA_BYTES: usize = 1024 * 1024;
/// The largest JSON frame payload.
pub const MAX_CONTROL_FRAME_BYTES: usize = 256 * 1024;
/// The most response header or trailer fields one frame carries.
pub const MAX_RESPONSE_HEADERS: usize = 128;
/// The most encoded response header bytes one frame carries.
pub const MAX_RESPONSE_HEADER_BYTES: usize = 64 * 1024;

const FRAME_HEADER_BYTES: usize = 5;
const DATA_PREFIX_BYTES: usize = 16;
/// The most undecoded bytes a [`FrameDecoder`] holds.
pub const MAX_BUFFERED_BYTES: usize = 2 * (FRAME_HEADER_BYTES + DATA_PREFIX_BYTES + MAX_DATA_BYTES);

const HEAD: u8 = 0x01;
const DATA: u8 = 0x02;
const TRAILERS: u8 = 0x03;
const END: u8 = 0x04;

/// Why a frame could not be encoded or a frame stream was refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameError {
    /// The frame type byte is not one this version defines.
    UnknownType,
    /// A payload exceeds its type's limit, or a data frame is empty.
    Size,
    /// A JSON payload is malformed or holds an invalid value.
    Json,
    /// The frame could not be encoded.
    Encode,
    /// A frame arrived out of the stream order.
    Order,
    /// A data frame's `seq` or `offset` does not continue the stream.
    Sequence,
    /// The upstream body exceeds the decoder's response limit.
    ResponseTooLarge,
    /// The caller pushed more bytes than [`MAX_BUFFERED_BYTES`] before
    /// draining frames.
    BufferFull,
    /// The stream ended before `End`, or bytes follow `End`.
    Truncated,
    /// An earlier error already refused this stream.
    Failed,
}

impl fmt::Display for FrameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::UnknownType => "relay frame type is unknown",
            Self::Size => "relay frame size is invalid",
            Self::Json => "relay frame payload is malformed",
            Self::Encode => "relay frame could not be encoded",
            Self::Order => "relay frame is out of order",
            Self::Sequence => "relay data frame does not continue the stream",
            Self::ResponseTooLarge => "relay response exceeds its size limit",
            Self::BufferFull => "relay frame buffer is full",
            Self::Truncated => "relay frame stream is truncated",
            Self::Failed => "relay frame stream was already refused",
        })
    }
}

impl std::error::Error for FrameError {}

/// The upstream response head, unchanged except for hop-by-hop fields.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Head {
    /// The upstream HTTP status, `100..=599`.
    pub status: u16,
    /// The upstream end-to-end headers, in order.
    pub headers: Vec<WireHeader>,
}

/// Upstream body bytes.
#[derive(Clone, Eq, PartialEq)]
pub struct Data {
    /// The frame's place among data frames, from 0.
    pub seq: u64,
    /// The upstream body offset of the first byte.
    pub offset: u64,
    /// The bytes, at most [`MAX_DATA_BYTES`] and never empty.
    pub bytes: Vec<u8>,
}

impl fmt::Debug for Data {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Data")
            .field("seq", &self.seq)
            .field("offset", &self.offset)
            .field("len", &self.bytes.len())
            .finish_non_exhaustive()
    }
}

/// Upstream trailers.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Trailers {
    /// The trailer fields, in order.
    pub headers: Vec<WireHeader>,
}

/// The transport phase of an upstream failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WirePhase {
    /// Hostname resolution.
    Resolution,
    /// TCP connection establishment.
    Connect,
    /// TLS or negotiated HTTP connection establishment.
    Tls,
    /// Request head or body transmission.
    RequestWrite,
    /// Request transmission or response-head wait, not distinguishable.
    RequestWriteOrResponseHead,
    /// Waiting for or parsing the response head.
    ResponseHead,
    /// Pulling or validating response body bytes.
    ResponseBody,
    /// Stopping and joining connection work.
    Cleanup,
}

/// How far an upstream request got.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireTransmission {
    /// No request bytes reached the upstream connection.
    NotStarted,
    /// The upstream peer cannot hold the complete request.
    RequestIncomplete,
    /// The whole request may have reached the upstream peer.
    MayHaveTransmitted,
}

/// The class of an upstream failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireFailureKind {
    /// Cancellation was observed.
    Cancelled,
    /// A deadline elapsed.
    DeadlineExceeded,
    /// A resource limit was exceeded.
    LimitExceeded,
    /// Name resolution failed.
    Resolution,
    /// The address policy refused every address.
    AddressRefused,
    /// TCP I/O failed.
    Io,
    /// TLS negotiation or verification failed.
    Tls,
    /// HTTP framing or protocol processing failed.
    Protocol,
    /// The upstream response was structurally unsupported.
    InvalidResponse,
    /// A transport task failed unexpectedly.
    Internal,
}

/// How a relayed exchange ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum RelayEnd {
    /// The upstream body ended cleanly.
    Complete {},
    /// The upstream exchange failed.
    UpstreamFailed {
        /// Where it failed.
        phase: WirePhase,
        /// How far the request got.
        transmission: WireTransmission,
        /// What failed.
        kind: WireFailureKind,
        /// Whether a timeout caused it.
        timeout: bool,
    },
    /// The gateway's kill switch cut the exchange.
    Cancelled {},
    /// The gateway failed internally after the upstream exchange started.
    GatewayFailed {},
}

/// One relay response frame.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Frame {
    /// The upstream response head.
    Head(Head),
    /// Upstream body bytes.
    Data(Data),
    /// Upstream trailers.
    Trailers(Trailers),
    /// The end of the exchange.
    End(RelayEnd),
}

impl Frame {
    /// Encodes the frame.
    ///
    /// # Errors
    /// Refuses an empty or oversized data frame, an invalid head or trailer
    /// list, and a payload over its limit.
    pub fn encode(&self) -> Result<Vec<u8>, FrameError> {
        let (kind, payload) = match self {
            Self::Head(frame) => {
                check_head(frame)?;
                (HEAD, json(frame)?)
            }
            Self::Data(frame) => {
                if frame.bytes.is_empty() || frame.bytes.len() > MAX_DATA_BYTES {
                    return Err(FrameError::Size);
                }
                let mut payload = Vec::with_capacity(DATA_PREFIX_BYTES + frame.bytes.len());
                payload.extend_from_slice(&frame.seq.to_be_bytes());
                payload.extend_from_slice(&frame.offset.to_be_bytes());
                payload.extend_from_slice(&frame.bytes);
                (DATA, payload)
            }
            Self::Trailers(frame) => {
                check_fields(&frame.headers)?;
                (TRAILERS, json(frame)?)
            }
            Self::End(frame) => (END, json(frame)?),
        };
        let length = u32::try_from(payload.len()).map_err(|_| FrameError::Size)?;
        let mut encoded = Vec::with_capacity(FRAME_HEADER_BYTES + payload.len());
        encoded.push(kind);
        encoded.extend_from_slice(&length.to_be_bytes());
        encoded.extend_from_slice(&payload);
        Ok(encoded)
    }
}

fn json(value: &impl serde::Serialize) -> Result<Vec<u8>, FrameError> {
    let payload = serde_json::to_vec(value).map_err(|_| FrameError::Encode)?;
    if payload.len() > MAX_CONTROL_FRAME_BYTES {
        return Err(FrameError::Size);
    }
    Ok(payload)
}

fn check_fields(headers: &[WireHeader]) -> Result<(), FrameError> {
    if within_limits(headers, MAX_RESPONSE_HEADERS, MAX_RESPONSE_HEADER_BYTES) {
        Ok(())
    } else {
        Err(FrameError::Json)
    }
}

fn check_head(head: &Head) -> Result<(), FrameError> {
    if !(100..=599).contains(&head.status) {
        return Err(FrameError::Json);
    }
    check_fields(&head.headers)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StreamState {
    AwaitHead,
    Body { trailed: bool },
    Ended,
    Failed,
}

/// An incremental, bounded decoder for one relay response stream.
pub struct FrameDecoder {
    buffer: Vec<u8>,
    start: usize,
    state: StreamState,
    next_seq: u64,
    next_offset: u64,
    max_response_bytes: u64,
}

impl FrameDecoder {
    /// A decoder that refuses an upstream body over `max_response_bytes`.
    #[must_use]
    pub const fn new(max_response_bytes: u64) -> Self {
        Self {
            buffer: Vec::new(),
            start: 0,
            state: StreamState::AwaitHead,
            next_seq: 0,
            next_offset: 0,
            max_response_bytes,
        }
    }

    /// Appends bytes read from the relay response body.
    ///
    /// # Errors
    /// Refuses bytes that would leave more than [`MAX_BUFFERED_BYTES`]
    /// undecoded; drain frames with [`Self::next_frame`] between pushes.
    pub fn push(&mut self, bytes: &[u8]) -> Result<(), FrameError> {
        if self.state == StreamState::Failed {
            return Err(FrameError::Failed);
        }
        self.buffer.drain(..self.start);
        self.start = 0;
        if self.buffer.len() + bytes.len() > MAX_BUFFERED_BYTES {
            return Err(self.fail(FrameError::BufferFull));
        }
        self.buffer.extend_from_slice(bytes);
        Ok(())
    }

    /// The next complete frame, if one has arrived.
    ///
    /// # Errors
    /// Refuses an unknown type, a bad size or payload, a frame out of order
    /// or sequence, an oversized response, and bytes after `End`. After an
    /// error every call returns [`FrameError::Failed`].
    pub fn next_frame(&mut self) -> Result<Option<Frame>, FrameError> {
        match self.state {
            StreamState::Failed => return Err(FrameError::Failed),
            StreamState::Ended if self.start < self.buffer.len() => {
                return Err(self.fail(FrameError::Truncated));
            }
            _ => {}
        }
        let Some(header) = self.buffer.get(self.start..self.start + FRAME_HEADER_BYTES) else {
            return Ok(None);
        };
        let kind = header[0];
        let declared = u32::from_be_bytes([header[1], header[2], header[3], header[4]]);
        let length = match usize::try_from(declared)
            .map_err(|_| FrameError::Size)
            .and_then(|length| check_size(kind, length).map(|()| length))
        {
            Ok(length) => length,
            Err(error) => return Err(self.fail(error)),
        };
        let body_start = self.start + FRAME_HEADER_BYTES;
        let Some(payload) = self.buffer.get(body_start..body_start + length) else {
            return Ok(None);
        };
        let decoded = decode_payload(kind, payload);
        self.start = body_start + length;
        match decoded.and_then(|frame| self.accept(frame)) {
            Ok(frame) => Ok(Some(frame)),
            Err(error) => Err(self.fail(error)),
        }
    }

    /// Whether `End` has been decoded.
    #[must_use]
    pub fn is_ended(&self) -> bool {
        self.state == StreamState::Ended
    }

    /// Checks the stream at the end of the relay response body.
    ///
    /// # Errors
    /// Refuses a stream that ended before `End` or with bytes after it.
    pub fn finish(&self) -> Result<(), FrameError> {
        match self.state {
            StreamState::Failed => Err(FrameError::Failed),
            StreamState::Ended if self.start == self.buffer.len() => Ok(()),
            _ => Err(FrameError::Truncated),
        }
    }

    fn fail(&mut self, error: FrameError) -> FrameError {
        self.state = StreamState::Failed;
        self.buffer = Vec::new();
        self.start = 0;
        error
    }

    fn accept(&mut self, frame: Frame) -> Result<Frame, FrameError> {
        self.state = match (self.state, &frame) {
            (StreamState::AwaitHead, Frame::Head(_)) => StreamState::Body { trailed: false },
            (StreamState::Body { trailed: false }, Frame::Data(data)) => {
                self.continue_body(data)?;
                StreamState::Body { trailed: false }
            }
            (StreamState::Body { trailed: false }, Frame::Trailers(_)) => {
                StreamState::Body { trailed: true }
            }
            (StreamState::AwaitHead | StreamState::Body { .. }, Frame::End(_)) => {
                StreamState::Ended
            }
            _ => return Err(FrameError::Order),
        };
        Ok(frame)
    }

    fn continue_body(&mut self, data: &Data) -> Result<(), FrameError> {
        if data.seq != self.next_seq || data.offset != self.next_offset {
            return Err(FrameError::Sequence);
        }
        let length = u64::try_from(data.bytes.len()).map_err(|_| FrameError::Size)?;
        let next_offset = self
            .next_offset
            .checked_add(length)
            .filter(|total| *total <= self.max_response_bytes)
            .ok_or(FrameError::ResponseTooLarge)?;
        self.next_seq = self.next_seq.checked_add(1).ok_or(FrameError::Sequence)?;
        self.next_offset = next_offset;
        Ok(())
    }
}

/// Buffered bytes may be upstream content, so only positions print.
impl fmt::Debug for FrameDecoder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FrameDecoder")
            .field("state", &self.state)
            .field("next_seq", &self.next_seq)
            .field("next_offset", &self.next_offset)
            .field("buffered", &(self.buffer.len() - self.start))
            .finish_non_exhaustive()
    }
}

/// Refuses a declared size before any payload byte is held.
fn check_size(kind: u8, length: usize) -> Result<(), FrameError> {
    let valid = match kind {
        HEAD | TRAILERS | END => length <= MAX_CONTROL_FRAME_BYTES,
        DATA => length > DATA_PREFIX_BYTES && length <= DATA_PREFIX_BYTES + MAX_DATA_BYTES,
        _ => return Err(FrameError::UnknownType),
    };
    if valid { Ok(()) } else { Err(FrameError::Size) }
}

fn decode_payload(kind: u8, payload: &[u8]) -> Result<Frame, FrameError> {
    fn parse<T: serde::de::DeserializeOwned>(payload: &[u8]) -> Result<T, FrameError> {
        serde_json::from_slice(payload).map_err(|_| FrameError::Json)
    }
    match kind {
        HEAD => {
            let head: Head = parse(payload)?;
            check_head(&head)?;
            Ok(Frame::Head(head))
        }
        DATA => {
            let (prefix, bytes) = payload
                .split_first_chunk::<DATA_PREFIX_BYTES>()
                .ok_or(FrameError::Size)?;
            let mut seq = [0_u8; 8];
            let mut offset = [0_u8; 8];
            seq.copy_from_slice(&prefix[..8]);
            offset.copy_from_slice(&prefix[8..]);
            Ok(Frame::Data(Data {
                seq: u64::from_be_bytes(seq),
                offset: u64::from_be_bytes(offset),
                bytes: bytes.to_vec(),
            }))
        }
        TRAILERS => {
            let trailers: Trailers = parse(payload)?;
            check_fields(&trailers.headers)?;
            Ok(Frame::Trailers(trailers))
        }
        END => parse(payload).map(Frame::End),
        _ => Err(FrameError::UnknownType),
    }
}
