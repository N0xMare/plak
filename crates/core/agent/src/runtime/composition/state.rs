//! A cell's private overlay; only a successful terminal may hand it to publication.

use crate::local_composition::{LocalCompositionResponse, StateError, StateOp, StateResult};
use crate::manifest::NamedState;

use super::CompositionOwner;

impl CompositionOwner {
    pub(in crate::runtime) fn apply_state_op(
        &mut self,
        cell_id: &str,
        op: StateOp,
        committed: &NamedState,
    ) -> Result<StateResult, StateError> {
        if self.cell() != Some(cell_id) {
            return Err(StateError::ForeignCell);
        }
        let state = self
            .state_overlay
            .as_ref()
            .filter(|(owner, _)| owner == cell_id)
            .map_or(committed, |(_, state)| state);
        match op {
            StateOp::Get { key } => state.get(&key).map(StateResult::Value),
            StateOp::List => Ok(StateResult::Keys(
                state
                    .values()
                    .iter()
                    .map(|(key, value)| (key.clone(), value.len()))
                    .collect(),
            )),
            StateOp::Put { key, value } => state.put(key, &value).map(|next| {
                self.state_overlay = Some((cell_id.to_owned(), next));
                StateResult::Stored
            }),
            StateOp::Drop { key } => {
                let existed = state.values().contains_key(&key);
                state.drop_key(&key).map(|next| {
                    self.state_overlay = Some((cell_id.to_owned(), next));
                    StateResult::Dropped(existed)
                })
            }
        }
    }

    pub(in crate::runtime) fn take_terminal_state(
        &mut self,
        response: &LocalCompositionResponse,
    ) -> Option<(String, NamedState)> {
        if !response.is_terminal() {
            return None;
        }
        // A definite response carries its overlay out before resource cleanup
        // can retire the cell or classify its generation as lost.
        self.state_overlay
            .take()
            .filter(|(owner, _)| self.cell() == Some(owner.as_str()) && owner == response.cell_id())
    }

    pub(in crate::runtime) fn finish_state(
        &mut self,
        response: &LocalCompositionResponse,
        terminal_state: Option<(String, NamedState)>,
        presented: bool,
    ) -> Option<NamedState> {
        if !presented {
            self.state_overlay = None;
            return None;
        }
        match response {
            LocalCompositionResponse::Yielded { .. } => None,
            LocalCompositionResponse::Result {
                cell_id,
                error: None,
                ..
            } => terminal_state
                .filter(|(owner, _)| owner == cell_id)
                .map(|(_, state)| state),
            LocalCompositionResponse::Result { error: Some(_), .. }
            | LocalCompositionResponse::Terminated { .. } => {
                self.state_overlay = None;
                None
            }
        }
    }
}
