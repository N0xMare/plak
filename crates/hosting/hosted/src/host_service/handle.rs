//! Service handles share one state owner; diagnostic formatting never takes its lock.

use super::{Arc, HostService, ProtocolExtension};

impl<P: ProtocolExtension> std::fmt::Debug for HostService<P> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Do not lock session state or render queued observations during diagnostics.
        formatter
            .debug_struct("HostService")
            .field("root", &self.root)
            .field("capacity", &self.capacity)
            .finish_non_exhaustive()
    }
}

impl<P: ProtocolExtension> Clone for HostService<P> {
    fn clone(&self) -> Self {
        Self {
            actions: self.actions.clone(),
            root: self.root.clone(),
            capacity: self.capacity,
            nodes: self.nodes.clone(),
            state: Arc::clone(&self.state),
        }
    }
}
