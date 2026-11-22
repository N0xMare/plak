use super::RootActor;

impl<P: plakous_host_protocol::ProtocolExtension> RootActor<'_, P> {
    /// Park outer operations, without blocking urgent planned release.
    pub(super) async fn wait_step_hold(&mut self) -> bool {
        let control = std::sync::Arc::clone(&self.role.events.step_control);
        control.observe_agent(self.opened.agent());
        loop {
            let mut changed = Box::pin(control.changed.notified());
            changed.as_mut().enable();
            if !control.requested() {
                break;
            }
            // Stopping a busy turn leaves its receipt to the idle owner.
            // Settle it here before reporting the boundary as parked.
            self.finish_turn_stops().await;
            let tree_id = super::super::rebind::durable_tree_id(
                &self.role.config.topology_rows(),
                &self.role.config.session,
            );
            control.owner_boundary(
                self.opened.agent(),
                None,
                crate::topology_runtime::lookup(&tree_id),
            );
            let watch = crate::interrupt::InterruptWatch::deferring(
                self.role.actions,
                &self.role.interrupt_state,
            );
            watch.poll();
            if self.role.actions.abandon_and_release_requested()
                || self.role.interrupt_state.borrow().has_deferred_quit()
            {
                return true;
            }
            let pending = self.role.interrupt_state.borrow_mut().take_step_control();
            if let Some(pending) = pending {
                let (action, _acknowledgement) = pending.into_parts();
                if let super::UserAction::StopSession { ticket } = action {
                    self.stop_session(ticket, None).await;
                }
                // An interrupt of an idle root never starts another turn.
                continue;
            }
            if !control.requested() {
                break;
            }
            tokio::select! {
                () = &mut changed => {},
                () = self.role.actions.ready() => {},
            }
        }
        false
    }
}
