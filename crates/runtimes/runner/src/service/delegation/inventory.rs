use plakous_runner_protocol::{Bind, LineStatus, Refusal, RunnerFrame};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::{RunnerService, lock};

impl RunnerService {
    pub(in crate::service) fn receive_carry_lines(
        &self,
        bind: &Bind,
        request_id: String,
        sender: &mpsc::UnboundedSender<RunnerFrame>,
    ) {
        if bind.version != self.hello().version || self.retention_context().is_none() {
            let _ = sender.send(super::super::refused(
                Some(request_id),
                Refusal::Unsupported {
                    what: "carry inventory requires an exact-triple bind to a retention-enabled daemon".into(),
                },
            ));
            return;
        }
        let service = self.clone();
        let host = bind.host_id.clone();
        let instance = bind.host_instance.clone();
        let sender = sender.clone();
        tokio::spawn(async move {
            let mut lines = service.lines(Some(&host)).await;
            let answer = match service
                .discover_retained_lines(&host, &instance, &mut lines)
                .await
            {
                Ok(()) => RunnerFrame::Lines { request_id, lines },
                Err(refusal) => super::super::refused(Some(request_id), refusal),
            };
            let _ = sender.send(answer);
        });
    }

    async fn discover_retained_lines(
        &self,
        host: &str,
        instance: &str,
        lines: &mut Vec<LineStatus>,
    ) -> Result<(), Refusal> {
        let Some(context) = self.retention_context() else {
            return Ok(());
        };
        let unavailable = || Refusal::Unsupported {
            what: "retained job inventory is unavailable; restore the original runner endpoint"
                .into(),
        };
        let scope = super::scope(host, instance).map_err(|_| unavailable())?;
        let retained = context
            .registry
            .discover(&scope, &context.origin)
            .await
            .map_err(|_| unavailable())?;
        // This is only a startup/recovery candidate, not native adoption or
        // proof of a terminal. The sweep must still acquire the head lease and
        // transfer before using its jobs.
        for row in retained.into_iter().filter(|row| !row.jobs.is_empty()) {
            let line = row.owner.line.as_str();
            if !lines.iter().any(|status| status.line == line) {
                lines.push(LineStatus {
                    host_id: host.into(),
                    line: line.into(),
                    owner: None,
                    adopted: false,
                    jobs: json!({"instance":self.hello().instance,"jobs":[]}),
                    foreground: false,
                });
            }
        }
        lines.sort_by(|left, right| left.line.cmp(&right.line));
        Ok(())
    }

    pub(in crate::service) fn delegated_line_inventory(
        &self,
        host: Option<&str>,
        lines: &mut Vec<LineStatus>,
    ) {
        let Some(context) = self.retention_context() else {
            return;
        };
        for entry in context
            .registry
            .all_entries()
            .into_iter()
            .filter(|entry| host.is_none_or(|host| entry.host == host))
        {
            let jobs: Vec<_> = entry
                .inventory
                .jobs
                .into_iter()
                .filter_map(|job| super::port::job_view(&job).ok())
                .collect();
            if jobs.is_empty() {
                continue;
            }
            let line = entry.to.line.as_str();
            if let Some(existing) = lines
                .iter_mut()
                .find(|row| row.host_id == entry.host && row.line == line)
            {
                if let Some(current) = existing.jobs.get_mut("jobs").and_then(Value::as_array_mut) {
                    for job in jobs {
                        if !current
                            .iter()
                            .any(|row| row["session_id"] == job["session_id"])
                        {
                            current.push(job);
                        }
                    }
                }
            } else {
                let owned = lock(&self.inner.tables).owner(&entry.host, line);
                lines.push(LineStatus {
                    host_id: entry.host,
                    line: line.into(),
                    adopted: owned.as_ref().is_some_and(|(_, adopted)| *adopted),
                    owner: owned.map(|(owner, _)| owner),
                    jobs: json!({"instance":self.hello().instance,"jobs":jobs}),
                    foreground: false,
                });
            }
        }
        lines
            .sort_by(|left, right| (&left.host_id, &left.line).cmp(&(&right.host_id, &right.line)));
    }
}
