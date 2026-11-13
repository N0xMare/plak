//! A normal bound host may read a bounded set of its own floors, without adopting them.
use super::{Refusal, RunnerFrame, RunnerService, lock, refused};

impl RunnerService {
    pub(super) fn owner_floors(
        &self,
        host: &str,
        request_id: String,
        lines: &[String],
    ) -> RunnerFrame {
        if !crate::client::owner_transfer::valid_floor_query(lines) {
            return refused(Some(request_id), Refusal::Malformed {
                detail: "owner-floor reads require 1..=256 unique line IDs of at most 128 printable ASCII bytes".into(),
            });
        }
        let tables = lock(&self.inner.tables);
        match tables.read_owner_floors(host, lines) {
            Ok(floors) => RunnerFrame::OwnerFloors { request_id, floors },
            Err(_) => refused(Some(request_id), Refusal::Unsupported {
                what: "durable owner floors cannot be reloaded; retain the matching build and repair the records without discarding fences".into(),
            }),
        }
    }
}
