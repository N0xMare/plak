use super::{Console, HostPrincipal, HostRequest, INPUT, PAGE};
use plakous_host_protocol::wire::RequestRefusal;
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, PoisonError};
use std::time::Instant;

#[derive(Default)]
pub(super) struct State {
    principals: BTreeMap<HostPrincipal, Quota>,
    active: usize,
}
struct Bucket {
    available: u64,
    burst: u64,
    rate: u64,
    at: Instant,
}
impl Bucket {
    fn new(rate: u64, burst: u64, now: Instant) -> Self {
        Self {
            available: burst,
            burst,
            rate,
            at: now,
        }
    }
    fn refill(&mut self, now: Instant) {
        let units = now
            .duration_since(self.at)
            .as_nanos()
            .saturating_mul(u128::from(self.rate))
            / 1_000_000_000;
        if units > 0 {
            self.available = self.burst.min(
                self.available
                    .saturating_add(u64::try_from(units).unwrap_or(u64::MAX)),
            );
            self.at = now;
        }
    }
}
struct Quota {
    requests: Bucket,
    output: Bucket,
    input: Bucket,
    search: VecDeque<Instant>,
    active: usize,
    searches: usize,
    changes: usize,
}
impl Quota {
    fn new(now: Instant) -> Self {
        Self {
            requests: Bucket::new(10, 20, now),
            output: Bucket::new(4 * 1024 * 1024, 8 * 1024 * 1024, now),
            input: Bucket::new(32 * 1024 * 1024, 64 * 1024 * 1024, now),
            search: VecDeque::new(),
            active: 0,
            searches: 0,
            changes: 0,
        }
    }
}
pub(super) struct Guard {
    console: Arc<Console>,
    principal: HostPrincipal,
    search: bool,
    changes: bool,
}
impl Guard {
    pub fn admit(
        console: Arc<Console>,
        principal: &HostPrincipal,
        request: &HostRequest,
    ) -> Result<Self, RequestRefusal> {
        Self::admit_at(console, principal, request, Instant::now())
    }

    fn admit_at(
        console: Arc<Console>,
        principal: &HostPrincipal,
        request: &HostRequest,
        now: Instant,
    ) -> Result<Self, RequestRefusal> {
        let search = matches!(request, HostRequest::FilesSearch { .. });
        let changes = matches!(
            request,
            HostRequest::ChangesStatus { .. } | HostRequest::ChangesDiff { .. }
        );
        let input = if matches!(request, HostRequest::FilesRead { .. }) || search {
            INPUT
        } else {
            0
        };
        let output = match request {
            HostRequest::FilesRead { max_bytes, .. } => max_bytes.unwrap_or(PAGE),
            _ => PAGE,
        };
        let mut state = console.files.lock().unwrap_or_else(PoisonError::into_inner);
        let state = &mut state.quota;
        let refused = || RequestRefusal::RateLimited {
            retry_after_ms: 1_000,
        };
        state
            .principals
            .retain(|_, q| q.active > 0 || now.duration_since(q.requests.at).as_secs() < 120);
        if state.active >= 16
            || (!state.principals.contains_key(principal) && state.principals.len() >= 1024)
        {
            return Err(refused());
        }
        let quota = state
            .principals
            .entry(principal.clone())
            .or_insert_with(|| Quota::new(now));
        for bucket in [&mut quota.requests, &mut quota.output, &mut quota.input] {
            bucket.refill(now);
        }
        quota
            .search
            .retain(|at| now.duration_since(*at).as_secs() < 60);
        if quota.active >= 2
            || (search && (quota.searches > 0 || quota.search.len() >= 6))
            || (changes && quota.changes > 0)
            || quota.requests.available == 0
            || quota.output.available < output
            || quota.input.available < input
        {
            return Err(refused());
        }
        quota.requests.available -= 1;
        quota.output.available -= output;
        quota.input.available -= input;
        if search {
            quota.searches += 1;
            quota.search.push_back(now);
        }
        if changes {
            quota.changes += 1;
        }
        quota.active += 1;
        state.active += 1;
        Ok(Self {
            console: Arc::clone(&console),
            principal: principal.clone(),
            search,
            changes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{console, target};
    use super::*;

    #[tokio::test]
    async fn request_burst_refills_without_allocating_refused_work() {
        let root = tempfile::tempdir().unwrap();
        let console = console(root.path());
        let request = HostRequest::FilesList {
            target: target(),
            path: ".".into(),
        };
        let principal = HostPrincipal::Socket(1);
        let now = Instant::now();
        for _ in 0..20 {
            drop(Guard::admit_at(Arc::clone(&console), &principal, &request, now).unwrap());
        }
        assert!(Guard::admit_at(Arc::clone(&console), &principal, &request, now).is_err());
        assert_eq!(console.files.lock().unwrap().quota.active, 0);
        assert!(
            Guard::admit_at(
                Arc::clone(&console),
                &principal,
                &request,
                now + std::time::Duration::from_millis(100)
            )
            .is_ok()
        );
    }

    #[tokio::test]
    async fn search_starts_use_a_rolling_minute_and_one_active_search() {
        let root = tempfile::tempdir().unwrap();
        let console = console(root.path());
        let request = HostRequest::FilesSearch {
            target: target(),
            path: ".".into(),
            literal: "needle".into(),
            match_case: None,
        };
        let principal = HostPrincipal::Socket(1);
        let now = Instant::now();
        let first = Guard::admit_at(Arc::clone(&console), &principal, &request, now).unwrap();
        assert!(Guard::admit_at(Arc::clone(&console), &principal, &request, now).is_err());
        drop(first);
        for n in 1..6 {
            drop(
                Guard::admit_at(
                    Arc::clone(&console),
                    &principal,
                    &request,
                    now + std::time::Duration::from_secs(n * 5),
                )
                .unwrap(),
            );
        }
        assert!(
            Guard::admit_at(
                Arc::clone(&console),
                &principal,
                &request,
                now + std::time::Duration::from_secs(59)
            )
            .is_err()
        );
        assert!(
            Guard::admit_at(
                Arc::clone(&console),
                &principal,
                &request,
                now + std::time::Duration::from_secs(60)
            )
            .is_ok()
        );
    }

    #[tokio::test]
    async fn output_budget_changes_serialization_and_process_wide_slots_are_independent() {
        let root = tempfile::tempdir().unwrap();
        let console = console(root.path());
        let request = HostRequest::ChangesStatus { target: target() };
        let principal = HostPrincipal::Socket(1);
        let now = Instant::now();
        let admit = |principal: &HostPrincipal| {
            Guard::admit_at(Arc::clone(&console), principal, &request, now)
        };
        let first = admit(&principal).unwrap();
        assert!(admit(&principal).is_err());
        drop(first);
        console
            .files
            .lock()
            .unwrap()
            .quota
            .principals
            .get_mut(&principal)
            .unwrap()
            .output
            .available = 0;
        assert!(admit(&principal).is_err());
        let guards = (2..18)
            .map(|uid| admit(&HostPrincipal::Socket(uid)).unwrap())
            .collect::<Vec<_>>();
        assert!(admit(&HostPrincipal::Socket(19)).is_err());
        drop(guards);
        assert!(admit(&HostPrincipal::Socket(19)).is_ok());
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        let mut state = self
            .console
            .files
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let state = &mut state.quota;
        state.active = state.active.saturating_sub(1);
        if let Some(quota) = state.principals.get_mut(&self.principal) {
            quota.active -= 1;
            if self.search {
                quota.searches -= 1;
            }
            if self.changes {
                quota.changes -= 1;
            }
        }
    }
}
