use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

pub(in crate::agent) type Hook =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

static BEFORE_WAKE_ADMISSION: Mutex<Option<HashMap<String, Hook>>> = Mutex::new(None);
static BEFORE_FAILED_WAKE_END: Mutex<Option<HashMap<String, Hook>>> = Mutex::new(None);
static AFTER_FOLLOW_UP_ADMISSION: Mutex<Option<HashMap<String, Hook>>> = Mutex::new(None);
static AFTER_RECOVERED_OCCUPY: Mutex<Option<HashMap<String, Hook>>> = Mutex::new(None);
static AROUND_REOPEN_SPAWNED_CHILD: Mutex<Option<HashMap<String, Hook>>> = Mutex::new(None);
static BEFORE_LOSS_CONTINUATION_RESERVE: Mutex<Option<HashMap<String, Hook>>> = Mutex::new(None);
static AFTER_LOSS_CONTINUATION_ADMISSION: Mutex<Option<HashMap<String, Hook>>> = Mutex::new(None);

fn set(slot: &Mutex<Option<HashMap<String, Hook>>>, agent_id: &str, hook: Hook) {
    slot.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get_or_insert_with(HashMap::new)
        .insert(agent_id.to_owned(), hook);
}

async fn run(slot: &Mutex<Option<HashMap<String, Hook>>>, agent_id: &str) {
    let hook = slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .and_then(|hooks| hooks.get(agent_id).cloned());
    if let Some(hook) = hook {
        hook().await;
    }
}

/// Runs `hook` after a wake began (the child is marked waking) and
/// before its Start is admitted.
pub(in crate::agent) fn set_before_wake_admission(agent_id: &str, hook: Hook) {
    set(&BEFORE_WAKE_ADMISSION, agent_id, hook);
}

pub(in crate::agent) async fn before_wake_admission(agent_id: &str) {
    run(&BEFORE_WAKE_ADMISSION, agent_id).await;
}

/// Runs `hook` after a wake that was not admitted marked the child
/// ending, before the attempt ends.
pub(in crate::agent) fn set_before_failed_wake_end(agent_id: &str, hook: Hook) {
    set(&BEFORE_FAILED_WAKE_END, agent_id, hook);
}

pub(in crate::agent) async fn before_failed_wake_end(agent_id: &str) {
    run(&BEFORE_FAILED_WAKE_END, agent_id).await;
}

pub(in crate::agent) fn set_after_follow_up_admission(agent_id: &str, hook: Hook) {
    set(&AFTER_FOLLOW_UP_ADMISSION, agent_id, hook);
}

pub(in crate::agent) async fn after_follow_up_admission(agent_id: &str) {
    run(&AFTER_FOLLOW_UP_ADMISSION, agent_id).await;
}

/// Runs `hook` after a mail Start's rejoin handed its recovered turn to
/// a turn waiter, before the rejoin returns.
pub(in crate::agent) fn set_after_recovered_occupy(agent_id: &str, hook: Hook) {
    set(&AFTER_RECOVERED_OCCUPY, agent_id, hook);
}

pub(in crate::agent) async fn after_recovered_occupy(agent_id: &str) {
    run(&AFTER_RECOVERED_OCCUPY, agent_id).await;
}

/// Runs `hook` after a mail Start has reopened the child session and
/// before it reserves the live slot. That is the window where another
/// Start can take the slot.
pub(in crate::agent) fn set_around_reopen_spawned_child(agent_id: &str, hook: Hook) {
    set(&AROUND_REOPEN_SPAWNED_CHILD, agent_id, hook);
}

pub(in crate::agent) fn clear_around_reopen_spawned_child(agent_id: &str) {
    if let Some(hooks) = AROUND_REOPEN_SPAWNED_CHILD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_mut()
    {
        hooks.remove(agent_id);
    }
}

pub(in crate::agent) async fn around_reopen_spawned_child(agent_id: &str) {
    run(&AROUND_REOPEN_SPAWNED_CHILD, agent_id).await;
}

/// Runs `hook` after a continuation after a loss recorded consent and
/// reopened the child, before its Start reserves the child.
pub(in crate::agent) fn set_before_loss_continuation_reserve(agent_id: &str, hook: Hook) {
    set(&BEFORE_LOSS_CONTINUATION_RESERVE, agent_id, hook);
}

pub(in crate::agent) async fn before_loss_continuation_reserve(agent_id: &str) {
    run(&BEFORE_LOSS_CONTINUATION_RESERVE, agent_id).await;
}

/// Runs `hook` after the runtime admitted a continuation's command, before
/// its turn binds.
pub(in crate::agent) fn set_after_loss_continuation_admission(agent_id: &str, hook: Hook) {
    set(&AFTER_LOSS_CONTINUATION_ADMISSION, agent_id, hook);
}

pub(in crate::agent) async fn after_loss_continuation_admission(agent_id: &str) {
    run(&AFTER_LOSS_CONTINUATION_ADMISSION, agent_id).await;
}
