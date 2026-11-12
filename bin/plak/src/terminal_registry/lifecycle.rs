//! First-bind barriers, awake namespace renewal and joined host shutdown.

use super::{
    Arc, Binding, OperatorAnswer, OperatorOperation, OperatorRefusal, RunnerClient, TerminalError,
    TerminalRegistry, TerminalRow, TerminalStatus, id,
};

impl TerminalRegistry {
    pub(crate) fn compose(
        services: &crate::shell::services::Services,
        generation: u64,
    ) -> plakous_hosted::CliResult<(crate::shell::services::Services, Arc<Self>)> {
        let registry = Self::new(services.paths()?.clone(), generation)
            .map_err(plakous_hosted::CliFailure::new)?;
        Ok((services.with_terminal_registry(registry.clone()), registry))
    }

    pub(crate) fn serve(
        self: &Arc<Self>,
        host: &Arc<
            plakous_hosted::host_service::wire::SessionHost<crate::surface_protocol::PlakExtension>,
        >,
        runtime: &tokio::runtime::Handle,
    ) -> plakous_hosted::CliResult<()> {
        let residency = Arc::downgrade(host);
        self.bind_residency(Arc::new(move |session| {
            residency
                .upgrade()
                .ok_or_else(|| "terminal host ended".to_owned())?
                .slot(session)
                .hold_resident(session)
                .ok_or_else(|| "terminal target must already be resident".to_owned())
        }))
        .map_err(plakous_hosted::CliFailure::new)?;
        self.start(runtime);
        Ok(())
    }

    pub(crate) async fn stop(&self) {
        let unresolved = self.shutdown(std::time::Duration::from_secs(4)).await;
        if unresolved != 0 {
            eprintln!(
                "plak host: operator terminal namespace ended with {unresolved} unresolved pins"
            );
        }
    }
    pub(crate) async fn reconcile(
        &self,
        runner: &str,
        client: &RunnerClient,
    ) -> Result<(), TerminalError> {
        let _operation = self.operations.lock().await;
        let binding = Binding {
            client: client.clone(),
            revision: Arc::new(client.connection_revision()),
            reconciled: false,
        };
        {
            let mut state = self.lock();
            if state.closing {
                return Err(TerminalError::Lost);
            }
            if state.bindings.get(runner).is_some_and(|prior| {
                !prior.revision.is_current() || prior.client.instance() != client.instance()
            }) {
                let detached: Vec<_> = state
                    .entries
                    .iter()
                    .filter(|(_, entry)| entry.pin.runner == runner)
                    .map(|(identity, _)| identity.clone())
                    .collect();
                for identity in detached {
                    if let Some(owner) = state.controllers.remove(&identity) {
                        let _ = owner.cancel.send(true);
                    }
                }
            }
            state.bindings.insert(runner.to_owned(), binding.clone());
        }
        let namespaces = self.synchronize_inventory(runner, &binding).await?;
        Self::namespace_table(
            &binding,
            super::OperatorNamespace {
                state_key: self.state_key.clone(),
                principal: id("host-inventory".into())?,
            },
        )
        .await?;
        for namespace in namespaces {
            if let Some(rows) = Self::namespace_table(&binding, namespace.clone()).await? {
                self.observe_table(runner, &namespace, rows)?;
            }
        }
        self.clean_discovered(runner, &binding).await?;
        self.synchronize_inventory(runner, &binding).await?;
        self.refresh_completed_rows(runner, &binding).await;
        // Retained lost pins fence their own workspaces, not unrelated work.
        self.lock().bindings.insert(
            runner.to_owned(),
            Binding {
                reconciled: true,
                ..binding
            },
        );
        Ok(())
    }

    async fn namespace_table(
        binding: &Binding,
        namespace: super::OperatorNamespace,
    ) -> Result<Option<Vec<TerminalRow>>, TerminalError> {
        let table = match Self::request(binding, namespace.clone(), OperatorOperation::Renew).await
        {
            Ok(OperatorAnswer::Table { terminals }) => Some(terminals),
            Err(TerminalError::Refused(OperatorRefusal::TerminalBusy)) => None,
            Err(TerminalError::Refused(OperatorRefusal::NamespaceExpired)) => {
                match Self::request(binding, namespace, OperatorOperation::List).await? {
                    OperatorAnswer::Table { terminals } => Some(terminals),
                    _ => return Err(TerminalError::Unavailable),
                }
            }
            Err(error) => return Err(error),
            _ => return Err(TerminalError::Unavailable),
        };
        Ok(table)
    }

    pub(super) fn mark_lost(&self, identity: &str) {
        let mut state = self.lock();
        if let Some(entry) = state.entries.get_mut(identity) {
            entry.lost = true;
        }
        if let Some(owner) = state.controllers.remove(identity) {
            let _ = owner.cancel.send(true);
        }
    }

    pub(crate) fn start(self: &Arc<Self>, runtime: &tokio::runtime::Handle) {
        let weak = Arc::downgrade(self);
        runtime.spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                let Some(registry) = weak.upgrade() else {
                    break;
                };
                if registry.lock().closing {
                    break;
                }
                let bindings: Vec<_> = registry
                    .lock()
                    .bindings
                    .iter()
                    .map(|(runner, binding)| (runner.clone(), binding.client.clone()))
                    .collect();
                for (runner, client) in bindings {
                    let _ = registry.reconcile(&runner, &client).await;
                }
            }
        });
    }

    pub(crate) async fn shutdown(&self, deadline: std::time::Duration) -> usize {
        {
            let mut state = self.lock();
            state.closing = true;
            for (_, owner) in std::mem::take(&mut state.controllers) {
                let _ = owner.cancel.send(true);
            }
        }
        let until = tokio::time::Instant::now() + deadline;
        let namespaces = {
            let state = self.lock();
            let mut namespaces: Vec<(String, Binding, super::OperatorNamespace)> = Vec::new();
            for entry in state.entries.values() {
                if namespaces.iter().any(|(runner, _, ns)| {
                    runner == &entry.pin.runner && ns == &entry.pin.namespace
                }) {
                    continue;
                }
                if let Some(binding) = state.bindings.get(&entry.pin.runner) {
                    namespaces.push((
                        entry.pin.runner.clone(),
                        binding.clone(),
                        entry.pin.namespace.clone(),
                    ));
                }
            }
            namespaces
        };
        let closing = async {
            let _operation = self.operations.lock().await;
            loop {
                let pins: Vec<_> = self
                    .lock()
                    .entries
                    .iter()
                    .filter(|(_, e)| e.pinned)
                    .map(|(id, e)| (id.clone(), e.pin.clone()))
                    .collect();
                if pins.is_empty() {
                    for (_, binding, namespace) in &namespaces {
                        let _ = Self::request(
                            binding,
                            namespace.clone(),
                            OperatorOperation::NamespaceClose,
                        )
                        .await;
                    }
                    return;
                }
                for (identity, pin) in pins {
                    let binding = self.lock().bindings.get(&pin.runner).cloned();
                    let Some(binding) =
                        binding.filter(|binding| binding.client.instance() == pin.instance)
                    else {
                        continue;
                    };
                    let _ = Self::request(&binding, pin.namespace.clone(), OperatorOperation::List)
                        .await;
                    let Ok(generation) = id(pin.generation.clone()) else {
                        continue;
                    };
                    if let Ok(OperatorAnswer::Terminal { terminal, .. }) = Self::request(
                        &binding,
                        pin.namespace.clone(),
                        OperatorOperation::Close {
                            key: pin.key.clone(),
                            generation: Some(generation),
                            acknowledge_loss: false,
                        },
                    )
                    .await
                    {
                        if terminal.status == TerminalStatus::TerminationUnknown {
                            let _ = Self::request(
                                &binding,
                                pin.namespace.clone(),
                                OperatorOperation::NamespaceClose,
                            )
                            .await;
                        }
                        let _ = self.observe(&identity, terminal);
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        };
        let _ = tokio::time::timeout_at(until, closing).await;
        let mut state = self.lock();
        for entry in state.entries.values_mut().filter(|entry| entry.pinned) {
            entry.lost = true;
        }
        state.entries.values().filter(|entry| entry.pinned).count()
    }
}
