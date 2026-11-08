use super::*;

#[test]
fn lifecycle_slot_predecessor_mutation_keeps_successor_serving() {
    let Switched {
        host,
        fake,
        slot,
        mut connection,
        out,
    } = switched();
    assert!(Arc::ptr_eq(&host.slot("s1"), &slot));
    request(
        &mut connection,
        3,
        Op::DeleteSession {
            session: "s1".into(),
            abandon_children: false,
        },
    );
    assert_eq!(
        outcome(&out.take(), 3),
        Outcome::Ok(Answer::Deleted {
            session: "s1".into()
        })
    );
    assert_eq!(fake.0.active_workers.load(Ordering::SeqCst), 1);
    assert_eq!(fake.0.opens.load(Ordering::SeqCst), 1);
    assert_eq!(host.slots().len(), 1);
    assert!(Arc::ptr_eq(&host.slot("s9"), &slot));
    assert!(
        matches!(slot.peek(),SlotView::Resident(served) if served.session_id()=="s9" && !served.ended())
    );
    connection.leave();
    handed_off(&host);
}

#[test]
fn lifecycle_slot_successor_releases_the_actor_through_its_predecessor_slot() {
    let Switched {
        host,
        fake,
        slot,
        mut connection,
        out,
    } = switched();
    assert!(Arc::ptr_eq(&host.slot("s9"), &slot));
    request(
        &mut connection,
        3,
        Op::ArchiveSession {
            session: "s9".into(),
            archived: true,
        },
    );
    assert_eq!(
        outcome(&out.take(), 3),
        Outcome::Ok(Answer::ArchiveUpdated {
            session: "s9".into(),
            archived: true
        })
    );
    assert_eq!(fake.0.active_workers.load(Ordering::SeqCst), 0);
    assert_eq!(fake.0.opens.load(Ordering::SeqCst), 1);
    assert_eq!(host.slots().len(), 1);
    assert_eq!(slot.known(), "s1");
    assert!(matches!(slot.peek(),SlotView::Dormant(id) if id=="s1"));
    connection.leave();
    handed_off(&host);
}

#[test]
fn lifecycle_unknown_ids_do_not_allocate_slots_and_delete_drops_its_slot() {
    let (host, _) = host(&["s1"]);
    let out = Out::default();
    let mut connection = host.connect(out.clone(), Face::Remote, || false);
    request(
        &mut connection,
        1,
        Op::ArchiveSession {
            session: "missing".into(),
            archived: true,
        },
    );
    assert!(matches!(
        outcome(&out.take(), 1),
        Outcome::Refused(RequestRefusal::SessionLifecycleRefused {
            reason: plakous_host_protocol::wire::LifecycleReason::NotFound,
            ..
        })
    ));
    assert!(host.slots().is_empty());
    request(
        &mut connection,
        2,
        Op::DeleteSession {
            session: "s1".into(),
            abandon_children: false,
        },
    );
    assert!(matches!(
        outcome(&out.take(), 2),
        Outcome::Ok(Answer::Deleted { .. })
    ));
    assert!(host.slots().is_empty());
}

#[test]
fn lifecycle_slot_waits_for_worker_join_before_mutation() {
    let (host, fake) = host(&["s1"]);
    fake.0.linger.store(150, Ordering::SeqCst);
    let out = Out::default();
    let mut connection = host.connect(out.clone(), Face::Remote, || false);
    request(&mut connection, 1, Op::Attach { node: node("s1") });
    attached(&out.take(), 1);
    request(
        &mut connection,
        2,
        Op::ArchiveSession {
            session: "s1".into(),
            archived: true,
        },
    );
    assert!(matches!(
        outcome(&out.take(), 2),
        Outcome::Ok(Answer::ArchiveUpdated { .. })
    ));
    assert_eq!(fake.0.active_workers.load(Ordering::SeqCst), 0);
    connection.leave();
    handed_off(&host);
}

#[test]
fn lifecycle_unarchive_noop_keeps_a_busy_actor_running() {
    let (host, fake) = host(&["s1"]);
    let out = Out::default();
    let mut connection = host.connect(out.clone(), Face::Remote, || false);
    request(&mut connection, 1, Op::Attach { node: node("s1") });
    let attachment = attached(&out.take(), 1);
    prompt(&mut connection, 2, attachment, "busy");
    let until = Instant::now() + Duration::from_secs(10);
    while row_of(&host, "s1").activity != SessionActivity::Busy {
        assert!(Instant::now() < until, "the actor never became busy");
        std::thread::yield_now();
    }
    request(
        &mut connection,
        3,
        Op::ArchiveSession {
            session: "s1".into(),
            archived: false,
        },
    );
    assert_eq!(
        outcome(&out.take(), 3),
        Outcome::Ok(Answer::ArchiveUpdated {
            session: "s1".into(),
            archived: false,
        })
    );
    assert_eq!(fake.0.active_workers.load(Ordering::SeqCst), 1);
    assert_eq!(fake.0.opens.load(Ordering::SeqCst), 1);
    connection.leave();
    handed_off(&host);
}

#[test]
fn lifecycle_releases_the_resident_actor_before_archive_or_delete_without_reopening() {
    for delete in [false, true] {
        let (host, fake) = host(&["s1"]);
        let out = Out::default();
        let mut connection = host.connect(out.clone(), Face::Remote, || false);
        request(&mut connection, 1, Op::Attach { node: node("s1") });
        let _attachment = attached(&out.take(), 1);
        assert_eq!(fake.0.active_workers.load(Ordering::SeqCst), 1);
        let op = if delete {
            Op::DeleteSession {
                session: "s1".into(),
                abandon_children: false,
            }
        } else {
            Op::ArchiveSession {
                session: "s1".into(),
                archived: true,
            }
        };
        request(&mut connection, 2, op);
        assert!(matches!(outcome(&out.take(), 2), Outcome::Ok(_)));
        assert_eq!(fake.0.lifecycle_mutations.load(Ordering::SeqCst), 1);
        assert_eq!(fake.0.opens.load(Ordering::SeqCst), 1);
        assert_eq!(fake.0.active_workers.load(Ordering::SeqCst), 0);
        connection.leave();
        handed_off(&host);
    }
}

#[test]
fn lifecycle_busy_refusal_keeps_the_same_actor_and_never_calls_the_product() {
    for delete in [false, true] {
        let (host, fake) = host(&["s1"]);
        let out = Out::default();
        let mut connection = host.connect(out.clone(), Face::Remote, || false);
        request(&mut connection, 1, Op::Attach { node: node("s1") });
        let attachment = attached(&out.take(), 1);
        prompt(&mut connection, 2, attachment, "busy");
        let until = Instant::now() + Duration::from_secs(10);
        while row_of(&host, "s1").activity != SessionActivity::Busy {
            assert!(Instant::now() < until, "the actor never became busy");
            std::thread::yield_now();
        }
        let op = if delete {
            Op::DeleteSession {
                session: "s1".into(),
                abandon_children: false,
            }
        } else {
            Op::ArchiveSession {
                session: "s1".into(),
                archived: true,
            }
        };
        request(&mut connection, 3, op);
        assert!(matches!(
            outcome(&out.take(), 3),
            Outcome::Refused(RequestRefusal::SessionLifecycleRefused {
                reason: plakous_host_protocol::wire::LifecycleReason::Busy { turn: true, .. },
                ..
            })
        ));
        assert_eq!(fake.0.lifecycle_mutations.load(Ordering::SeqCst), 0);
        assert_eq!(fake.0.opens.load(Ordering::SeqCst), 1);
        assert_eq!(fake.0.active_workers.load(Ordering::SeqCst), 1);
        connection.leave();
        handed_off(&host);
    }
}
