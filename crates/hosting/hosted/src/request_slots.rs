//! One shared model-request budget per account, across the root and every
//! descendant this process runs.
//!
//! The product sets the per-account limit once it has read its configuration
//! (`[orchestration] max_concurrent_requests_per_account`); every session it
//! builds afterwards, root or child, draws on the budget of the account it
//! is bound to. A limit change takes effect for sessions built after it.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use plakous::agent::RequestSlots;

/// Requests one account may have in flight when the product sets no limit.
pub const DEFAULT_REQUESTS_PER_ACCOUNT: NonZeroUsize = NonZeroUsize::MIN.saturating_add(7);

static LIMIT: AtomicUsize = AtomicUsize::new(DEFAULT_REQUESTS_PER_ACCOUNT.get());

fn budgets() -> &'static Mutex<HashMap<String, RequestSlots>> {
    static BUDGETS: OnceLock<Mutex<HashMap<String, RequestSlots>>> = OnceLock::new();
    BUDGETS.get_or_init(Mutex::default)
}

/// Sets how many model requests each account may have in flight.
pub fn set_requests_per_account(limit: NonZeroUsize) {
    LIMIT.store(limit.get(), Ordering::Relaxed);
}

/// The process-wide budget of `account`, labelled with the account name.
#[must_use]
pub fn for_account(account: &str) -> RequestSlots {
    let limit =
        NonZeroUsize::new(LIMIT.load(Ordering::Relaxed)).unwrap_or(DEFAULT_REQUESTS_PER_ACCOUNT);
    let mut budgets = budgets()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match budgets.get(account) {
        Some(slots) if slots.limit() == limit => slots.clone(),
        _ => {
            let slots = RequestSlots::new(account, limit);
            budgets.insert(account.to_owned(), slots.clone());
            slots
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_limit_sizes_budgets_built_after_it() {
        assert_eq!(for_account("a").limit(), DEFAULT_REQUESTS_PER_ACCOUNT);
        assert_eq!(for_account("a").label(), "a");
        let three = NonZeroUsize::new(3).unwrap_or(DEFAULT_REQUESTS_PER_ACCOUNT);
        set_requests_per_account(three);
        assert_eq!(for_account("a").limit(), three);
        assert_eq!(for_account("b").limit(), three);
    }
}
