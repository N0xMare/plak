use super::*;

#[test]
fn keyed_product_views_are_unindexed_but_intent_writes_are_not() {
    let mut tables = BTreeSet::from(["projector_cursors".to_owned(), "session_views".to_owned()]);
    for table in [
        "session_call_views",
        "session_open_turn_views",
        "session_process_views",
    ] {
        let single = BTreeSet::from([table.to_owned()]);
        assert!(projector_only("product", &single));
        assert!(!projector_only("runtime", &single));
        tables.insert(table.to_owned());
    }
    assert!(projector_only("product", &tables));
    for table in [
        "sessions",
        "submissions",
        "workflow_requests",
        "operator_notices",
    ] {
        let mut mixed = tables.clone();
        mixed.insert(table.to_owned());
        assert!(!projector_only("product", &mixed));
    }
    assert!(!projector_only("product", &BTreeSet::new()));
}

#[test]
fn rollback_releases_only_its_own_pending_commit() -> std::io::Result<()> {
    let trace = Arc::new(Trace {
        gate: Mutex::new(Gate {
            owner: None,
            ordinal: 0,
            store_indices: BTreeMap::new(),
            store_ordinals: BTreeMap::new(),
            file: tempfile::tempfile()?,
            kill: None,
        }),
        released: Condvar::new(),
    });
    let owner = Arc::new(Context {
        id: 1,
        store: "runtime",
        tables: Mutex::new(BTreeSet::from(["a".to_owned()])),
        trace: trace.clone(),
    });
    let other = Context {
        id: 2,
        store: "tree",
        tables: Mutex::new(BTreeSet::new()),
        trace: trace.clone(),
    };
    trace.acquire(owner.id);
    trace.acquire(owner.id);
    PENDING.with(|pending| {
        *pending.borrow_mut() = Some(Pending {
            context: owner.clone(),
            label: "commit",
        });
    });
    rolled_back(&other);
    assert_eq!(lock(&trace.gate).owner, Some(1));
    assert!(PENDING.with(|pending| pending.borrow().is_some()));
    rolled_back(&owner);
    assert_eq!(lock(&trace.gate).owner, None);
    assert!(PENDING.with(|pending| pending.borrow().is_none()));
    assert!(lock(&owner.tables).is_empty());
    trace.acquire(2);
    trace.release(2);
    Ok(())
}

#[test]
fn labels_restore_after_nested_scope_and_unwind() {
    assert_eq!(LABEL.get(), "unlabelled");
    let outer = label("create");
    {
        let _inner = label("renew");
        assert_eq!(LABEL.get(), "renew");
    }
    assert_eq!(LABEL.get(), "create");
    let _ = std::panic::catch_unwind(|| {
        let _inner = label("commit");
        panic!("test unwind");
    });
    assert_eq!(LABEL.get(), "create");
    drop(outer);
    assert_eq!(LABEL.get(), "unlabelled");
}

#[test]
fn settlement_guard_releases_frameless_claim_after_unwind() -> std::io::Result<()> {
    let trace = Arc::new(Trace {
        gate: Mutex::new(Gate {
            owner: None,
            ordinal: 0,
            store_indices: BTreeMap::new(),
            store_ordinals: BTreeMap::new(),
            file: tempfile::tempfile()?,
            kill: None,
        }),
        released: Condvar::new(),
    });
    let context = Arc::new(Context {
        id: 1,
        store: "runtime",
        tables: Mutex::new(BTreeSet::from(["a".to_owned()])),
        trace: trace.clone(),
    });
    let panic = std::panic::catch_unwind(|| {
        let _settle = SettleGuard;
        trace.acquire(context.id);
        PENDING.with(|pending| {
            *pending.borrow_mut() = Some(Pending {
                context: context.clone(),
                label: "commit",
            });
        });
        panic!("test operation unwind");
    });
    assert!(panic.is_err());
    assert!(PENDING.with(|pending| pending.borrow().is_none()));
    assert!(lock(&context.tables).is_empty());
    let gate = lock(&trace.gate);
    assert_eq!(gate.owner, None);
    assert_eq!(gate.ordinal, 0);
    assert!(gate.store_ordinals.is_empty());
    drop(gate);
    trace.acquire(2);
    trace.release(2);
    Ok(())
}
