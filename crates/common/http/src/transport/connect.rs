//! Staggered TCP connection racing within one connect-phase deadline.

use std::future::Future;
use std::net::SocketAddr;
use std::time::Duration;

use futures_util::stream::{FuturesUnordered, StreamExt as _};
use tokio::net::TcpStream;

use crate::{ErrorKind, Failure, Phase, TransmissionFact};

const ATTEMPT_DELAY: Duration = Duration::from_millis(250);

pub(super) fn interleave(addresses: Vec<SocketAddr>) -> Vec<SocketAddr> {
    let ipv6_first = addresses.first().is_some_and(SocketAddr::is_ipv6);
    let (first, second): (Vec<_>, Vec<_>) = addresses
        .into_iter()
        .partition(|address| address.is_ipv6() == ipv6_first);
    let mut first = first.into_iter();
    let mut second = second.into_iter();
    let mut result = Vec::new();
    loop {
        match (first.next(), second.next()) {
            (None, None) => return result,
            (a, b) => result.extend(a.into_iter().chain(b)),
        }
    }
}

pub(super) async fn connect(addresses: Vec<SocketAddr>) -> Result<TcpStream, Failure> {
    race(addresses, |address| async move {
        let stream = TcpStream::connect(address).await?;
        // HEADERS and DATA must not wait for the peer's delayed ACK.
        stream.set_nodelay(true)?;
        Ok(stream)
    })
    .await
}

async fn race<T, F, Fut>(addresses: Vec<SocketAddr>, mut connector: F) -> Result<T, Failure>
where
    F: FnMut(SocketAddr) -> Fut,
    Fut: Future<Output = Result<T, std::io::Error>>,
{
    let mut addresses = addresses.into_iter();
    let mut pending = FuturesUnordered::new();
    let mut next = tokio::time::Instant::now();
    let mut failed = Failure::new(Phase::Connect, TransmissionFact::NotStarted, ErrorKind::Io);
    let mut attempts = crate::error::ConnectAttempts::default();
    loop {
        if pending.is_empty() || tokio::time::Instant::now() >= next {
            if let Some(address) = addresses.next() {
                attempts.record(address);
                pending.push(connector(address));
                next = tokio::time::Instant::now() + ATTEMPT_DELAY;
            } else if pending.is_empty() {
                return Err(failed.with_connect_attempts(attempts));
            }
        }
        tokio::select! {
            biased;
            Some(result) = pending.next() => match result {
                // Dropping the other futures cancels their sockets; no tasks detach.
                Ok(stream) => return Ok(stream),
                Err(error) => failed = failed.with_io_cause(&error),
            },
            () = tokio::time::sleep_until(next), if addresses.len() > 0 => {},
        }
    }
}

#[cfg(test)]
mod tests;
