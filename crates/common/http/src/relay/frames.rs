//! A relayed response body: the relay frames after the upstream head.

use bytes::Bytes;
use plakous_gateway_protocol::frame::{Frame, FrameDecoder, FrameError, RelayEnd};

use super::{header_map, kind_from_wire};
use crate::{ErrorKind, Failure, LimitKind, Phase, ResponseBody, TransmissionFact};

/// The frames of one relay response, decoded as they arrive.
pub(super) struct RelayFrames {
    hop: Option<ResponseBody>,
    decoder: FrameDecoder,
    /// The hop drops before the next body frame.
    cut: bool,
}

impl RelayFrames {
    pub(super) const fn new(hop: ResponseBody, decoder: FrameDecoder) -> Self {
        Self {
            hop: Some(hop),
            decoder,
            cut: false,
        }
    }

    /// Drops the hop before the next body frame, as a lost connection does.
    pub(super) const fn cut(&mut self) {
        self.cut = true;
    }

    /// The first frame: the upstream head, or an `End` before it. Every break
    /// before the head is a lost response after possible transmission.
    pub(super) async fn first(&mut self) -> Result<Frame, Failure> {
        let lost = |kind| {
            Failure::new(
                Phase::ResponseHead,
                TransmissionFact::MayHaveTransmitted,
                kind,
            )
        };
        loop {
            match self.decoder.next_frame() {
                Ok(Some(frame)) => return Ok(frame),
                Ok(None) => {}
                Err(error) => return Err(lost(frame_error_kind(error))),
            }
            match self.pull().await {
                Ok(()) => {}
                Err(kind) => return Err(lost(kind)),
            }
        }
    }

    /// The next upstream body frame; `None` after `End{complete}` and the
    /// relay body's own clean end. Cancel-safe: a frame is consumed only when
    /// it is returned.
    async fn next(&mut self) -> Option<Result<http_body::Frame<Bytes>, ErrorKind>> {
        self.hop.as_ref()?;
        if self.cut {
            self.hop = None;
            return Some(Err(ErrorKind::Io));
        }
        loop {
            match self.decoder.next_frame() {
                Ok(Some(Frame::Data(data))) => {
                    return Some(Ok(http_body::Frame::data(Bytes::from(data.bytes))));
                }
                Ok(Some(Frame::Trailers(trailers))) => {
                    return Some(
                        header_map(&trailers.headers)
                            .map(http_body::Frame::trailers)
                            .ok_or(ErrorKind::InvalidResponse),
                    );
                }
                Ok(Some(Frame::End(RelayEnd::Complete {}))) => return self.finish().await,
                Ok(Some(Frame::End(RelayEnd::UpstreamFailed { kind, timeout, .. }))) => {
                    self.hop = None;
                    return Some(Err(if timeout {
                        ErrorKind::DeadlineExceeded
                    } else {
                        kind_from_wire(kind)
                    }));
                }
                Ok(Some(
                    Frame::End(RelayEnd::Cancelled {} | RelayEnd::GatewayFailed {})
                    | Frame::Head(_),
                )) => {
                    self.hop = None;
                    return Some(Err(ErrorKind::Protocol));
                }
                Ok(None) => {}
                Err(error) => {
                    self.hop = None;
                    return Some(Err(frame_error_kind(error)));
                }
            }
            if let Err(kind) = self.pull().await {
                self.hop = None;
                return Some(Err(kind));
            }
        }
    }

    /// Reads relay bytes into the decoder.
    async fn pull(&mut self) -> Result<(), ErrorKind> {
        let hop = self.hop.as_mut().ok_or(ErrorKind::Protocol)?;
        match Box::pin(hop.next_chunk()).await {
            Some(Ok(bytes)) => self.decoder.push(&bytes).map_err(frame_error_kind),
            Some(Err(failure)) => Err(failure.kind()),
            None => Err(ErrorKind::Protocol),
        }
    }

    /// After `End{complete}`: the relay body must end with nothing after it,
    /// which also lets its connection carry the next relay request.
    async fn finish(&mut self) -> Option<Result<http_body::Frame<Bytes>, ErrorKind>> {
        let mut hop = self.hop.take()?;
        loop {
            match Box::pin(hop.next_chunk()).await {
                None => {
                    return self
                        .decoder
                        .finish()
                        .err()
                        .map(|error| Err(frame_error_kind(error)));
                }
                Some(Ok(bytes)) => {
                    if bytes.is_empty() {
                        continue;
                    }
                    return Some(Err(ErrorKind::Protocol));
                }
                Some(Err(failure)) => return Some(Err(failure.kind())),
            }
        }
    }
}

const fn frame_error_kind(error: FrameError) -> ErrorKind {
    match error {
        FrameError::ResponseTooLarge => ErrorKind::LimitExceeded(LimitKind::ResponseBodyBytes),
        _ => ErrorKind::Protocol,
    }
}

impl crate::transport::hooks::FrameSource for RelayFrames {
    fn frame(
        &mut self,
    ) -> crate::transport::hooks::HookFuture<'_, Option<Result<http_body::Frame<Bytes>, ErrorKind>>>
    {
        Box::pin(self.next())
    }
}
