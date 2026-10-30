use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct TestWire;

impl FamilyWire for TestWire {
    const NAME: &'static str = "Test";
    const LEDGER_POLICY: LedgerPolicy = LedgerPolicy {
        require_service_admitted_for_dispatch: true,
        require_active_request_always: false,
        dual_auth_slots: false,
        clear_service_on_terminal_success: true,
    };
    const HOSTED_EFFECTS: bool = false;
    const PROJECTOR_TERMINAL: bool = false;
    const PROVIDER_DEADLINE_REPLAY_ONCE: bool = true;
    const BUILD_REVALIDATES_RETRY: bool = false;
    type StreamBinding = ();
    type Projector = ();
    type StreamError = ();
    type Projection = ModelResponse;
    type Stop = ();
    fn projector(
        (): (),
        _: Option<ModelProviderRequestId>,
        _: Arc<dyn Clock>,
        _: Arc<dyn IdGenerator>,
    ) -> Result<(), ()> {
        unreachable!()
    }
    fn push((): &mut (), _: &[u8]) -> Result<Vec<ModelEvent>, ()> {
        unreachable!()
    }
    fn finish((): &mut ()) -> Result<Vec<ModelEvent>, ()> {
        unreachable!()
    }
    fn projection((): ()) -> Result<ModelResponse, ()> {
        unreachable!()
    }
    fn request_id((): &()) -> Option<ModelProviderRequestId> {
        unreachable!()
    }
    fn completed_items((): &()) -> Option<u64> {
        None
    }
    fn usage_limit((): &()) -> bool {
        false
    }
    fn into_parts(response: ModelResponse) -> (ModelResponse, ()) {
        (response, ())
    }
}
