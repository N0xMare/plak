//! Native authority remains local; retained job handles stay with their exact
//! original worker. Every remote CAS follows a synced, build-local intent.

use plakous_runner_protocol::retention::{self as wire, Answer};
use plakous_runner_protocol::{LineOwner, RunnerFrame, TransferOwner};

use super::{RunnerService, lock, retention};

mod inventory;
mod port;
#[cfg(test)]
mod tests;

fn scope(host: &str, instance: &str) -> Result<wire::Bind, String> {
    Ok(wire::Bind {
        version: wire::VERSION,
        host_id: retention::id(host).map_err(|_| "invalid host identity")?,
        host_instance: retention::id(instance).map_err(|_| "invalid host instance")?,
    })
}

fn call(origin: &wire::Origin, id: &str, request: wire::Request) -> Result<wire::Call, String> {
    Ok(wire::Call {
        id: retention::id(id).map_err(|_| "invalid control identity")?,
        instance: origin.instance.clone(),
        mutation: None,
        request,
    })
}

impl RunnerService {
    pub(super) async fn delegate_line(
        &self,
        host: &str,
        instance: &str,
        transfer: &TransferOwner,
    ) -> Result<(), Box<RunnerFrame>> {
        let Some(context) = self.retention_context() else {
            return Ok(());
        };
        let refuse = |detail: String| {
            Box::new(RunnerFrame::TransferRefused {
                control_id: transfer.control_id.clone(),
                detail,
            })
        };
        let scope = scope(host, instance).map_err(refuse)?;
        let to = retention::owner(&transfer.to).map_err(|_| refuse("invalid owner".into()))?;
        let observed = context
            .registry
            .discover(&scope, &context.origin)
            .await
            .map_err(|error| refuse(format!("retained job inventory unavailable: {error}")))?;
        let existing = context.registry.entries(host, Some(&transfer.to.line));
        let mut intents = Vec::new();
        for inventory in observed
            .into_iter()
            .filter(|line| line.owner.line == to.line)
        {
            if inventory.origin == context.origin {
                continue;
            }
            if inventory.jobs.is_empty() && inventory.native_disabled {
                let from = existing
                    .iter()
                    .find(|entry| entry.inventory.origin == inventory.origin)
                    .map_or_else(|| inventory.owner.clone(), |entry| entry.to.clone());
                let intent = context
                    .registry
                    .intent(&scope, from, to.clone(), inventory)
                    .map_err(|error| refuse(error.to_string()))?;
                context
                    .registry
                    .update(
                        &scope,
                        &intent.inventory.origin,
                        &to,
                        Vec::new(),
                        intent.inventory.next_number,
                    )
                    .map_err(|error| refuse(error.to_string()))?;
                continue;
            }
            // A fully consumed original has no remaining authority or capacity
            // obligation. Its number floor stays in our own receipt.
            if existing.iter().any(|entry| {
                entry.confirmed
                    && entry.inventory.jobs.is_empty()
                    && entry.inventory.origin == inventory.origin
            }) {
                continue;
            }
            let saved = existing.iter().find(|entry| {
                entry.to == to
                    && entry.holder == instance
                    && entry.inventory.origin == inventory.origin
            });
            let from = saved.map_or_else(|| inventory.owner.clone(), |entry| entry.from.clone());
            if saved.is_none() && transfer.from.as_ref() != Some(&retention::native_owner(&from)) {
                return Err(Box::new(RunnerFrame::StaleOwner {
                    request_id: None,
                    control_id: Some(transfer.control_id.clone()),
                    current: retention::native_owner(&from),
                }));
            }
            let intent = context
                .registry
                .intent(&scope, from, to.clone(), inventory)
                .map_err(|error| refuse(format!("retention intent was not recorded: {error}")))?;
            intents.push(intent);
        }
        // All occupancy is durable before the first original worker changes
        // owner. A partial transfer can only recover its saved own tuple.
        self.account_retained();
        self.transfer_delegated(&context, &scope, &transfer.control_id, intents)
            .await
    }

    async fn transfer_delegated(
        &self,
        context: &retention::Context,
        scope: &wire::Bind,
        control_id: &str,
        intents: Vec<crate::retention::registry::Delegation>,
    ) -> Result<(), Box<RunnerFrame>> {
        let refuse = |detail: String| {
            Box::new(RunnerFrame::TransferRefused {
                control_id: control_id.into(),
                detail,
            })
        };
        for intent in intents {
            if intent.inventory.lost {
                context
                    .registry
                    .update(
                        scope,
                        &intent.inventory.origin,
                        &intent.to,
                        intent.inventory.jobs.clone(),
                        intent.inventory.next_number,
                    )
                    .map_err(|error| refuse(error.to_string()))?;
                continue;
            }
            let request = wire::Request::Transfer {
                from: intent.from,
                to: intent.to.clone(),
            };
            let request = call(&intent.inventory.origin, control_id, request).map_err(refuse)?;
            let (next_number, has_jobs) = match context
                .registry
                .call(scope, &intent.inventory.origin, &request)
                .await
            {
                Ok(Answer::Transferred {
                    next_number,
                    has_jobs,
                }) => (next_number, has_jobs),
                Err(error) if crate::retention::confirmed_loss(&error) => {
                    context
                        .registry
                        .lose(scope, &intent.inventory.origin, &intent.to)
                        .map_err(|error| refuse(error.to_string()))?;
                    continue;
                }
                answer => return Err(refuse(format!("retention transfer unresolved: {answer:?}"))),
            };
            // The accepted CAS itself proves the native ban. Persist that
            // witness before a separate inventory read can lose its reply.
            context
                .registry
                .update(
                    scope,
                    &intent.inventory.origin,
                    &intent.to,
                    if has_jobs {
                        intent.inventory.jobs.clone()
                    } else {
                        Vec::new()
                    },
                    next_number,
                )
                .map_err(|error| refuse(error.to_string()))?;
            if !has_jobs {
                continue;
            }
            self.refresh_retained(scope, &intent.inventory.origin, &intent.to)
                .await
                .map_err(|error| refuse(error.to_string()))?;
        }
        Ok(())
    }

    pub(super) fn account_retained(&self) {
        if let Some(context) = self.retention_context() {
            let tables = lock(&self.inner.tables);
            for (origin, count) in context
                .registry
                .occupancy(|host, line| !tables.retention_only(host, line))
            {
                plakous_tools::internal::account_external_jobs(&self.inner.capacity, origin, count);
            }
        }
    }

    pub(super) fn retention_maintenance(&self) -> tokio::task::JoinHandle<()> {
        let weak = std::sync::Arc::downgrade(&self.inner);
        tokio::spawn(async move {
            let mut cursor = 0usize;
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                let Some(inner) = weak.upgrade() else { return };
                let service = RunnerService { inner };
                if service
                    .inner
                    .closing
                    .load(std::sync::atomic::Ordering::Acquire)
                {
                    return;
                }
                service.refresh_one_delegation(cursor).await;
                cursor = cursor.wrapping_add(1);
            }
        })
    }

    async fn refresh_one_delegation(&self, cursor: usize) {
        let Some(context) = self.retention_context() else {
            return;
        };
        let entries = context
            .registry
            .all_entries()
            .into_iter()
            .filter(|entry| {
                !entry.inventory.jobs.is_empty()
                    && !entry.inventory.lost
                    && !lock(&self.inner.tables).retention_only(&entry.host, entry.to.line.as_str())
            })
            .collect::<Vec<_>>();
        if entries.is_empty() {
            return;
        }
        let entry = &entries[cursor % entries.len()];
        let Ok(scope) = scope(&entry.host, &entry.holder) else {
            return;
        };
        if let Err(error) = self
            .refresh_retained(&scope, &entry.inventory.origin, &entry.to)
            .await
            && crate::retention::confirmed_loss(&error)
        {
            let _ = context
                .registry
                .lose(&scope, &entry.inventory.origin, &entry.to);
        }
        self.account_retained();
    }

    async fn refresh_retained(
        &self,
        scope: &wire::Bind,
        origin: &wire::Origin,
        owner: &wire::Owner,
    ) -> std::io::Result<Vec<wire::Job>> {
        let context = self
            .retention_context()
            .ok_or_else(|| std::io::Error::other("retention registry is unavailable"))?;
        let mut jobs = Vec::new();
        let mut after = 0;
        let mut floor = 1;
        loop {
            let request = call(
                origin,
                "snapshot",
                wire::Request::JobsSnapshot {
                    owner: owner.clone(),
                    after,
                },
            )
            .map_err(std::io::Error::other)?;
            let Answer::Jobs {
                jobs: page,
                next,
                next_number,
                ..
            } = context.registry.call(scope, origin, &request).await?
            else {
                return Err(std::io::Error::other(
                    "original worker refused retained inventory",
                ));
            };
            jobs.extend(page);
            floor = floor.max(next_number);
            match next {
                Some(next) if next > after => after = next,
                Some(_) => return Err(std::io::Error::other("retained inventory did not advance")),
                None => break,
            }
        }
        context
            .registry
            .update(scope, origin, owner, jobs.clone(), floor)?;
        self.account_retained();
        Ok(context
            .registry
            .entries(scope.host_id.as_str(), Some(owner.line.as_str()))
            .into_iter()
            .find(|entry| entry.to == *owner && entry.inventory.origin == *origin)
            .map_or_else(Vec::new, |entry| entry.inventory.jobs))
    }

    pub(super) fn retained_number_floor(&self, host: &str, line: &str) -> u32 {
        self.retention_context().map_or(1, |context| {
            context
                .registry
                .entries(host, Some(line))
                .iter()
                .map(|entry| entry.inventory.next_number)
                .max()
                .unwrap_or(1)
        })
    }

    fn delegated_owner(&self, host: &str, instance: &str, owner: &LineOwner) -> bool {
        self.instance_authority(host, instance)
            && lock(&self.inner.tables).held_by(host, instance, owner)
    }
}
