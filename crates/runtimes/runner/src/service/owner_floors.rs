//! A normal bound host may read a bounded set of its own floors, without adopting them.
use super::{Refusal, RunnerFrame, RunnerService, lock, refused};

impl RunnerService {
    pub(super) fn receive_owner_floors(
        &self,
        host: &str,
        instance: &str,
        connection: u64,
        request_id: String,
        lines: Vec<String>,
        sender: &tokio::sync::mpsc::UnboundedSender<RunnerFrame>,
    ) {
        let (service, host, instance, sender) = (
            self.clone(),
            host.to_owned(),
            instance.to_owned(),
            sender.clone(),
        );
        tokio::spawn(async move {
            let _ = sender.send(
                service
                    .owner_floors(&host, &instance, connection, request_id, &lines)
                    .await,
            );
        });
    }

    pub(super) fn forget_owner_witnesses(
        &self,
        host: &str,
        instance: &str,
        connection: u64,
        request_id: String,
        lines: &[String],
    ) -> RunnerFrame {
        if !crate::client::owner_transfer::valid_floor_query(lines) {
            return refused(
                Some(request_id),
                Refusal::Malformed {
                    detail: "witness release requires 1..=256 unique bounded line IDs".into(),
                },
            );
        }
        if let Some(context) = self.retention_context() {
            let Ok(scope) = super::delegation::scope(host, instance) else {
                return refused(Some(request_id), Refusal::OwnerNotTransferred);
            };
            if context.registry.observation_connection(&scope) != Some(connection) {
                return refused(Some(request_id), Refusal::OwnerNotTransferred);
            }
            context.registry.forget_observations(host, instance, lines);
            self.account_retained();
        }
        RunnerFrame::OwnerWitnessesForgotten { request_id }
    }

    pub(super) async fn owner_floors(
        &self,
        host: &str,
        instance: &str,
        connection: u64,
        request_id: String,
        lines: &[String],
    ) -> RunnerFrame {
        if !crate::client::owner_transfer::valid_floor_query(lines) {
            return refused(Some(request_id), Refusal::Malformed {
                detail: "owner-floor reads require 1..=256 unique line IDs of at most 128 printable ASCII bytes".into(),
            });
        }
        let retained = if let Some(context) = self.retention_context() {
            let Ok(scope) = super::delegation::scope(host, instance) else {
                return refused(Some(request_id), Refusal::OwnerNotTransferred);
            };
            match context.registry.fresh_lines(&scope, &context.origin, lines, connection).await {
                Ok(lines) => lines,
                Err(_) => return refused(Some(request_id), Refusal::Unsupported {
                    what: "original-worker owner witnesses are unavailable; keep the group withheld and retry through the retained compatible build".into(),
                }),
            }
        } else {
            Vec::new()
        };
        let floors = lock(&self.inner.tables).read_owner_floors(host, lines);
        match floors {
            Ok(mut floors) => {
                for floor in &mut floors {
                    floor.retained = retained.iter()
                        .filter(|line| line.owner.line.as_str() == floor.line)
                        .map(|line| super::retention::native_owner(&line.owner))
                        .collect();
                }
                RunnerFrame::OwnerFloors { request_id, floors }
            }
            Err(_) => refused(Some(request_id), Refusal::Unsupported {
                what: "durable owner floors cannot be reloaded; retain the matching build and repair the records without discarding fences".into(),
            }),
        }
    }
}
