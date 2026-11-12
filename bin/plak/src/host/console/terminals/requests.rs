use super::mapping::{dimensions, handle, id, invalid, refusal, view};
use super::{
    Arc, Console, HostRequest, HostRequestContext, HostResponse, Instant, PlakExtension,
    PoisonError, RequestRefusal, RunnerCursor, RunnerKey, RunnerRefusal, TerminalError,
    TerminalRegistry, WINDOW, dto, owner, refused,
};

impl Console {
    pub(in crate::host) fn terminal_request(
        self: &Arc<Self>,
        context: HostRequestContext,
        request: HostRequest,
        registry: Option<Arc<TerminalRegistry>>,
    ) -> plakous_hosted::host_service::wire::HostReadTask<PlakExtension> {
        let this = Arc::clone(self);
        Box::pin(async move {
            this.terminal_capability(&context.principal)?;
            let Some(registry) = registry else {
                return Ok(refused(dto::ReadRefusal::WorkspaceUnavailable {}));
            };
            let Some(runtime) = this.runtime.clone() else {
                return Ok(refused(dto::ReadRefusal::WorkspaceUnavailable {}));
            };
            let audit =
                super::super::audit::Guard::new(Arc::clone(&this), context.clone(), &request);
            let task = runtime
                .spawn(async move { this.terminal_action(context, request, registry).await });
            let _cancel = super::super::Cancel {
                abort: task.abort_handle(),
                cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            };
            // Do not replay timed-out terminal actions; stream and namespace ownership
            // still bound any accepted work even if the response is lost.
            let mut audit = audit;
            let result = task
                .await
                .unwrap_or(Ok(HostResponse::OperationOutcomeUnknown {}));
            audit.finish(&result);
            result
        })
    }

    async fn terminal_action(
        self: &Arc<Self>,
        context: HostRequestContext,
        request: HostRequest,
        registry: Arc<TerminalRegistry>,
    ) -> Result<HostResponse, RequestRefusal> {
        match self
            .terminal_action_inner(&context, request, &registry)
            .await
        {
            Ok(response) => Ok(response),
            Err(reason) => Ok(refused(reason)),
        }
    }

    async fn terminal_action_inner(
        self: &Arc<Self>,
        context: &HostRequestContext,
        request: HostRequest,
        registry: &Arc<TerminalRegistry>,
    ) -> Result<HostResponse, dto::ReadRefusal> {
        match request {
            request @ HostRequest::TerminalOpen { .. } => {
                self.open_terminal(context, registry, request).await
            }
            HostRequest::TerminalList {} => Ok(HostResponse::TerminalList {
                terminals: registry
                    .list(context)
                    .map_err(refusal)?
                    .into_iter()
                    .map(view)
                    .collect(),
            }),
            request @ HostRequest::TerminalAttach { .. } => {
                self.attach_terminal(context, registry, request).await
            }
            HostRequest::TerminalClose {
                terminal,
                acknowledge_loss,
            } => match registry
                .close(context, &handle(&terminal), acknowledge_loss)
                .await
            {
                Ok(terminal) => Ok(HostResponse::Terminal {
                    terminal: view(terminal),
                }),
                Err(TerminalError::OutcomeUnknown) => Ok(HostResponse::OperationOutcomeUnknown {}),
                Err(error) => Err(refusal(error)),
            },
            HostRequest::TerminalReleaseSession {
                session,
                confirmation,
            } => {
                self.consume(
                    context,
                    &dto::OperatorAction::TerminalRelease {
                        session: session.clone(),
                    },
                    &confirmation,
                )?;
                self.release_terminals(context, registry, &session).await?;
                Ok(HostResponse::TerminalAccepted {})
            }
            HostRequest::TerminalDetach { stream } => {
                let mut state = self
                    .terminals
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                if state
                    .streams
                    .get(&stream)
                    .is_none_or(|s| s.owner != owner(context))
                {
                    return Err(dto::ReadRefusal::TerminalLost {});
                }
                state.streams.remove(&stream);
                Ok(HostResponse::TerminalAccepted {})
            }
            HostRequest::TerminalInput {
                stream,
                offset,
                bytes_b64,
            } => {
                self.terminal_input(context, stream, offset, &bytes_b64)
                    .await
            }
            HostRequest::TerminalCreditBatch { grants } => self.terminal_credit(context, grants),
            HostRequest::TerminalResize {
                stream,
                dimensions: size,
            } => {
                let size = dimensions(size)?;
                let state = self
                    .terminals
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                let stream = state
                    .streams
                    .get(&stream)
                    .filter(|s| s.owner == owner(context) && !s.ended)
                    .ok_or(dto::ReadRefusal::TerminalLost {})?;
                stream.resize.send_replace(Some(size));
                Ok(HostResponse::TerminalAccepted {})
            }
            _ => Err(invalid()),
        }
    }

    async fn open_terminal(
        self: &Arc<Self>,
        context: &HostRequestContext,
        registry: &Arc<TerminalRegistry>,
        request: HostRequest,
    ) -> Result<HostResponse, dto::ReadRefusal> {
        let HostRequest::TerminalOpen {
            session,
            node,
            terminal_id,
            dimensions: size,
            confirmation,
        } = request
        else {
            return Err(invalid());
        };

        let dimensions = dimensions(size)?;
        let key = RunnerKey {
            session: id(session.clone())?,
            node: id(node.clone())?,
            terminal_id: id(terminal_id.clone())?,
        };
        match registry.recover_open(context, &key, dimensions).await {
            Ok(Some(terminal)) => {
                return Ok(HostResponse::Terminal {
                    terminal: view(terminal),
                });
            }
            Ok(None) => {}
            Err(TerminalError::OutcomeUnknown) => {
                return Ok(HostResponse::OperationOutcomeUnknown {});
            }
            Err(error) => return Err(refusal(error)),
        }
        let reservation = self.reserve_consent(
            context,
            &dto::OperatorAction::TerminalOpen {
                session,
                node,
                terminal_id,
                dimensions: size,
            },
            &confirmation,
        )?;
        match registry.open(context, key, dimensions).await {
            Ok(terminal) => Ok(HostResponse::Terminal {
                terminal: view(terminal),
            }),
            Err(TerminalError::OutcomeUnknown) => Ok(HostResponse::OperationOutcomeUnknown {}),
            Err(error) => {
                self.restore_consent(reservation);
                Err(refusal(error))
            }
        }
    }

    async fn attach_terminal(
        self: &Arc<Self>,
        context: &HostRequestContext,
        registry: &Arc<TerminalRegistry>,
        request: HostRequest,
    ) -> Result<HostResponse, dto::ReadRefusal> {
        let HostRequest::TerminalAttach {
            terminal,
            cursor,
            takeover,
            confirmation,
        } = request
        else {
            return Err(invalid());
        };

        let position = RunnerCursor {
            instance: id(cursor.instance.clone())?,
            offset: cursor.offset,
        };
        let reservation = self.reserve_consent(
            context,
            &dto::OperatorAction::TerminalAttach {
                terminal: terminal.clone(),
                cursor,
                takeover,
            },
            &confirmation,
        )?;
        let handle = handle(&terminal);
        if takeover {
            let mut state = self
                .terminals
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if let Err(error) = registry.revoke_controller(context, &handle) {
                drop(state);
                self.restore_consent(reservation);
                return Err(refusal(error));
            }
            let mut notify = Vec::new();
            for stream in state.streams.values_mut().filter(|s| {
                s.owner.0 == context.principal && s.terminal.id == terminal.id && !s.ended
            }) {
                stream.ended = true;
                stream.events.clear();
                stream.events.push_back(dto::TerminalEvent::Suspended {});
                notify.push(stream.owner.clone());
            }
            for owner in notify {
                if let Some(notify) = state.notifications.get(&owner) {
                    notify.notify_one();
                }
            }
        }
        let deadline = Instant::now() + std::time::Duration::from_secs(1);
        let controller = loop {
            match registry
                .attach(context, &handle, position.clone(), takeover)
                .await
            {
                Ok(controller) => break controller,
                Err(TerminalError::Busy | TerminalError::Refused(RunnerRefusal::TerminalBusy))
                    if takeover && Instant::now() < deadline =>
                {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                Err(TerminalError::OutcomeUnknown) => {
                    return Ok(HostResponse::OperationOutcomeUnknown {});
                }
                Err(error) => {
                    if !takeover {
                        self.restore_consent(reservation);
                    }
                    return Err(refusal(error));
                }
            }
        };
        let row = registry
            .list(context)
            .map_err(refusal)?
            .into_iter()
            .find(|v| v.handle.identity == terminal.id)
            .ok_or(dto::ReadRefusal::TerminalLost {})?;
        let row = view(row);
        let stream = self.start_terminal(
            context.clone(),
            row.terminal.clone(),
            row.input_next,
            controller,
        )?;
        Ok(HostResponse::TerminalAttached {
            terminal: row,
            stream,
            input_credit: WINDOW,
        })
    }

    async fn release_terminals(
        &self,
        context: &HostRequestContext,
        registry: &TerminalRegistry,
        session: &str,
    ) -> Result<(), dto::ReadRefusal> {
        let deadline = Instant::now() + std::time::Duration::from_secs(4);
        loop {
            let rows = registry.list(context).map_err(refusal)?;
            let mut remaining = false;
            for row in rows
                .into_iter()
                .filter(|row| row.key.session.as_str() == session)
            {
                let row = registry
                    .close(context, &row.handle, false)
                    .await
                    .map_err(refusal)?;
                remaining |= !matches!(
                    view(row).status,
                    dto::TerminalStatus::Exited | dto::TerminalStatus::Lost
                );
            }
            if !remaining {
                return if registry.resident_reason(session).is_none() {
                    Ok(())
                } else {
                    Err(dto::ReadRefusal::TerminalBusy {})
                };
            }
            if Instant::now() >= deadline {
                return Err(dto::ReadRefusal::TerminationUnknown {});
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }
}
