use plakous_runner_protocol::LineStatus;
use serde_json::{Value, json};

use super::{RunnerService, lock};

impl RunnerService {
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
