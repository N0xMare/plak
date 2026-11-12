use super::*;

#[test]
fn operator_host_hold_requires_an_existing_exact_resident_without_opening() {
    let (host, fake) = host(&["s1", "s2"]);
    let slot = host.slot("s1");
    assert!(slot.hold_resident("s1").is_none());
    assert_eq!(fake.0.opens.load(Ordering::SeqCst), 0);
    let out = Out::default();
    let mut connection = host.connect(
        out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
    request(&mut connection, 1, Op::Attach { node: node("s1") });
    attached(&out.take(), 1);
    assert!(slot.hold_resident("s2").is_none());
    let hold = slot.hold_resident("s1").unwrap();
    assert_eq!(fake.0.opens.load(Ordering::SeqCst), 1);
    assert!(host.slot("s2").hold_resident("s2").is_none());
    assert_eq!(fake.0.opens.load(Ordering::SeqCst), 1);
    drop(hold);
    connection.leave();
    handed_off(&host);
}

#[test]
fn a_same_node_switch_reopens_one_dormant_successor_slot_from_a_fresh_client() {
    let Switched {
        host,
        fake,
        slot,
        mut connection,
        ..
    } = switched_on_node(true);
    let SlotView::Resident(served) = slot.peek() else {
        panic!("not resident")
    };
    assert_eq!(served.root(), node("s1"));
    connection.leave();
    let out = Out::default();
    let mut fresh = host.connect(
        out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
    release_session(&mut fresh, 1, "s9");
    assert_eq!(outcome(&out.take(), 1), Outcome::Ok(Answer::Released));
    release_session(&mut fresh, 2, "s1");
    assert_eq!(outcome(&out.take(), 2), Outcome::Ok(Answer::Released));
    assert!(matches!(slot.peek(), SlotView::Dormant(id) if id == "s9"));
    assert_eq!(host.slots().len(), 1);
    request(&mut fresh, 3, Op::Attach { node: node("s1") });
    attached(&out.take(), 3);
    assert_eq!(host.slots().len(), 1);
    assert_eq!(fake.0.opens.load(Ordering::SeqCst), 2);
    assert_eq!(fake.0.active_workers.load(Ordering::SeqCst), 1);
    assert_eq!(row_of(&host, "s9").residency, SessionResidency::Resident);
    assert_eq!(row_of(&host, "s1").residency, SessionResidency::Dormant);
    fresh.leave();
    handed_off(&host);
}

#[test]
fn same_node_switch_stop_and_archive_use_the_addressed_record_not_a_cached_label() {
    for session in ["s1", "s9"] {
        let Switched {
            host,
            fake,
            slot,
            mut connection,
            out,
        } = switched_on_node(true);
        request(
            &mut connection,
            2,
            Op::StopSession {
                session: session.into(),
            },
        );
        assert_eq!(
            answer(&mut connection, &out, 2),
            Outcome::Ok(Answer::Stopped {
                runner_pending: false
            })
        );
        assert_eq!(
            fake.0.stop_intents.load(Ordering::SeqCst),
            usize::from(session == "s9")
        );
        assert_eq!(
            fake.0.dormant_stops.load(Ordering::SeqCst),
            usize::from(session == "s1")
        );
        request(
            &mut connection,
            3,
            Op::ArchiveSession {
                session: session.into(),
                archived: true,
            },
        );
        assert_eq!(
            outcome(&out.take(), 3),
            Outcome::Ok(Answer::ArchiveUpdated {
                session: session.into(),
                archived: true
            })
        );
        assert_eq!(fake.0.opens.load(Ordering::SeqCst), 1);
        if session == "s1" {
            assert!(
                matches!(slot.peek(), SlotView::Resident(served) if served.session_id() == "s9")
            );
            // The historical record has its own dormant metadata slot, not another line owner.
            assert_eq!(host.slots().len(), 2);
            request(
                &mut connection,
                4,
                Op::DeleteSession {
                    session: session.into(),
                    abandon_children: false,
                },
            );
            assert_eq!(
                outcome(&out.take(), 4),
                Outcome::Ok(Answer::Deleted {
                    session: session.into()
                })
            );
        } else {
            assert!(matches!(slot.peek(), SlotView::Dormant(id) if id == "s9"));
            assert_eq!(fake.0.active_workers.load(Ordering::SeqCst), 0);
        }
        assert_eq!(host.slots().len(), 1);
        connection.leave();
        handed_off(&host);
    }
}

#[test]
fn a_same_node_attach_waits_for_the_successors_release_instead_of_opening_another_slot() {
    let Switched {
        host,
        fake,
        slot,
        mut connection,
        ..
    } = switched_on_node(true);
    fake.0.linger.store(150, Ordering::SeqCst);
    let SlotView::Resident(served) = slot.peek() else {
        panic!("not resident")
    };
    connection.leave();
    served.request_release();
    let until = Instant::now() + Duration::from_secs(3);
    while !matches!(slot.peek(), SlotView::Releasing(_)) {
        assert!(
            Instant::now() < until,
            "the successor did not begin releasing"
        );
        std::thread::yield_now();
    }
    let out = Out::default();
    let mut fresh = host.connect(
        out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
    request(&mut fresh, 1, Op::Attach { node: node("s1") });
    attached(&out.take(), 1);
    assert_eq!(host.slots().len(), 1);
    assert_eq!(fake.0.opens.load(Ordering::SeqCst), 2);
    assert_eq!(fake.0.active_workers.load(Ordering::SeqCst), 1);
    assert_eq!(row_of(&host, "s9").residency, SessionResidency::Resident);
    fresh.leave();
    handed_off(&host);
}

#[test]
fn concurrent_same_node_attaches_share_the_opening_successor_slot() {
    let Switched {
        host,
        fake,
        slot,
        mut connection,
        ..
    } = switched_on_node(true);
    connection.leave();
    slot.quit(Duration::from_secs(3)).unwrap();
    let (open, gate) = std::sync::mpsc::channel();
    *fake.0.gate.lock().unwrap() = Some(gate);
    let first_host = Arc::clone(&host);
    let first = std::thread::spawn(move || {
        let out = Out::default();
        let mut fresh = first_host.connect(
            out.clone(),
            Face::Remote,
            HostPrincipal::Socket(501),
            || false,
        );
        request(&mut fresh, 1, Op::Attach { node: node("s1") });
        attached(&out.take(), 1);
        fresh
    });
    let until = Instant::now() + Duration::from_secs(3);
    while !matches!(slot.peek(), SlotView::Opening(id) if id == "s9") {
        assert!(
            Instant::now() < until,
            "the successor did not begin opening"
        );
        std::thread::yield_now();
    }
    let second_host = Arc::clone(&host);
    let second = std::thread::spawn(move || second_host.resolve(&node("s1")).unwrap());
    open.send(()).unwrap();
    let mut first = first.join().unwrap();
    assert!(Arc::ptr_eq(&second.join().unwrap(), &slot));
    assert_eq!(host.slots().len(), 1);
    assert_eq!(fake.0.opens.load(Ordering::SeqCst), 2);
    first.leave();
    handed_off(&host);
}

#[test]
fn lifecycle_on_a_fork_source_keeps_the_forks_slot_serving() {
    let Switched {
        host,
        fake,
        slot,
        mut connection,
        out,
    } = switched();
    assert!(!Arc::ptr_eq(&host.slot("s1"), &slot));
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
fn lifecycle_parks_a_fork_under_its_served_id_not_its_sources_label() {
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
    assert_eq!(slot.known(), "s9");
    assert!(matches!(slot.peek(),SlotView::Dormant(id) if id=="s9"));
    connection.leave();
    handed_off(&host);
}

#[test]
fn lifecycle_unknown_ids_do_not_allocate_slots_and_delete_drops_its_slot() {
    let (host, _) = host(&["s1"]);
    let out = Out::default();
    let mut connection = host.connect(
        out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
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
    let mut connection = host.connect(
        out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
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
    let mut connection = host.connect(
        out.clone(),
        Face::Remote,
        HostPrincipal::Socket(501),
        || false,
    );
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
        let mut connection = host.connect(
            out.clone(),
            Face::Remote,
            HostPrincipal::Socket(501),
            || false,
        );
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
        let mut connection = host.connect(
            out.clone(),
            Face::Remote,
            HostPrincipal::Socket(501),
            || false,
        );
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
