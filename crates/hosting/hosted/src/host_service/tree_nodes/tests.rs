use std::sync::{Arc, Mutex};

use super::Answer;

/// A control spawned on an executor that already stopped is still
/// answered: the dropped answer reports that the executor stopped.
#[test]
fn a_control_on_a_stopped_executor_is_answered() {
    let answered: Arc<Mutex<Vec<Result<(), String>>>> = Arc::default();
    let executor = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let handle = executor.handle().clone();
    drop(executor);
    let done = Answer(Some(Box::new({
        let answered = Arc::clone(&answered);
        move |result| answered.lock().unwrap().push(result)
    })));
    drop(handle.spawn(async move {
        done.send(Ok(()));
    }));
    assert_eq!(
        *answered.lock().unwrap(),
        [Err("the agent tree's executor stopped".to_owned())]
    );
}

/// An answered control is answered once.
#[test]
fn an_answer_is_sent_once() {
    let answered: Arc<Mutex<Vec<Result<(), String>>>> = Arc::default();
    let done = Answer(Some(Box::new({
        let answered = Arc::clone(&answered);
        move |result| answered.lock().unwrap().push(result)
    })));
    done.send(Ok(()));
    assert_eq!(*answered.lock().unwrap(), [Ok(())]);
}
