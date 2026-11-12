//! Authenticated host-internal terminal actions; ambiguous keyboard bytes never replay.

use super::{
    Arc, ControllerOwner, HostRequestContext, OperatorAnswer, OperatorOperation, OperatorRefusal,
    PathBuf, ResidencyHold, TerminalError, TerminalHandle, TerminalKey, TerminalPin,
    TerminalRegistry, TerminalView, id, pins,
};
use plakous::runner::protocol::operator::{TerminalCursor, TerminalDimensions, TerminalEvent};

pub(crate) struct TerminalController {
    registry: std::sync::Weak<TerminalRegistry>,
    identity: String,
    token: String,
    stream: Arc<tokio::sync::Mutex<Option<plakous::runner::OperatorTerminalStream>>>,
    cancelled: tokio::sync::watch::Receiver<bool>,
}

pub(super) struct Placement {
    pub(super) runner: String,
    pub(super) workspace: PathBuf,
}

impl TerminalRegistry {
    pub(crate) fn revoke_controller(
        &self,
        context: &HostRequestContext,
        handle: &TerminalHandle,
    ) -> Result<(), TerminalError> {
        self.owned(context, handle)?;
        if let Some(owner) = self.lock().controllers.remove(&handle.identity) {
            let _ = owner.cancel.send(true);
        }
        Ok(())
    }
    fn placement(&self, key: &TerminalKey) -> Result<Placement, TerminalError> {
        use plakous_recursive_subagents::tree::TreeStore as _;
        let mut store = self.store()?;
        let record = store
            .load(key.session.as_str())
            .map_err(|_| TerminalError::Lost)?;
        if store
            .lifecycle_metadata(key.session.as_str())
            .map_err(|_| TerminalError::Storage)?
            .1
        {
            return Err(TerminalError::Lost);
        }
        let persisted = crate::store::tree::open_tree_store(&self.paths)
            .map_err(|_| TerminalError::Storage)?
            .load(&record.node.tree_id)
            .map_err(|_| TerminalError::Storage)?
            .ok_or(TerminalError::Lost)?;
        let tree = plakous_recursive_subagents::tree::ProductAgentTree::reopen(persisted);
        let node = tree
            .node(key.node.as_str())
            .map_err(|_| TerminalError::Lost)?;
        let workspace = node
            .placement
            .workspace
            .as_ref()
            .map_or(&node.path, |w| &w.cwd);
        let runner = node.placement.runner_id.as_deref().unwrap_or("local");
        Ok(Placement {
            runner: runner.into(),
            workspace: pins::physical(runner, workspace)?,
        })
    }

    pub(crate) async fn open(
        &self,
        context: &HostRequestContext,
        key: TerminalKey,
        dimensions: TerminalDimensions,
    ) -> Result<TerminalView, TerminalError> {
        self.authenticate(context)?;
        let hold = self.hold.get().ok_or(TerminalError::Unavailable)?(key.session.as_str())
            .map_err(|_| TerminalError::Unavailable)?;
        let placement = self.placement(&key)?;
        self.open_placement(context, key, dimensions, placement, hold)
            .await
    }

    pub(crate) async fn recover_open(
        &self,
        context: &HostRequestContext,
        key: &TerminalKey,
        dimensions: TerminalDimensions,
    ) -> Result<Option<TerminalView>, TerminalError> {
        let _operation = self.operations.lock().await;
        self.authenticate(context)?;
        let namespace = self.namespace(&context.principal)?;
        let prior = self
            .lock()
            .entries
            .iter()
            .find(|(_, entry)| entry.pin.namespace == namespace && entry.pin.key == *key)
            .map(|(identity, entry)| (identity.clone(), entry.pin.clone(), entry.lost));
        let Some((identity, pin, lost)) = prior else {
            return Ok(None);
        };
        let binding = self.binding(&pin.runner)?;
        if lost
            || pin.generation != self.runner_generation
            || pin.instance != binding.client.instance()
        {
            return Err(TerminalError::Lost);
        }
        let operation = OperatorOperation::Open {
            key: key.clone(),
            dimensions,
            workspace: pin
                .workspace
                .to_str()
                .ok_or(TerminalError::Conflict)?
                .into(),
        };
        let digest = plakous_hosted::workspace_maintenance::digest_text(
            &serde_json::to_string(&operation).map_err(|_| TerminalError::Conflict)?,
        );
        if digest != pin.digest {
            return Err(TerminalError::Conflict);
        }
        let answer = binding
            .client
            .operator_recover_open(namespace, operation)
            .await
            .map_err(|_| TerminalError::Unavailable)?;
        if !binding.revision.is_current() {
            return Err(TerminalError::Unavailable);
        }
        match answer {
            OperatorAnswer::Terminal { terminal, .. } => self.observe(&identity, terminal)?,
            OperatorAnswer::Refused { refusal } => return Err(TerminalError::Refused(refusal)),
            _ => return Err(TerminalError::Lost),
        }
        self.view(&identity).map(Some)
    }

    pub(super) async fn open_placement(
        &self,
        context: &HostRequestContext,
        key: TerminalKey,
        dimensions: TerminalDimensions,
        placement: Placement,
        hold: ResidencyHold,
    ) -> Result<TerminalView, TerminalError> {
        let _operation = self.operations.lock().await;
        self.authenticate(context)?;
        let binding = self.binding(&placement.runner)?;
        let namespace = self.namespace(&context.principal)?;
        let operation = OperatorOperation::Open {
            key: key.clone(),
            workspace: placement
                .workspace
                .to_str()
                .ok_or(TerminalError::Conflict)?
                .to_owned(),
            dimensions,
        };
        let digest = plakous_hosted::workspace_maintenance::digest_text(
            &serde_json::to_string(&operation).map_err(|_| TerminalError::Conflict)?,
        );
        let prior = self
            .lock()
            .entries
            .iter()
            .find(|(_, e)| {
                e.pin.namespace == namespace && e.pin.runner == placement.runner && e.pin.key == key
            })
            .map(|(identity, e)| (identity.clone(), e.pin.clone(), e.lost));
        if let Some((identity, pin, lost)) = prior {
            if lost
                || pin.generation != self.runner_generation
                || pin.instance != binding.client.instance()
            {
                return Err(TerminalError::Lost);
            }
            if pin.digest != digest {
                return Err(TerminalError::Conflict);
            }
            let answer = binding
                .client
                .operator_recover_open(namespace, operation)
                .await
                .map_err(|_| TerminalError::Unavailable)?;
            if let OperatorAnswer::Terminal { terminal, .. } = answer {
                self.observe(&identity, terminal)?;
            } else {
                return Err(TerminalError::Lost);
            }
            return self.view(&identity);
        }
        Self::request(&binding, namespace.clone(), OperatorOperation::Renew).await?;
        let pin = TerminalPin {
            host_generation: self.generation,
            host_id: self.host_id.clone(),
            generation: self.runner_generation.clone(),
            namespace: namespace.clone(),
            runner: placement.runner,
            instance: binding.client.instance(),
            key,
            workspace: placement.workspace,
            digest,
        };
        let identity = self.reserve(pin, hold)?;
        match Self::request(&binding, namespace, operation).await {
            Ok(OperatorAnswer::Terminal { terminal, .. }) => self.observe(&identity, terminal)?,
            Err(TerminalError::Refused(refusal)) => {
                self.release_proven(&identity)?;
                return Err(TerminalError::Refused(refusal));
            }
            Err(TerminalError::OutcomeUnknown) => return Err(TerminalError::OutcomeUnknown),
            _ => return Err(TerminalError::Unavailable),
        }
        self.view(&identity)
    }

    fn view(&self, identity: &str) -> Result<TerminalView, TerminalError> {
        let state = self.lock();
        let entry = state.entries.get(identity).ok_or(TerminalError::Lost)?;
        Ok(TerminalView {
            handle: TerminalHandle {
                identity: identity.into(),
                host_generation: entry.pin.host_generation,
                instance: entry.pin.instance.clone(),
            },
            key: entry.pin.key.clone(),
            row: entry.row.clone(),
            lost: entry.lost,
        })
    }

    pub(crate) fn list(
        &self,
        context: &HostRequestContext,
    ) -> Result<Vec<TerminalView>, TerminalError> {
        self.authenticate(context)?;
        let namespace = self.namespace(&context.principal)?;
        let identities = {
            let state = self.lock();
            let mut live = Vec::new();
            let mut completed = Vec::new();
            for (id, entry) in state
                .entries
                .iter()
                .filter(|(_, e)| e.pin.namespace == namespace)
            {
                if entry.pinned {
                    live.push(id.clone());
                } else {
                    completed.push(id.clone());
                }
            }
            // Keep replay fences even when an old receipt leaves the visible table.
            live.extend(completed.into_iter().rev().take(8));
            live
        };
        identities
            .iter()
            .map(|identity| self.view(identity))
            .collect()
    }

    pub(crate) async fn close(
        &self,
        context: &HostRequestContext,
        handle: &TerminalHandle,
        acknowledge_loss: bool,
    ) -> Result<TerminalView, TerminalError> {
        let _operation = self.operations.lock().await;
        let pin = self.owned(context, handle)?;
        let binding = self.lock().bindings.get(&pin.runner).cloned();
        let Some(binding) = binding.filter(|binding| {
            binding.client.instance() == pin.instance && binding.client.is_connected()
        }) else {
            if !acknowledge_loss {
                return Err(TerminalError::Unavailable);
            }
            self.release_proven(&handle.identity)?;
            self.mark_lost(&handle.identity);
            return self.view(&handle.identity);
        };
        let _ = Self::request(&binding, pin.namespace.clone(), OperatorOperation::List).await;
        let answer = Self::request(
            &binding,
            pin.namespace.clone(),
            OperatorOperation::Close {
                key: pin.key.clone(),
                generation: Some(id(pin.generation.clone())?),
                acknowledge_loss,
            },
        )
        .await;
        match answer {
            Ok(OperatorAnswer::Terminal { terminal, .. }) => {
                self.observe(&handle.identity, terminal)?;
            }
            Err(TerminalError::Refused(OperatorRefusal::TerminalLost)) if acknowledge_loss => {
                self.release_proven(&handle.identity)?;
                self.mark_lost(&handle.identity);
            }
            Err(error) => return Err(error),
            _ => return Err(TerminalError::Unavailable),
        }
        self.view(&handle.identity)
    }

    pub(crate) async fn attach(
        self: &Arc<Self>,
        context: &HostRequestContext,
        handle: &TerminalHandle,
        cursor: TerminalCursor,
        takeover: bool,
    ) -> Result<TerminalController, TerminalError> {
        let _operation = self.operations.lock().await;
        let pin = self.owned(context, handle)?;
        if handle.host_generation != self.generation
            || pin.generation != self.runner_generation
            || handle.instance != cursor.instance.as_str()
        {
            return Err(TerminalError::Lost);
        }
        if self
            .lock()
            .entries
            .get(&handle.identity)
            .is_none_or(|entry| entry.lost)
        {
            return Err(TerminalError::Lost);
        }
        if self.lock().controllers.contains_key(&handle.identity) {
            return Err(TerminalError::Busy);
        }
        let binding = self.binding(&pin.runner)?;
        Self::request(&binding, pin.namespace.clone(), OperatorOperation::List).await?;
        let answer = Self::request(
            &binding,
            pin.namespace.clone(),
            OperatorOperation::Attach {
                takeover,
                key: pin.key.clone(),
                cursor,
            },
        )
        .await?;
        let OperatorAnswer::Terminal {
            terminal,
            stream: Some(stream),
        } = answer
        else {
            return Err(TerminalError::Unavailable);
        };
        self.observe(&handle.identity, terminal)?;
        let stream = Arc::new(tokio::sync::Mutex::new(Some(
            binding
                .client
                .operator_stream(stream)
                .map_err(|_| TerminalError::OutcomeUnknown)?,
        )));
        let (cancel, cancelled) = tokio::sync::watch::channel(false);
        let token = uuid::Uuid::new_v4().to_string();
        self.lock().controllers.insert(
            handle.identity.clone(),
            ControllerOwner {
                principal: context.principal.clone(),
                connection: context.connection_id,
                token: token.clone(),
                cancel,
            },
        );
        let mut cancellation = cancelled.clone();
        let detached = stream.clone();
        tokio::spawn(async move {
            let _ = cancellation.changed().await;
            drop(detached.lock().await.take());
        });
        Ok(TerminalController {
            registry: Arc::downgrade(self),
            identity: handle.identity.clone(),
            token,
            stream,
            cancelled,
        })
    }
}

impl TerminalController {
    fn authorized(&self, context: &HostRequestContext) -> Result<(), TerminalError> {
        let registry = self.registry.upgrade().ok_or(TerminalError::Lost)?;
        registry.authenticate(context)?;
        let state = registry.lock();
        let owner = state
            .controllers
            .get(&self.identity)
            .ok_or(TerminalError::Lost)?;
        let entry = state
            .entries
            .get(&self.identity)
            .ok_or(TerminalError::Lost)?;
        if owner.principal != context.principal
            || owner.connection != context.connection_id
            || owner.token != self.token
            || entry.lost
        {
            return Err(TerminalError::Lost);
        }
        let runner = entry.pin.runner.clone();
        drop(state);
        registry.binding(&runner)?;
        Ok(())
    }
    pub(crate) async fn input(
        &self,
        context: &HostRequestContext,
        offset: u64,
        bytes: &[u8],
    ) -> Result<(), TerminalError> {
        let mut stream = self.stream.lock().await;
        self.authorized(context)?;
        stream
            .as_mut()
            .ok_or(TerminalError::Lost)?
            .input(offset, bytes)
            .map_err(|_| TerminalError::Unavailable)
    }
    pub(crate) async fn credit(
        &self,
        context: &HostRequestContext,
        bytes: u64,
    ) -> Result<(), TerminalError> {
        let mut stream = self.stream.lock().await;
        self.authorized(context)?;
        stream
            .as_mut()
            .ok_or(TerminalError::Lost)?
            .credit(bytes)
            .map_err(|_| TerminalError::Unavailable)
    }
    pub(crate) async fn next(
        &mut self,
        context: &HostRequestContext,
    ) -> Result<TerminalEvent, TerminalError> {
        self.authorized(context)?;
        let mut stream = self.stream.lock().await;
        let current = stream.as_mut().ok_or(TerminalError::Lost)?;
        tokio::select! {
            _ = self.cancelled.changed() => { stream.take(); Err(TerminalError::Lost) },
            event = current.next() => event.map_err(|_| TerminalError::Unavailable),
        }
    }
    pub(crate) async fn resize(
        &self,
        context: &HostRequestContext,
        dimensions: TerminalDimensions,
    ) -> Result<(), TerminalError> {
        self.authorized(context)?;
        let registry = self.registry.upgrade().ok_or(TerminalError::Lost)?;
        let pin = registry
            .lock()
            .entries
            .get(&self.identity)
            .ok_or(TerminalError::Lost)?
            .pin
            .clone();
        let binding = registry.binding(&pin.runner)?;
        TerminalRegistry::request(
            &binding,
            pin.namespace,
            OperatorOperation::Resize {
                key: pin.key,
                dimensions,
            },
        )
        .await?;
        Ok(())
    }
}

impl Drop for TerminalController {
    fn drop(&mut self) {
        if let Some(registry) = self.registry.upgrade() {
            let mut state = registry.lock();
            if state
                .controllers
                .get(&self.identity)
                .is_some_and(|owner| owner.token == self.token)
                && let Some(owner) = state.controllers.remove(&self.identity)
            {
                let _ = owner.cancel.send(true);
            }
        }
    }
}
