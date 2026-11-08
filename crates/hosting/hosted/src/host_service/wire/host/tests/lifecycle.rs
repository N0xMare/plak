use super::*;

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
