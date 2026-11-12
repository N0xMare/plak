use super::{
    HostPrincipal, HostRequestContext, ReadRefusal, WorkspaceReadFailure, WorkspaceTarget, failure,
};
use plakous::tools::WorkspaceReadCursor;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

#[derive(Default)]
pub(super) struct State {
    cursors: BTreeMap<String, Cursor>,
}
struct Cursor {
    principal: HostPrincipal,
    generation: u64,
    target: WorkspaceTarget,
    identity: String,
    cursor: WorkspaceReadCursor,
    at: Instant,
}
impl State {
    pub fn get(
        &mut self,
        token: &str,
        context: &HostRequestContext,
        target: &WorkspaceTarget,
        identity: &str,
        path: &str,
    ) -> Result<WorkspaceReadCursor, ReadRefusal> {
        let cursor = self
            .cursors
            .get(token)
            .filter(|c| {
                c.principal == context.principal
                    && c.generation == context.host_generation
                    && &c.target == target
                    && c.identity == identity
                    && c.cursor.path() == std::path::Path::new(path)
                    && c.at.elapsed() < Duration::from_secs(60)
            })
            .map(|c| c.cursor.clone())
            .ok_or_else(|| failure(WorkspaceReadFailure::InvalidCursor))?;
        self.cursors.remove(token);
        Ok(cursor)
    }
    pub fn insert(
        &mut self,
        context: &HostRequestContext,
        target: WorkspaceTarget,
        identity: String,
        cursor: WorkspaceReadCursor,
    ) -> Result<String, ReadRefusal> {
        self.cursors
            .retain(|_, cursor| cursor.at.elapsed() < Duration::from_secs(60));
        if self.cursors.len() >= 8192
            || self
                .cursors
                .values()
                .filter(|c| c.principal == context.principal)
                .count()
                >= 64
        {
            return Err(super::super::validation(
                "cursor",
                plakd_wire_types::console::ValidationCode::InvalidLimit,
            ));
        }
        let token = uuid::Uuid::new_v4().to_string();
        self.cursors.insert(
            token.clone(),
            Cursor {
                principal: context.principal.clone(),
                generation: context.host_generation,
                target,
                identity,
                cursor,
                at: Instant::now(),
            },
        );
        Ok(token)
    }
}
