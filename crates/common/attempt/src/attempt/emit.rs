//! Shared panic-contained sink delivery race for Responses-family emit paths.

use std::future::{Future, poll_fn};
use std::pin::Pin;
use std::task::Poll;

use super::delivery::{ContainedSinkDelivery, PanicAsError};

/// Event-sink errors that can report cancellation as well as panic.
pub trait SinkDeliveryError: PanicAsError {
    /// Cancellation won the event-delivery race.
    fn cancelled() -> Self;
}

/// Races a sink delivery future against cooperative cancellation.
///
/// Drop panic is mapped to [`PanicAsError::panicked`]. Cancel observed on the
/// `cancelled` future is mapped to [`SinkDeliveryError::cancelled`].
pub async fn race_sink_delivery<E, C>(
    delivery: Pin<Box<dyn Future<Output = Result<(), E>> + Send + '_>>,
    mut cancelled: C,
) -> Result<(), E>
where
    E: SinkDeliveryError,
    C: Future<Output = ()> + Unpin,
{
    let mut delivery = ContainedSinkDelivery::new(delivery);
    let delivered = poll_fn(|context| {
        if Pin::new(&mut cancelled).poll(context).is_ready() {
            return Poll::Ready(Err(E::cancelled()));
        }
        Pin::new(&mut delivery).poll(context)
    })
    .await;
    if delivery.dispose() {
        Err(E::panicked())
    } else {
        delivered
    }
}
