use super::{
    HostPrincipal, HostRequestContext, ReadRefusal, WorkspaceReadFailure, WorkspaceTarget, failure,
};
use plakous::tools::WorkspaceReadCursor;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

#[derive(Default)]
pub(super) struct State {
    cursors: BTreeMap<String, Cursor>,
    pending: BTreeMap<String, (HostPrincipal, Option<String>)>,
}
struct Cursor {
    principal: HostPrincipal,
    generation: u64,
    target: WorkspaceTarget,
    identity: String,
    continuation: WorkspaceReadCursor,
    at: Instant,
}
impl State {
    pub fn get(
        &self,
        token: &str,
        context: &HostRequestContext,
        target: &WorkspaceTarget,
        identity: &str,
        path: &str,
    ) -> Result<WorkspaceReadCursor, ReadRefusal> {
        self.cursors
            .get(token)
            .filter(|c| {
                c.principal == context.principal
                    && c.generation == context.host_generation
                    && &c.target == target
                    && c.identity == identity
                    && c.continuation.path() == std::path::Path::new(path)
                    && c.at.elapsed() < Duration::from_secs(60)
            })
            .map(|c| c.continuation.clone())
            .ok_or_else(|| failure(WorkspaceReadFailure::InvalidCursor))
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
                continuation: cursor,
                at: Instant::now(),
            },
        );
        Ok(token)
    }
}

pub(super) struct Reservation<'a> {
    console: &'a super::Console,
    id: String,
}
impl<'a> Reservation<'a> {
    pub fn admit(
        console: &'a super::Console,
        context: &HostRequestContext,
        replaces: Option<&str>,
    ) -> Result<Self, ReadRefusal> {
        let mut files = console
            .files
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let state = &mut files.cursors;
        state
            .cursors
            .retain(|_, cursor| cursor.at.elapsed() < Duration::from_secs(60));
        if replaces.is_some_and(|token| {
            !state.cursors.contains_key(token)
                || state
                    .pending
                    .values()
                    .any(|(_, old)| old.as_deref() == Some(token))
        }) {
            return Err(failure(WorkspaceReadFailure::InvalidCursor));
        }
        let extra = usize::from(replaces.is_none());
        let new_pending = state
            .pending
            .values()
            .filter(|(_, old)| {
                old.as_ref()
                    .is_none_or(|old| !state.cursors.contains_key(old))
            })
            .count();
        let owned = state
            .cursors
            .values()
            .filter(|cursor| cursor.principal == context.principal)
            .count();
        let owned_pending = state
            .pending
            .values()
            .filter(|(principal, old)| {
                principal == &context.principal
                    && old
                        .as_ref()
                        .is_none_or(|old| !state.cursors.contains_key(old))
            })
            .count();
        if state.cursors.len() + new_pending + extra > 8192 || owned + owned_pending + extra > 64 {
            return Err(super::super::validation(
                "cursor",
                plakd_wire_types::console::ValidationCode::InvalidLimit,
            ));
        }
        let id = uuid::Uuid::new_v4().to_string();
        state.pending.insert(
            id.clone(),
            (context.principal.clone(), replaces.map(str::to_owned)),
        );
        Ok(Self { console, id })
    }
    pub fn consume(&self) {
        let mut files = self
            .console
            .files
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((_, Some(old))) = files.cursors.pending.get(&self.id).cloned() {
            files.cursors.cursors.remove(&old);
        }
    }
}
impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        self.console
            .files
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .cursors
            .pending
            .remove(&self.id);
    }
}
