//! Test-owned provider rendezvous, independent of elapsed host time.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex, Weak};

use plakous::model::{CancellationToken, ModelError, ModelErrorKind};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

static GATES: LazyLock<Mutex<HashMap<String, Weak<State>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

struct State {
    entered: watch::Sender<bool>,
    released: watch::Sender<bool>,
}

/// A scripted wait for an in-process registry entry or a cross-process file.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ScriptGate {
    /// A name issued by [`GateHandle`]. Unknown names fail rather than hang.
    Registry(String),
    /// Release when this sentinel exists. Other filesystem errors fail.
    File(PathBuf),
}

/// Owns a unique registry gate; dropping it releases any pending stream.
pub struct GateHandle {
    name: String,
    state: Arc<State>,
}

impl Default for GateHandle {
    fn default() -> Self {
        Self::new()
    }
}

impl GateHandle {
    /// Registers an unreleased gate with a fresh process-local identity.
    #[must_use]
    pub fn new() -> Self {
        let name = uuid::Uuid::new_v4().to_string();
        let state = Arc::new(State {
            entered: watch::channel(false).0,
            released: watch::channel(false).0,
        });
        registry().insert(name.clone(), Arc::downgrade(&state));
        Self { name, state }
    }

    /// The script value that waits on this handle.
    #[must_use]
    pub fn script(&self) -> ScriptGate {
        ScriptGate::Registry(self.name.clone())
    }

    /// Waits for the model to reach the gate, including an earlier arrival.
    pub async fn entered(&self) {
        wait_true(self.state.entered.subscribe()).await;
    }

    /// Releases this gate permanently, including later arrivals.
    pub fn release(&self) {
        self.state.released.send_replace(true);
    }
}

impl Drop for GateHandle {
    fn drop(&mut self) {
        self.release();
        registry().remove(&self.name);
    }
}

fn registry() -> std::sync::MutexGuard<'static, HashMap<String, Weak<State>>> {
    GATES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

async fn wait_true(mut receiver: watch::Receiver<bool>) {
    while !*receiver.borrow_and_update() {
        if receiver.changed().await.is_err() {
            break;
        }
    }
}

pub(crate) async fn wait_gate(
    gate: &ScriptGate,
    cancellation: &CancellationToken,
    ignores_cancellation: bool,
) -> Result<(), ModelError> {
    let released = async {
        match gate {
            ScriptGate::Registry(name) => {
                let state = registry()
                    .get(name)
                    .and_then(Weak::upgrade)
                    .ok_or_else(|| {
                        ModelError::new(ModelErrorKind::Protocol, "unregistered scripted gate")
                    })?;
                state.entered.send_replace(true);
                wait_true(state.released.subscribe()).await;
            }
            ScriptGate::File(path) => loop {
                match std::fs::metadata(path) {
                    Ok(_) => break,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                    Err(_) => {
                        return Err(ModelError::new(
                            ModelErrorKind::Protocol,
                            "could not inspect scripted gate sentinel",
                        ));
                    }
                }
            },
        }
        Ok(())
    };
    if ignores_cancellation {
        return released.await;
    }
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(ModelError::new(
            ModelErrorKind::Cancelled, "the scripted gate was cancelled",
        )),
        result = released => result,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test-owned rendezvous")]
mod tests {
    use super::*;

    #[tokio::test]
    async fn registry_gate_waits_for_release_and_remembers_early_release() {
        let gate = GateHandle::new();
        let script = gate.script();
        let cancellation = CancellationToken::new();
        let pending = tokio::spawn(async move { wait_gate(&script, &cancellation, false).await });
        gate.entered().await;
        assert!(!pending.is_finished());
        gate.release();
        pending.await.unwrap().unwrap();
        wait_gate(&gate.script(), &CancellationToken::new(), false)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn registry_gate_cancellation_is_event_driven_and_drop_releases() {
        let gate = GateHandle::new();
        let script = gate.script();
        let cancellation = CancellationToken::new();
        let signal = cancellation.clone();
        let pending = tokio::spawn(async move { wait_gate(&script, &cancellation, false).await });
        gate.entered().await;
        signal.cancel();
        assert_eq!(
            pending.await.unwrap().unwrap_err().kind(),
            ModelErrorKind::Cancelled
        );
        let gate = GateHandle::new();
        let script = gate.script();
        let pending =
            tokio::spawn(async move { wait_gate(&script, &CancellationToken::new(), true).await });
        gate.entered().await;
        drop(gate);
        pending.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn file_gate_waits_for_sentinel_and_unknown_registry_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("release");
        let cancellation = CancellationToken::new();
        let script = ScriptGate::File(path.clone());
        let mut pending = Box::pin(wait_gate(&script, &cancellation, false));
        let waker = futures_util::task::noop_waker();
        let mut context = std::task::Context::from_waker(&waker);
        assert!(pending.as_mut().poll(&mut context).is_pending());
        std::fs::write(path, b"release").unwrap();
        pending.await.unwrap();
        let error = wait_gate(
            &ScriptGate::Registry("missing".into()),
            &cancellation,
            false,
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), ModelErrorKind::Protocol);
    }
}
