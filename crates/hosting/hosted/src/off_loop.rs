//! Blocking product reads the host awaits without blocking its loop.

/// A blocking product read the host runs on its own thread.
pub(crate) type BlockingRead<T> = Box<dyn FnOnce() -> T + Send>;

/// Runs `read` on a thread of its own and awaits its answer, so a slow read
/// (a gateway round trip, a subscription lock) never holds a runtime worker.
/// `failed` answers when the thread cannot start or ends without answering.
pub(crate) async fn await_read<T: Send + 'static>(
    name: &str,
    read: BlockingRead<T>,
    failed: impl FnOnce() -> T,
) -> T {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let spawned = std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            let _ = sender.send(read());
        });
    if spawned.is_err() {
        return failed();
    }
    receiver.await.unwrap_or_else(|_| failed())
}
