//! Discover surviving runner authority; an empty replacement is not old-instance proof.

use super::{
    Binding, Entry, OperatorAnswer, OperatorNamespace, OperatorOperation, TerminalError,
    TerminalPin, TerminalRegistry, id, pins,
};

impl TerminalRegistry {
    pub(super) async fn synchronize_inventory(
        &self,
        runner: &str,
        binding: &Binding,
        operation: OperatorOperation,
    ) -> Result<Vec<OperatorNamespace>, TerminalError> {
        let answer = Self::request(
            binding,
            OperatorNamespace {
                state_key: self.state_key.clone(),
                principal: id("host-inventory".into())?,
            },
            operation,
        )
        .await?;
        let OperatorAnswer::Inventory {
            instance,
            pins: inventory,
        } = answer
        else {
            return Err(TerminalError::Unavailable);
        };
        if instance.as_str() != binding.client.instance() || inventory.len() > 32 {
            return Err(TerminalError::Unavailable);
        }
        let mut present = Vec::new();
        let mut namespaces = Vec::new();
        for item in inventory {
            if item.namespace.state_key != self.state_key || item.terminal.next.instance != instance
            {
                return Err(TerminalError::Unavailable);
            }
            let pin = TerminalPin {
                host_generation: 0,
                host_id: self.host_id.clone(),
                generation: item.generation.as_str().into(),
                namespace: item.namespace,
                runner: runner.into(),
                instance: instance.as_str().into(),
                key: item.terminal.key.clone(),
                workspace: pins::physical(runner, std::path::Path::new(&item.workspace))?,
                digest: item.digest,
            };
            let identity = pin.identity().map_err(|_| TerminalError::Conflict)?;
            if pin.generation == self.runner_generation && !namespaces.contains(&pin.namespace) {
                namespaces.push(pin.namespace.clone());
            }
            present.push(identity.clone());
            let hold = self
                .hold
                .get()
                .and_then(|hold| hold(pin.key.session.as_str()).ok());
            let mut state = self.lock();
            if state.entries.len() >= 1024 && !state.entries.contains_key(&identity) {
                return Err(TerminalError::Refused(super::OperatorRefusal::Quota));
            }
            let hold = state
                .residents
                .get(pin.key.session.as_str())
                .and_then(plakous_hosted::host_service::AttachmentCount::hold)
                .or(hold);
            state.entries.entry(identity.clone()).or_insert(Entry {
                lost: true,
                pin,
                pinned: true,
                row: None,
                hold,
            });
            drop(state);
            self.observe(&identity, item.terminal)?;
        }
        self.release_absent(runner, instance.as_str(), &present)?;
        Ok(namespaces)
    }

    fn release_absent(
        &self,
        runner: &str,
        instance: &str,
        present: &[String],
    ) -> Result<(), TerminalError> {
        let prior: Vec<_> = self
            .lock()
            .entries
            .iter()
            .filter(|(_, entry)| entry.pinned && entry.pin.runner == runner)
            .map(|(identity, entry)| (identity.clone(), entry.pin.instance.clone()))
            .collect();
        for (identity, prior_instance) in prior {
            if prior_instance != instance {
                self.mark_lost(&identity);
            } else if !present.contains(&identity) {
                // A complete same-instance inventory excludes every still-pinned
                // terminal; the runner releases one only on proof or explicit loss.
                {
                    let mut state = self.lock();
                    if let Some(entry) = state.entries.get_mut(&identity)
                        && entry.row.as_ref().is_none_or(|row| {
                            !matches!(
                                row.status,
                                super::TerminalStatus::Exited | super::TerminalStatus::Lost
                            )
                        })
                    {
                        entry.lost = true;
                        entry.row = None;
                    }
                }
                self.release_proven(&identity)?;
            }
        }
        Ok(())
    }

    pub(super) fn observe_table(
        &self,
        runner: &str,
        namespace: &OperatorNamespace,
        rows: Vec<super::TerminalRow>,
    ) -> Result<(), TerminalError> {
        for row in rows {
            let identity = self
                .lock()
                .entries
                .iter()
                .find(|(_, entry)| {
                    entry.pin.runner == runner
                        && &entry.pin.namespace == namespace
                        && entry.pin.generation == self.runner_generation
                        && entry.pin.key == row.key
                        && entry.pin.instance == row.next.instance.as_str()
                })
                .map(|(identity, _)| identity.clone());
            if let Some(identity) = identity {
                self.observe(&identity, row)?;
            }
        }
        Ok(())
    }

    pub(super) async fn clean_discovered(
        &self,
        runner: &str,
        binding: &Binding,
    ) -> Result<(), TerminalError> {
        let pins: Vec<_> = self
            .lock()
            .entries
            .values()
            .filter(|entry| {
                entry.pinned
                    && entry.lost
                    && entry.pin.runner == runner
                    && entry.pin.instance == binding.client.instance()
            })
            .map(|entry| entry.pin.clone())
            .collect();
        for pin in pins {
            let answer = Self::request(
                binding,
                pin.namespace.clone(),
                OperatorOperation::Close {
                    key: pin.key.clone(),
                    generation: Some(id(pin.generation.clone())?),
                    acknowledge_loss: false,
                },
            )
            .await;
            if let Ok(OperatorAnswer::Terminal { terminal, .. }) = answer {
                self.observe(
                    &pin.identity().map_err(|_| TerminalError::Conflict)?,
                    terminal,
                )?;
            }
        }
        Ok(())
    }

    pub(super) async fn refresh_completed_rows(&self, runner: &str, binding: &Binding) {
        let namespaces: Vec<_> = self
            .lock()
            .entries
            .values()
            .filter(|entry| {
                !entry.pinned
                    && entry.row.is_none()
                    && entry.pin.runner == runner
                    && entry.pin.instance == binding.client.instance()
                    && entry.pin.generation == self.runner_generation
            })
            .map(|entry| entry.pin.namespace.clone())
            .collect();
        for namespace in namespaces {
            if let Ok(OperatorAnswer::Table { terminals }) =
                Self::request(binding, namespace.clone(), OperatorOperation::List).await
            {
                let _ = self.observe_table(runner, &namespace, terminals);
            }
        }
    }
}
