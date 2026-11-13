use super::{io, receive, send, wire};
use tokio::net::UnixStream;

#[derive(Clone, Copy)]
pub(super) enum Codec {
    V1,
}

impl Codec {
    pub(super) fn select(version: u32, current: u32) -> io::Result<Self> {
        if version != current && !(current > 1 && version == current - 1) {
            return Err(super::invalid(
                "finish/consume/disown jobs through a retained compatible build, then retry",
            ));
        }
        match version {
            1 => Ok(Self::V1),
            _ => Err(super::invalid(
                "finish/consume/disown jobs through a retained compatible build, then retry",
            )),
        }
    }

    pub(super) async fn read_call(self, stream: &mut UnixStream) -> io::Result<wire::Call> {
        match self {
            Self::V1 => receive(stream).await,
        }
    }
    pub(super) async fn write_call(
        self,
        stream: &mut UnixStream,
        call: &wire::Call,
    ) -> io::Result<()> {
        match self {
            Self::V1 => send(stream, call).await,
        }
    }
    pub(super) async fn read_reply(self, stream: &mut UnixStream) -> io::Result<wire::Reply> {
        match self {
            Self::V1 => receive(stream).await,
        }
    }
    pub(super) async fn write_reply(
        self,
        stream: &mut UnixStream,
        reply: &wire::Reply,
    ) -> io::Result<()> {
        match self {
            Self::V1 => send(stream, reply).await,
        }
    }
}
