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
    let request = fork_request(source, b, identity)?;
    let bundle = request.target().clone();
    graph.extend(
        bundle
            .supplied_objects()
            .iter()
            .map(|o| (o.address(), o.clone())),
    );
    let receipt = fork(b, &request)?;
    Ok(Branch {
        head: receipt.target(),
        lease: receipt.lease().clone(),
        bundle,
        value: source.value,
    })
}

fn fork_request(source: &Branch, b: &Backend, identity: u128) -> TestResult<ForkBranch> {
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
    let revision = session_head(&b.connection, selector.session_id())?
        .ok_or("session")?
        .revision();
    let semantic = Digest::sha256(DigestDomain::RequestPlan, 1, &[b"oracle fork"])?;
    Ok(ForkBranch::new(
        workflow(identity)?,
        semantic,
        source.lease.clone(),
        revision,
        source.head,
        target_branch,
        fixture::id(identity + 9)?,
        NonZeroU64::new(3600).ok_or("lease")?,
        bundle,
    ))
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

#[test]
fn shrinking_successor_charges_only_its_closure_across_sessions() -> TestResult {
    let directory = tempfile::tempdir()?;
    let mut b = backend(&directory.path().join("runtime.sqlite3"))?;
    let root_request = seed(810_000)?;
    let root = create(&mut b, &root_request)?;
    let child_request = seed(820_000)?;
    let child = create(&mut b, &child_request)?;
    let mut branch = Branch {
        head: child.branch(),
        lease: child.lease().clone(),
        bundle: child_request.initial().clone(),
        value: 1,
    };
    let mut graph = BTreeMap::new();
    advance_branch_with_payload(
        &mut branch,
        &mut b,
        &mut graph,
        830_000,
        &"x".repeat(100_000),
    )?;
    let ancestral_bytes = branch.bundle.stats().retained_bytes();
    b.checkpoints.clear();
    let warm_root = b.load_branch(root.branch().session())?;
    let root_weight = checkpoint_cache::weight(warm_root.checkpoint());
    // Reset history to one small item, analogous to a compaction fold.
    branch.value = 0;
    advance_branch(&mut branch, &mut b, &mut graph, 840_000)?;
    let warm_child = b.load_branch(branch.head.session())?;
    assert!(warm_child.checkpoint().bundle().stats().retained_bytes() < ancestral_bytes / 2);
    let child_weight = checkpoint_cache::weight(warm_child.checkpoint());
    b.checkpoints = CheckpointCache::new(root_weight + child_weight * 3);
    b.load_branch(root.branch().session())?;
    for identity in [850_000, 860_000, 870_000, 880_000] {
        // Keep the warm root newer than the prior child's head.
        let before = b.checkpoint_decodes;
        b.load_branch(root.branch().session())?;
        assert_eq!(b.checkpoint_decodes, before);
        branch.value = 0;
        advance_branch(&mut branch, &mut b, &mut graph, identity)?;
        let before = b.checkpoint_decodes;
        let loaded_root = b.load_branch(root.branch().session())?;
        assert_eq!(loaded_root.checkpoint(), warm_root.checkpoint());
        assert_eq!(
            b.checkpoint_decodes, before,
            "child publication evicted the warm root"
        );
    }
    let warm = b.load_branch(branch.head.session())?;
    b.checkpoints.clear();
    let cold = b.load_branch(branch.head.session())?;
    assert_eq!(warm.checkpoint().bundle(), cold.checkpoint().bundle());
    Ok(())
}

#[test]
fn fresh_commits_replace_pending_seeds_without_closure_work() -> TestResult {
    let directory = tempfile::tempdir()?;
    let mut b = backend(&directory.path().join("runtime.sqlite3"))?;
    let request = seed(910_000)?;
    let created = create(&mut b, &request)?;
    let mut branch = Branch {
        head: created.branch(),
        lease: created.lease().clone(),
        bundle: request.initial().clone(),
        value: 1,
    };
    let mut graph = BTreeMap::new();
    for identity in [920_000, 930_000, 940_000] {
        let previous = branch.head.checkpoint().checkpoint_id();
        advance_branch(&mut branch, &mut b, &mut graph, identity)?;
        assert!(b.checkpoints.has_pending(branch.head.session()));
        assert!(b.checkpoints.get(previous).is_none());
        assert_eq!(b.checkpoints.normalizations, 0, "commit walked the closure");
        assert_eq!(b.checkpoint_decodes, 0);
    }
    b.load_branch(branch.head.session())?;
    assert_eq!(b.checkpoints.normalizations, 1);
    b.load_branch(branch.head.session())?;
    assert_eq!(b.checkpoints.normalizations, 1);
    Ok(())
}

fn cache_branch(b: &mut Backend, identity: u128) -> TestResult<Branch> {
    let request = seed(identity)?;
    let created = create(b, &request)?;
    Ok(Branch {
        head: created.branch(),
        lease: created.lease().clone(),
        bundle: request.initial().clone(),
        value: 1,
    })
}

fn successor_request(branch: &Branch, identity: u128) -> TestResult<CommitCheckpoint> {
    let checkpoint = fixture::checkpoint(
        fixture::topology(
            branch.head.session().session_id(),
            branch.head.session().branch_id(),
            fixture::id(identity)?,
            Some(branch.bundle.checkpoint_id()),
            branch.head.revision().get() + 1,
            None,
            branch.lease.fence().get(),
        )?,
        branch.value + 1,
        RuntimeDurability::Required,
    )?;
    Ok(CommitCheckpoint::new(
        branch.lease.clone(),
        branch.head,
        fixture::bundle(&checkpoint, Some(&branch.bundle))?,
    ))
}

fn assert_no_seed(b: &Backend, branch: SessionRef) {
    assert!(!b.checkpoints.has_pending(branch));
    assert_eq!(b.checkpoints.normalizations, 0);
}

#[test]
fn background_reclaim_preserves_pending_seed() -> TestResult {
    let directory = tempfile::tempdir()?;
    let mut b = backend(&directory.path().join("runtime.sqlite3"))?;
    let source = cache_branch(&mut b, 970_000)?;
    let receipt = commit(&mut b, &successor_request(&source, 980_000)?)?;
    b.gc_pending = true;
    assert!(b.background_step());
    assert!(b.checkpoints.has_pending(source.head.session()));
    let before = b.checkpoint_decodes;
    assert_eq!(b.load_branch(source.head.session())?.head(), receipt.head());
    assert_eq!(b.checkpoint_decodes, before);
    Ok(())
}

#[test]
fn failed_normalization_drops_seed_and_cold_load_inserts_ready() -> TestResult {
    let directory = tempfile::tempdir()?;
    let mut b = backend(&directory.path().join("runtime.sqlite3"))?;
    let source = cache_branch(&mut b, 990_000)?;
    let receipt = commit(&mut b, &successor_request(&source, 1_000_000)?)?;
    b.checkpoints.fail_normalization = true;
    assert!(
        b.checkpoints
            .get(receipt.head().checkpoint().checkpoint_id())
            .is_none()
    );
    assert!(!b.checkpoints.has_pending(source.head.session()));
    assert_eq!(b.checkpoints.normalizations, 1);
    assert!(
        b.checkpoints
            .get(receipt.head().checkpoint().checkpoint_id())
            .is_none()
    );
    let before = b.checkpoint_decodes;
    b.load_branch(source.head.session())?;
    assert_eq!(b.checkpoint_decodes, before + 1);
    b.load_branch(source.head.session())?;
    assert_eq!(b.checkpoint_decodes, before + 1);
    assert_eq!(b.checkpoints.normalizations, 1);
    Ok(())
}

#[test]
fn commit_replays_and_refusals_do_not_seed() -> TestResult {
    let directory = tempfile::tempdir()?;
    let mut b = backend(&directory.path().join("runtime.sqlite3"))?;
    let source = cache_branch(&mut b, 1_010_000)?;
    let request = successor_request(&source, 1_020_000)?;
    let receipt = commit(&mut b, &request)?;
    b.checkpoints.clear();
    assert_eq!(commit(&mut b, &request)?, receipt);
    assert_no_seed(&b, source.head.session());
    let collision = CommitCheckpoint::new(
        source.lease.clone(),
        receipt.head(),
        request.checkpoint().clone(),
    );
    assert_eq!(
        commit(&mut b, &collision),
        Err(SessionStoreError::IdentityConflict)
    );
    assert_no_seed(&b, source.head.session());
    assert_eq!(
        commit(&mut b, &successor_request(&source, 1_030_000)?),
        Err(SessionStoreError::Conflict)
    );
    assert_no_seed(&b, source.head.session());
    release(&mut b, &source.lease)?;
    assert_eq!(
        commit(&mut b, &successor_request(&source, 1_040_000)?),
        Err(SessionStoreError::FenceSuperseded)
    );
    assert_no_seed(&b, source.head.session());
    Ok(())
}

#[test]
fn fork_replays_and_refusals_do_not_seed() -> TestResult {
    let directory = tempfile::tempdir()?;
    let mut b = backend(&directory.path().join("runtime.sqlite3"))?;
    let source = cache_branch(&mut b, 1_050_000)?;
    let request = fork_request(&source, &b, 1_060_000)?;
    let receipt = fork(&mut b, &request)?;
    b.checkpoints.clear();
    let target = receipt.target().session();
    assert_eq!(fork(&mut b, &request)?, receipt);
    assert_no_seed(&b, target);
    let conflict = fork_request(&source, &b, 1_070_000)?;
    fork(&mut b, &conflict)?;
    b.checkpoints.clear();
    // Reusing a workflow with the newer session revision changes its exact request.
    let collision = fork_request(&source, &b, 1_060_000)?;
    assert_eq!(
        fork(&mut b, &collision),
        Err(SessionStoreError::IdentityConflict)
    );
    assert_no_seed(&b, target);
    let stale_revision = fork_request(&source, &b, 1_080_000)?;
    let intervening = fork_request(&source, &b, 1_090_000)?;
    fork(&mut b, &intervening)?;
    b.checkpoints.clear();
    assert_eq!(
        fork(&mut b, &stale_revision),
        Err(SessionStoreError::Conflict)
    );
    assert_no_seed(
        &b,
        SessionRef::new(
            source.head.session().session_id(),
            stale_revision.target_branch(),
        ),
    );
    let stale_fence = fork_request(&source, &b, 1_100_000)?;
    release(&mut b, &source.lease)?;
    assert_eq!(
        fork(&mut b, &stale_fence),
        Err(SessionStoreError::FenceSuperseded)
    );
    assert_no_seed(
        &b,
        SessionRef::new(
            source.head.session().session_id(),
            stale_fence.target_branch(),
        ),
    );
    Ok(())
}

#[test]
fn fork_target_pending_seed_survives_source_commit() -> TestResult {
    let directory = tempfile::tempdir()?;
    let mut b = backend(&directory.path().join("runtime.sqlite3"))?;
    let source = cache_branch(&mut b, 1_110_000)?;
    let request = fork_request(&source, &b, 1_120_000)?;
    let target = fork(&mut b, &request)?.target();
    commit(&mut b, &successor_request(&source, 1_130_000)?)?;
    assert!(b.checkpoints.has_pending(target.session()));
    assert!(b.checkpoints.has_pending(source.head.session()));
    let before = b.checkpoint_decodes;
    assert_eq!(b.load_branch(target.session())?.head(), target);
    assert_eq!(b.checkpoint_decodes, before);
    assert!(!b.checkpoints.has_pending(target.session()));
    assert!(b.checkpoints.has_pending(source.head.session()));
    Ok(())
}

#[test]
fn capacity_retry_publications_preserve_other_sessions_ready_roots() -> TestResult {
    for forking in [false, true] {
        let directory = tempfile::tempdir()?;
        let mut b = backend(&directory.path().join("runtime.sqlite3"))?;
        let warm = cache_branch(&mut b, 1_140_000)?;
        b.load_branch(warm.head.session())?;
        let source = cache_branch(&mut b, 1_150_000)?;
        let commit_request = successor_request(&source, 1_160_000)?;
        let fork_request = fork_request(&source, &b, 1_170_000)?;
        let target = if forking {
            SessionRef::new(
                source.head.session().session_id(),
                fork_request.target_branch(),
            )
        } else {
            source.head.session()
        };
        let limits = b.limits;
        b.limits = MemoryStoreLimits::new(
            nonzero(1024),
            nonzero(1024),
            nonzero(2),
            nonzero(4_194_304),
            nonzero(4 * 1024 * 1024 * 1024),
            nonzero(256),
        );
        let mut attempts = 0;
        b.retry_after_reclaim(|b| {
            attempts += 1;
            let result = if forking {
                fork(b, &fork_request).map(|_| ())
            } else {
                commit(b, &commit_request).map(|_| ())
            };
            if attempts == 1 {
                assert!(matches!(result, Err(SessionStoreError::Capacity(_))));
                assert_no_seed(b, target);
                b.limits = limits;
            }
            result
        })?;
        assert_eq!(attempts, 2);
        assert!(b.checkpoints.has_pending(target));
        let before = b.checkpoint_decodes;
        b.load_branch(warm.head.session())?;
        assert_eq!(b.checkpoint_decodes, before);
    }
    Ok(())
}
