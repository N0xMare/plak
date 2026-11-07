use crate::local_composition::LocalCompositionFuture;
use crate::runtime::composition::{
    CompositionOwner, LocalCompositionNestedDisposition, LocalCompositionNestedReplay,
};
use crate::runtime::operation_record::{NESTED_HEADROOM_REFUSAL, has_operation_headroom};
use serde_json::json;
use std::sync::{Arc, Mutex};

type Replies = Arc<Mutex<Vec<Result<serde_json::Value, String>>>>;
type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn lock_mutex<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct Reply(Replies);

impl crate::local_composition::LocalCompositionNestedResponder for Reply {
    fn resolve(
        self: Box<Self>,
        value: serde_json::Value,
    ) -> LocalCompositionFuture<'static, crate::local_composition::LocalCompositionNestedDelivery>
    {
        lock_mutex(&self.0).push(Ok(value));
        Box::pin(async { Ok(crate::local_composition::LocalCompositionNestedDelivery::Accepted) })
    }

    fn reject(
        self: Box<Self>,
        reason: String,
    ) -> LocalCompositionFuture<'static, crate::local_composition::LocalCompositionNestedDelivery>
    {
        lock_mutex(&self.0).push(Err(reason));
        Box::pin(async { Ok(crate::local_composition::LocalCompositionNestedDelivery::Accepted) })
    }
}

fn owner() -> TestResult<CompositionOwner> {
    let mut owner = CompositionOwner::new(
        crate::runtime::durable::tests::headroom_test_construction()?,
        None,
    );
    owner.cell = Some("C".into());
    Ok(owner)
}

fn call(id: usize) -> crate::local_composition::LocalCompositionNestedCall {
    crate::local_composition::LocalCompositionNestedCall {
        cell_id: "C".into(),
        runtime_call_id: format!("request-{id}"),
        tool_name: "exec_command".into(),
        input: Some(json!({"cmd":"true"})),
    }
}

#[tokio::test]
async fn headroom_refuses_new_nested_work_before_sequence_but_replays_old_identity() -> TestResult {
    let mut owner = owner()?;
    let replies = Replies::default();
    owner.admit_nested(call(0), Box::new(Reply(replies.clone())), 3_584)?;
    assert_eq!(owner.settle_deliveries().await.len(), 0);
    assert_eq!(
        *lock_mutex(&replies),
        vec![Err(NESTED_HEADROOM_REFUSAL.into())]
    );
    assert_eq!(owner.nested_sequence, 0);
    assert_eq!(owner.nested_replays.len(), 0);
    assert_eq!(owner.pending_nested.len(), 0);
    assert_eq!(owner.child_operations.len(), 0);

    let request = call(1);
    owner.nested_replays.insert(
        request.runtime_call_id.clone(),
        LocalCompositionNestedReplay {
            request_digest:
                crate::runtime::composition::nested::local_composition_nested_request_digest(
                    &request,
                )?,
            request_sequence: 1,
            disposition: LocalCompositionNestedDisposition::Resolved(json!("committed")),
            waiters: Vec::new(),
        },
    );
    owner.admit_nested(request, Box::new(Reply(replies.clone())), 4_096)?;
    assert_eq!(owner.settle_deliveries().await.len(), 0);
    assert_eq!(lock_mutex(&replies).last(), Some(&Ok(json!("committed"))));
    // Refusal did not consume the sequence or poison the owner: later work fits.
    owner.admit_nested(call(2), Box::new(Reply(replies)), 100)?;
    assert_eq!(owner.nested_sequence, 1);
    assert_eq!(owner.pending_nested.len(), 1);
    Ok(())
}

#[tokio::test]
async fn headroom_reserves_all_sixteen_accepted_nested_requests() -> TestResult {
    let mut owner = owner()?;
    let replies = Replies::default();
    for id in 0..16 {
        owner.admit_nested(call(id), Box::new(Reply(replies.clone())), 3_516)?;
    }
    assert_eq!(owner.pending_nested.len(), 16);
    assert_eq!(owner.nested_sequence, 16);
    owner.admit_nested(call(16), Box::new(Reply(replies.clone())), 3_524)?;
    assert_eq!(owner.settle_deliveries().await.len(), 0);
    assert_eq!(
        *lock_mutex(&replies),
        vec![Err(NESTED_HEADROOM_REFUSAL.into())]
    );
    assert_eq!(owner.nested_sequence, 16);
    assert!(!has_operation_headroom(3_524, 15));
    assert!(has_operation_headroom(3_516, 15));
    assert!(!has_operation_headroom(usize::MAX, usize::MAX));
    // 16 maximal four-record paths plus boundary/terminal work fit the reserve.
    assert!(owner.pending_nested.len() * 4 + 8 < 512);
    Ok(())
}
