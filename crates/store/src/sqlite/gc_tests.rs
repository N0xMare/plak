use super::*;
use crate::integrity_fixture as fixture;
use proptest::prelude::*;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

struct FixedClock(LeaseInstant);
impl LeaseClock for FixedClock {
    fn now(&self) -> Result<LeaseInstant, SessionStoreError> {
        Ok(self.0)
    }
}

pub(super) fn backend(path: &Path) -> TestResult<Backend> {
    let clock = Arc::new(FixedClock(SystemLeaseClock::new()?.now()?));
    Ok(Backend::open(
        path,
        clock.clone(),
        clock.now()?.domain(),
        SqliteStoreOptions::default(),
        &StartupProgress(Mutex::new(StartupStage::Header)),
    )?)
}

pub(super) fn seed(base: u128) -> TestResult<CreateSession> {
    let session = fixture::id(base)?;
    let branch = fixture::id(base + 1)?;
    let checkpoint = fixture::checkpoint(
        fixture::topology(session, branch, fixture::id(base + 2)?, None, 0, None, 1)?,
        1,
        RuntimeDurability::Required,
    )?;
    Ok(CreateSession::new(
        SessionRef::new(session, branch),
        Timestamp::new(50, 0)?,
        fixture::id(base + 3)?,
        NonZeroU64::new(3600).ok_or("lease")?,
        fixture::bundle(&checkpoint, None)?,
    ))
}

fn addresses(c: &Connection, sql: &str) -> TestResult<BTreeSet<RuntimeObjectAddress>> {
    let mut statement = c.prepare(sql)?;
    let mut rows = statement.query([])?;
    let mut result = BTreeSet::new();
    while let Some(row) = rows.next()? {
        result.insert(decode_address(row.get(0)?, row.get(1)?, row.get(2)?)?);
    }
    Ok(result)
}

fn closure(
    roots: BTreeSet<RuntimeObjectAddress>,
    graph: &BTreeMap<RuntimeObjectAddress, RuntimeObject>,
) -> TestResult<BTreeSet<RuntimeObjectAddress>> {
    let mut reached = BTreeSet::new();
    let mut queue: Vec<_> = roots.into_iter().collect();
    while let Some(address) = queue.pop() {
        if reached.insert(address) {
            queue.extend(
                graph
                    .get(&address)
                    .ok_or("oracle graph address")?
                    .children(),
            );
        }
    }
    Ok(reached)
}

fn oracle(
    b: &Backend,
    graph: &BTreeMap<RuntimeObjectAddress, RuntimeObject>,
    drained: bool,
) -> TestResult {
    validate_counts(&b.connection)?;
    let live = closure(
        addresses(
            &b.connection,
            "SELECT root_kind,root_codec,root_digest FROM checkpoints",
        )?,
        graph,
    )?;
    let actual = addresses(
        &b.connection,
        "SELECT kind,codec,digest FROM runtime_objects",
    )?;
    assert!(live.is_subset(&actual), "GC removed a reachable object");
    let queued = closure(
        addresses(&b.connection, "SELECT kind,codec,digest FROM gc_candidates")?,
        graph,
    )?;
    assert!(
        actual.is_subset(&live.union(&queued).copied().collect()),
        "an unreachable object lost its persistent queue ancestry"
    );
    if drained {
        assert_eq!(
            actual, live,
            "drain differs from independent mark/sweep oracle"
        );
    }
    let bytes: usize = actual.iter().map(|a| graph[a].retained_bytes()).sum();
    assert_eq!(read_stats(&b.connection)?.object_bytes(), u64n(bytes));
    Ok(())
}

#[test]
fn deletion_defers_objects_and_gc_steps_keep_exact_counters() -> TestResult {
    let dir = tempfile::tempdir()?;
    let mut b = backend(&dir.path().join("runtime.sqlite3"))?;
    let request = seed(120_000)?;
    let graph = request
        .initial()
        .supplied_objects()
        .iter()
        .map(|o| (o.address(), o.clone()))
        .collect();
    let created = create(&mut b, &request)?;
    release(&mut b, created.lease())?;
    let archived = archive(
        &mut b,
        ArchiveSession::new(created.session().session_id(), created.session().revision()),
    )?;
    let before = read_stats(&b.connection)?;
    let request = DeleteSession::new(
        archived.session().session_id(),
        archived.session().revision(),
    );
    let receipt = delete(&mut b, request)?;
    assert_eq!(read_stats(&b.connection)?.objects(), before.objects());
    assert!(gc::pending(&b.connection)?);
    oracle(&b, &graph, false)?;
    while gc::pending(&b.connection)? {
        let report = gc::step(&mut b.connection, 1, 1)?;
        assert!(report.objects() <= 1);
        oracle(&b, &graph, !report.pending())?;
    }
    assert_eq!(
        read_stats(&b.connection)?.reclaimed_objects(),
        before.objects()
    );
    assert_eq!(
        read_stats(&b.connection)?.reclaimed_object_bytes(),
        before.object_bytes()
    );
    assert_eq!(load_delete(&b.connection, request)?, receipt);
    Ok(())
}

pub(super) struct Branch {
    pub(super) head: BranchHead,
    pub(super) lease: BranchLease,
    pub(super) bundle: CheckpointObjectBundle,
    pub(super) value: u32,
}

fn workflow(identity: u128) -> TestResult<SessionWorkflowId> {
    let mut bytes = [0; 32];
    bytes[..16].copy_from_slice(&identity.to_be_bytes());
    bytes[16..].copy_from_slice(&identity.to_be_bytes());
    Ok(SessionWorkflowId::new(bytes)?)
}

pub(super) fn advance_branch(
    branch: &mut Branch,
    b: &mut Backend,
    graph: &mut BTreeMap<RuntimeObjectAddress, RuntimeObject>,
    identity: u128,
) -> TestResult {
    advance_branch_with_payload(branch, b, graph, identity, "")
}

/// Commits the next checkpoint, whose new history item carries `payload`.
pub(super) fn advance_branch_with_payload(
    branch: &mut Branch,
    b: &mut Backend,
    graph: &mut BTreeMap<RuntimeObjectAddress, RuntimeObject>,
    identity: u128,
    payload: &str,
) -> TestResult {
    let topology = fixture::topology(
        branch.head.session().session_id(),
        branch.head.session().branch_id(),
        fixture::id(identity + 5)?,
        Some(branch.bundle.checkpoint_id()),
        branch.head.revision().get() + 1,
        branch
            .bundle
            .validated_checkpoint()?
            .manifest()
            .topology()
            .fork,
        branch.lease.fence().get(),
    )?;
    branch.value += 1;
    let checkpoint = fixture::checkpoint_with_payload(
        topology,
        branch.value,
        RuntimeDurability::Required,
        branch
            .bundle
            .validated_checkpoint()?
            .manifest()
            .canonical()
            .context()
            .clone(),
        payload,
    )?;
    let bundle = fixture::bundle(&checkpoint, Some(&branch.bundle))?;
    graph.extend(
        bundle
            .supplied_objects()
            .iter()
            .map(|o| (o.address(), o.clone())),
    );
    branch.head = commit(
        b,
        &CommitCheckpoint::new(branch.lease.clone(), branch.head, bundle.clone()),
    )?
    .head();
    branch.bundle = bundle;
    Ok(())
}

pub(super) fn fork_branch(
    source: &Branch,
    b: &mut Backend,
    graph: &mut BTreeMap<RuntimeObjectAddress, RuntimeObject>,
    identity: u128,
) -> TestResult<Branch> {
    let selector = source.head.session();
    let target_branch = fixture::id(identity + 6)?;
    let lineage = ForkLineage {
        session_id: selector.session_id(),
        branch_id: selector.branch_id(),
        checkpoint_id: source.bundle.checkpoint_id(),
        branch_revision: source.head.revision().get(),
    };
    let context = source
        .bundle
        .validated_checkpoint()?
        .manifest()
        .canonical()
        .context()
        .ordinary_fork(
            source.bundle.checkpoint_id(),
            selector.session_id(),
            target_branch,
            [0x64; 32],
        )?;
    let checkpoint = fixture::checkpoint_with_context(
        fixture::topology(
            selector.session_id(),
            target_branch,
            fixture::id(identity + 7)?,
            Some(source.bundle.checkpoint_id()),
            0,
            Some(lineage),
            1,
        )?,
        source.value,
        RuntimeDurability::Required,
        context,
    )?;
    let bundle = fixture::bundle(&checkpoint, Some(&source.bundle))?;
    graph.extend(
        bundle
            .supplied_objects()
            .iter()
            .map(|o| (o.address(), o.clone())),
    );
    let revision = session_head(&b.connection, selector.session_id())?
        .ok_or("session")?
        .revision();
    let semantic = Digest::sha256(DigestDomain::RequestPlan, 1, &[b"oracle fork"])?;
    let request = ForkBranch::new(
        workflow(identity)?,
        semantic,
        source.lease.clone(),
        revision,
        source.head,
        target_branch,
        fixture::id(identity + 9)?,
        NonZeroU64::new(3600).ok_or("lease")?,
        bundle.clone(),
    );
    let receipt = fork(b, &request)?;
    Ok(Branch {
        head: receipt.target(),
        lease: receipt.lease().clone(),
        bundle,
        value: source.value,
    })
}

fn interleavings(operations: &[(u8, u8)]) -> TestResult {
    let dir = tempfile::tempdir()?;
    let mut b = backend(&dir.path().join("runtime.sqlite3"))?;
    let mut graph = BTreeMap::new();
    let mut branches = Vec::<Branch>::new();
    let mut identity = 200_000_u128;
    for &(operation, selection) in operations {
        identity += 100;
        if branches.is_empty() {
            let request = seed(identity)?;
            graph.extend(
                request
                    .initial()
                    .supplied_objects()
                    .iter()
                    .map(|o| (o.address(), o.clone())),
            );
            let created = create(&mut b, &request)?;
            branches.push(Branch {
                head: created.branch(),
                lease: created.lease().clone(),
                bundle: request.initial().clone(),
                value: 1,
            });
        }
        let index = usize::from(selection) % branches.len();
        match operation % 5 {
            0 => advance_branch(&mut branches[index], &mut b, &mut graph, identity)?,
            1 if branches.len() < 4 => {
                let target = fork_branch(&branches[index], &mut b, &mut graph, identity)?;
                branches.push(target);
            }
            2 => {
                let branch = &branches[index];
                prune(
                    &mut b,
                    &PruneBranch::new(branch.lease.clone(), branch.head, BTreeSet::new(), 1),
                )?;
            }
            3 => {
                for branch in &branches {
                    release(&mut b, &branch.lease)?;
                }
                let session = session_head(&b.connection, branches[0].head.session().session_id())?
                    .ok_or("session")?;
                let archived = archive(
                    &mut b,
                    ArchiveSession::new(session.session_id(), session.revision()),
                )?;
                delete(
                    &mut b,
                    DeleteSession::new(session.session_id(), archived.session().revision()),
                )?;
                branches.clear();
            }
            _ => {
                gc::step(&mut b.connection, usize::from(selection % 8) + 1, 4096)?;
            }
        }
        oracle(&b, &graph, false)?;
    }
    b.reclaim(StoreReclaimMode::Drain)?;
    oracle(&b, &graph, true)?;
    verify_database(&b.connection)?;
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]
    #[test]
    fn gc_interleavings_match_independent_mark_sweep(operations in prop::collection::vec((0_u8..5, any::<u8>()), 1..40)) {
        interleavings(&operations).map_err(|error| TestCaseError::fail(error.to_string()))?;
    }
}

#[cfg(unix)]
#[test]
fn gc_crash_child() -> TestResult {
    let Some(path) = std::env::var_os("PLAKOUS_SQLITE_TEST_GC_DB") else {
        return Ok(());
    };
    let mut b = backend(Path::new(&path))?;
    let created = create(&mut b, &seed(120_000)?)?;
    release(&mut b, created.lease())?;
    let archived = archive(
        &mut b,
        ArchiveSession::new(created.session().session_id(), created.session().revision()),
    )?;
    delete(
        &mut b,
        DeleteSession::new(
            archived.session().session_id(),
            archived.session().revision(),
        ),
    )?;
    gc::step(&mut b.connection, 256, 8 * 1024 * 1024)?;
    Err("GC crash cut did not park the child".into())
}

#[cfg(unix)]
#[test]
fn sigkill_gc_cuts_preserve_queue_counters_and_oracle() -> TestResult {
    use std::os::unix::process::ExitStatusExt as _;
    use std::process::{Command, Stdio};
    for (stage, committed) in [("gc-before-commit", false), ("gc-after-commit", true)] {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("runtime.sqlite3");
        let marker = dir.path().join("gc.reached");
        let child = Command::new(std::env::current_exe()?)
            .args(["--exact", "sqlite::gc_tests::gc_crash_child", "--nocapture"])
            .env("PLAKOUS_SQLITE_TEST_GC_DB", &path)
            .env(CRASH_CUT_STAGE_ENV, stage)
            .env(CRASH_CUT_MARKER_ENV, &marker)
            .env("RUST_TEST_THREADS", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let mut child = tests::KillOnDrop::new(child);
        tests::wait_for_marker(child.child(), &marker)?;
        assert_eq!(child.sigkill()?.signal(), Some(9));
        let request = seed(120_000)?;
        let graph: BTreeMap<_, _> = request
            .initial()
            .supplied_objects()
            .iter()
            .map(|o| (o.address(), o.clone()))
            .collect();
        let mut reopened = backend(&path)?;
        verify_database(&reopened.connection)?;
        let stats = read_stats(&reopened.connection)?;
        assert_eq!(
            stats.objects(),
            if committed { 0 } else { u64n(graph.len()) }
        );
        assert_eq!(gc::pending(&reopened.connection)?, !committed);
        oracle(&reopened, &graph, committed)?;
        reopened.reclaim(StoreReclaimMode::Drain)?;
        oracle(&reopened, &graph, true)?;
        verify_database(&reopened.connection)?;
    }
    Ok(())
}

#[test]
fn full_verification_checks_payload_lengths() -> TestResult {
    let dir = tempfile::tempdir()?;
    let mut b = backend(&dir.path().join("runtime.sqlite3"))?;
    create(&mut b, &seed(120_000)?)?;
    b.connection.execute("UPDATE runtime_object_bytes SET bytes=CAST(bytes||x'00' AS BLOB) WHERE id=(SELECT min(id) FROM runtime_objects)", [])?;
    assert_eq!(
        verify_database(&b.connection),
        Err(SessionStoreError::corrupt())
    );
    Ok(())
}

#[test]
fn capacity_retry_reclaims_one_batch_before_refusing() -> TestResult {
    let dir = tempfile::tempdir()?;
    let mut b = backend(&dir.path().join("runtime.sqlite3"))?;
    for base in [120_000, 130_000, 140_000] {
        let created = create(&mut b, &seed(base)?)?;
        release(&mut b, created.lease())?;
        let archived = archive(
            &mut b,
            ArchiveSession::new(created.session().session_id(), created.session().revision()),
        )?;
        delete(
            &mut b,
            DeleteSession::new(
                archived.session().session_id(),
                archived.session().revision(),
            ),
        )?;
    }
    let request = seed(150_000)?;
    let objects = request.initial().supplied_objects().len();
    b.limits = MemoryStoreLimits::new(
        nonzero(1024),
        nonzero(1024),
        nonzero(524_288),
        nonzero((objects * 8).div_ceil(7) + 3),
        nonzero(4 * 1024 * 1024 * 1024),
        nonzero(256),
    );
    assert!(matches!(
        create(&mut b, &request),
        Err(SessionStoreError::Capacity(_))
    ));
    let created = b.retry_after_reclaim(|b| create(b, &request))?;
    assert_eq!(created.branch().session(), request.session());
    assert!(read_stats(&b.connection)?.reclaimed_objects() > 0);
    assert!(
        usage(&b.connection, b.limits)?
            .ordinary_exhausted()
            .is_none()
    );
    Ok(())
}
