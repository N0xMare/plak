use super::{
    AcquireBranch, BranchId, BranchLease, CheckpointId, Command, CommitCheckpoint, Connection,
    CrashCut, FeedPage, FeedSeq, KillOnDrop, NonZeroU64, NonZeroUsize, Path, PathBuf, RunId,
    RuntimeCheckpoint, SessionRef, SessionStore, SessionStoreError, SqliteSessionStore, Stdio,
    TestResult, fixture, fs, wait_for_marker,
};
use plakous_agent::internal::{
    ForkBranch, ForkLineage, LoadedBranch, PREVIOUS_RUNTIME_FORMAT_EPOCH, RUNTIME_FORMAT_EPOCH,
    RuntimeCanonicalState, RuntimeCheckpointCodec, RuntimeManifest, RuntimeTopology,
    SessionWorkflowId,
};
use plakous_model::{Digest, DigestDomain, LineId};
use std::os::unix::{fs::PermissionsExt as _, process::ExitStatusExt as _};

const LEGACY: &[u8] = include_bytes!("../../../tests/fixtures/epoch-38/runtime.sqlite3");
const SESSION: &str = "14a66aa6-de1f-82ff-a328-043f50c93274";
const BRANCH: &str = "7ea84d74-97c5-8931-a807-61312f7f1034";
const CHECKPOINT: &str = "57ba0708-c3d9-8559-b58d-0d95bf918491";
const CRASH_TEST: &str = "sqlite::tests::epochs::epoch_crash_worker";
const MODE: &str = "PLAKOUS_TEST_EPOCH_CRASH_MODE";

fn legacy(directory: &Path) -> TestResult<PathBuf> {
    let path = directory.join("runtime.sqlite3");
    fs::write(&path, LEGACY)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    Ok(path)
}

fn selector() -> TestResult<SessionRef> {
    Ok(SessionRef::new(SESSION.parse()?, BRANCH.parse()?))
}

fn old_receipt(path: &Path) -> TestResult<Vec<u8>> {
    let c = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    Ok(c.query_row(
        "SELECT receipt_integrity FROM create_receipts WHERE session_id=?1",
        [SESSION],
        |row| row.get(0),
    )?)
}

fn feed(path: &Path) -> TestResult<FeedPage> {
    Ok(SqliteSessionStore::inspect_feed(
        path,
        selector()?.session_id(),
        FeedSeq::ZERO,
        NonZeroUsize::new(32).ok_or("page bound")?,
    )?)
}

async fn lease(store: &SqliteSessionStore) -> TestResult<BranchLease> {
    Ok(store
        .acquire(AcquireBranch::new(
            selector()?,
            fixture::id::<RunId>(86_000)?,
            NonZeroU64::new(600).ok_or("lease bound")?,
        ))
        .await?)
}

fn successor(
    prior: &LoadedBranch,
    topology: RuntimeTopology,
    canonical: RuntimeCanonicalState,
) -> TestResult<RuntimeCheckpoint> {
    let source = prior.checkpoint().checkpoint().manifest();
    Ok(RuntimeCheckpoint::new(RuntimeManifest::new(
        source.provenance().clone(),
        topology,
        source.lifecycle().clone(),
        source.harness_plan_component().clone(),
        source.provider_configuration().clone(),
        canonical,
        source.binding().clone(),
        source.tools().clone(),
        source.ledger().clone(),
        None,
        source.interpreter_observation().cloned(),
        source.jobs().clone(),
        source.named_state().clone(),
        source.durability(),
    )?)?)
}

fn advancing_checkpoint_id() -> TestResult<CheckpointId> {
    fixture::id(86_001)
}

fn commit_request(old: &LoadedBranch, lease: BranchLease) -> TestResult<CommitCheckpoint> {
    commit_to(old, lease, advancing_checkpoint_id()?)
}

fn commit_to(
    old: &LoadedBranch,
    lease: BranchLease,
    checkpoint_id: CheckpointId,
) -> TestResult<CommitCheckpoint> {
    let before = old.checkpoint().checkpoint().manifest();
    let mut topology = before.topology();
    topology.parent_checkpoint_id = Some(topology.checkpoint_id);
    topology.checkpoint_id = checkpoint_id;
    topology.branch_revision += 1;
    topology.lease_fence = lease.fence().get();
    topology.commit_identity = Digest::sha256(
        DigestDomain::ProviderOperation,
        1,
        &[b"epoch fixture commit"],
    )?;
    let checkpoint = successor(old, topology, before.canonical().clone())?;
    let bundle = RuntimeCheckpointCodec::advance(&checkpoint, old.checkpoint().bundle())?;
    assert_eq!(bundle.base_root(), Some(old.head().checkpoint().root()));
    Ok(CommitCheckpoint::new(lease, old.head(), bundle))
}

#[tokio::test]
async fn preceding_epoch_fork_has_a_current_closure_and_unchanged_source_receipt() -> TestResult {
    let dir = tempfile::tempdir()?;
    let path = legacy(dir.path())?;
    let original_seal = old_receipt(&path)?;
    SqliteSessionStore::bootstrap_stopped(&path, |_| {})?;
    let store = SqliteSessionStore::open(&path)?;
    let lease = lease(&store).await?;
    let old = store.load_branch(selector()?, "reopen").await?;
    let source = old.checkpoint().checkpoint().manifest();
    let origin = source.topology();
    let target_branch = fixture::id::<BranchId>(86_003)?;
    let checkpoint_id = fixture::id::<CheckpointId>(86_004)?;
    let context = source.canonical().context().ordinary_fork(
        origin.checkpoint_id,
        origin.session_id,
        target_branch,
        [3; 32],
    )?;
    let canonical = RuntimeCanonicalState::new(
        source.canonical().stable_prefix().clone(),
        source.canonical().history().clone(),
        source.canonical().working().clone(),
        context,
    )?;
    let checkpoint = successor(
        &old,
        RuntimeTopology {
            branch_id: target_branch,
            checkpoint_id,
            parent_checkpoint_id: Some(origin.checkpoint_id),
            branch_revision: 0,
            lease_fence: 1,
            line: LineId::started_by(target_branch),
            line_generation: 1,
            fork: Some(ForkLineage {
                session_id: origin.session_id,
                branch_id: origin.branch_id,
                checkpoint_id: origin.checkpoint_id,
                branch_revision: origin.branch_revision,
            }),
            ..origin
        },
        canonical,
    )?;
    let bundle = RuntimeCheckpointCodec::advance(&checkpoint, old.checkpoint().bundle())?;
    let workflow = SessionWorkflowId::new([86; 32])?;
    let identity = Digest::sha256(DigestDomain::RequestPlan, 1, &[b"epoch fixture fork"])?;
    let request = ForkBranch::new(
        workflow,
        identity,
        lease.clone(),
        old.session().revision(),
        old.head(),
        target_branch,
        lease.owner(),
        lease.lease_seconds(),
        bundle,
    );
    let receipt = store.fork(request.clone()).await?;
    drop(store);
    let reopened = SqliteSessionStore::open_verified(&path)?;
    assert_eq!(reopened.fork(request).await?, receipt);
    assert_eq!(
        receipt.source().checkpoint().root().codec_revision(),
        PREVIOUS_RUNTIME_FORMAT_EPOCH
    );
    assert_eq!(
        receipt.target().checkpoint().root().codec_revision(),
        RUNTIME_FORMAT_EPOCH
    );
    assert_eq!(old_receipt(&path)?, original_seal);
    assert_eq!(
        reopened
            .load_checkpoint(checkpoint_id)
            .await?
            .checkpoint()
            .as_ref(),
        &checkpoint
    );
    Ok(())
}

#[tokio::test]
async fn epoch_crash_worker() -> TestResult {
    let Some(path) = std::env::var_os(super::CRASH_DB_ENV).map(PathBuf::from) else {
        return Ok(());
    };
    let mode = std::env::var(MODE)?;
    if mode == "bootstrap" {
        let cut = std::env::var(super::CRASH_CUT_STAGE_ENV)?;
        let marker = std::env::var_os(super::CRASH_CUT_MARKER_ENV).ok_or("marker")?;
        SqliteSessionStore::bootstrap_stopped(&path, |point| {
            if point == cut {
                fs::write(&marker, point).expect("mark bootstrap cut");
                loop {
                    std::thread::park();
                }
            }
        })?;
    } else {
        SqliteSessionStore::bootstrap_stopped(&path, |_| {})?;
        let store = SqliteSessionStore::open(&path)?;
        let lease = lease(&store).await?;
        let prior = store.load_branch(selector()?, "reopen").await?;
        store.commit(commit_request(&prior, lease)?).await?;
    }
    Err("configured crash cut did not stop the child".into())
}

fn kill_at(path: &Path, mode: &str, cut: &str) -> TestResult {
    let marker = path.with_extension("cut");
    let child = Command::new(std::env::current_exe()?)
        .args(["--exact", CRASH_TEST, "--nocapture"])
        .env(super::CRASH_DB_ENV, path)
        .env(MODE, mode)
        .env(super::CRASH_CUT_STAGE_ENV, cut)
        .env(super::CRASH_CUT_MARKER_ENV, &marker)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let mut child = KillOnDrop::new(child);
    wait_for_marker(child.child(), &marker)?;
    assert_eq!(child.sigkill()?.signal(), Some(9));
    fs::remove_file(marker)?;
    Ok(())
}

#[tokio::test]
async fn sigkill_bootstrap_and_first_epoch_publication_keep_exact_authority() -> TestResult {
    for bootstrap_cut in ["before-commit", "after-commit"] {
        for (commit_cut, committed) in [
            (CrashCut::BeforeCommit, false),
            (CrashCut::PostCommitPreReply, true),
        ] {
            let directory = tempfile::tempdir()?;
            let path = legacy(directory.path())?;
            let before = feed(&path)?;
            let seal = old_receipt(&path)?;
            let old_id = CHECKPOINT.parse()?;
            let old = SqliteSessionStore::inspect_checkpoint(&path, old_id)?;
            kill_at(&path, "bootstrap", bootstrap_cut)?;
            assert_eq!(
                SqliteSessionStore::bootstrap_pending(&path)?,
                bootstrap_cut == "before-commit"
            );
            let backup = path.with_extension("pre-codec-bootstrap.sqlite3");
            assert!(SqliteSessionStore::bootstrap_pending(&backup)?);
            assert_eq!(feed(&backup)?, before);
            assert_eq!(feed(&path)?, before);
            kill_at(&path, "publication", commit_cut.name())?;
            let store = SqliteSessionStore::open_verified(&path)?;
            let head = store.load_branch(selector()?, "reopen").await?;
            let retained = store.load_checkpoint(old_id).await?;
            assert_eq!(retained.reference(), old.reference());
            assert_eq!(retained.checkpoint(), old.checkpoint());
            assert_eq!(old_receipt(&path)?, seal);
            assert_eq!(
                head.head().checkpoint().root().codec_revision(),
                if committed {
                    RUNTIME_FORMAT_EPOCH
                } else {
                    PREVIOUS_RUNTIME_FORMAT_EPOCH
                }
            );
            let outcome = store.recover_commit(advancing_checkpoint_id()?).await;
            if committed {
                let recovered = outcome?;
                let records = feed(&path)?;
                let FeedPage::Records {
                    records: old_records,
                    ..
                } = &before
                else {
                    return Err("legacy feed is not retained".into());
                };
                let FeedPage::Records {
                    records: new_records,
                    ..
                } = &records
                else {
                    return Err("mixed feed is not retained".into());
                };
                assert_eq!(new_records.len(), old_records.len() + 1);
                assert_eq!(&new_records[..old_records.len()], old_records);
                assert_eq!(
                    new_records.last().map(|record| record.header().seq),
                    Some(recovered.receipt().feed_seq())
                );
                assert_eq!(recovered.receipt().previous().checkpoint(), old.reference());
                assert_eq!(recovered.receipt().head(), head.head());
                let base =
                    LoadedBranch::from_store(head.session(), recovered.operation().expected(), old);
                let retry = commit_request(&base, recovered.operation().lease().clone())?;
                assert_eq!(store.commit(retry).await?, recovered.receipt());
                assert_eq!(feed(&path)?, records);
            } else {
                assert!(matches!(outcome, Err(SessionStoreError::NotFound)));
                assert_eq!(feed(&path)?, before);
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn sigkill_additive_admission_revision_preserves_the_current_store() -> TestResult {
    for cut in ["before-commit", "after-commit"] {
        let directory = tempfile::tempdir()?;
        let path = legacy(directory.path())?;
        SqliteSessionStore::bootstrap_stopped(&path, |_| {})?;
        let codec_backup = path.with_extension("pre-codec-bootstrap.sqlite3");
        let codec_bytes = fs::read(&codec_backup)?;
        let store = SqliteSessionStore::open(&path)?;
        let initial_lease = lease(&store).await?;
        let prior = store.load_branch(selector()?, "reopen").await?;
        let committed = store
            .commit(commit_request(&prior, initial_lease.clone())?)
            .await?;
        store.release(initial_lease).await?;
        store.shutdown().await?;
        drop(store);
        super::remove_admission_revision(&Connection::open(&path)?)?;
        let before = feed(&path)?;
        assert_eq!(
            SqliteSessionStore::open(&path).err(),
            Some(SessionStoreError::BootstrapRequired)
        );
        assert_eq!(
            SqliteSessionStore::audit(&path),
            Err(SessionStoreError::BootstrapRequired)
        );
        kill_at(&path, "bootstrap", cut)?;
        assert_eq!(
            SqliteSessionStore::bootstrap_pending(&path)?,
            cut == "before-commit"
        );
        let backup = path.with_extension("pre-admission-revision.sqlite3");
        assert!(SqliteSessionStore::bootstrap_pending(&backup)?);
        assert_eq!(feed(&backup)?, before);
        let backup_bytes = fs::read(&backup)?;
        SqliteSessionStore::bootstrap_stopped(&path, |_| {})?;
        assert_eq!(fs::read(&codec_backup)?, codec_bytes);
        assert_eq!(fs::read(&backup)?, backup_bytes);
        let retry_backup = path.with_extension("pre-admission-revision-1.sqlite3");
        assert_eq!(retry_backup.exists(), cut == "before-commit");
        if retry_backup.exists() {
            assert_eq!(feed(&retry_backup)?, before);
        }
        SqliteSessionStore::audit(&path)?;
        let store = SqliteSessionStore::open(&path)?;
        assert_eq!(
            store
                .recover_commit(advancing_checkpoint_id()?)
                .await?
                .receipt(),
            committed
        );
        let lease = lease(&store).await?;
        let next = store.load_branch(selector()?, "reopen").await?;
        store
            .commit(commit_to(&next, lease, fixture::id(86_002)?)?)
            .await?;
        store.shutdown().await?;
        drop(store);
        let post_revision = feed(&path)?;
        assert_ne!(post_revision, before);
        super::remove_admission_revision(&Connection::open(&path)?)?;
        assert!(SqliteSessionStore::bootstrap_pending(&path)?);
        assert_eq!(feed(&path)?, post_revision);
    }
    Ok(())
}

#[test]
fn indexed_admission_refuses_future_and_fractional_object_codecs() -> TestResult {
    let directory = tempfile::tempdir()?;
    let path = legacy(directory.path())?;
    SqliteSessionStore::bootstrap_stopped(&path, |_| {})?;
    let connection = Connection::open(&path)?;
    let future = RUNTIME_FORMAT_EPOCH + 1;
    for (codec, expected) in [
        (
            rusqlite::types::Value::Integer(i64::from(future)),
            SessionStoreError::UnsupportedRuntimeEpoch(future),
        ),
        (
            rusqlite::types::Value::Real(f64::from(PREVIOUS_RUNTIME_FORMAT_EPOCH) + 0.5),
            SessionStoreError::corrupt(),
        ),
    ] {
        connection.execute(
            "INSERT INTO runtime_objects(kind,codec,digest,byte_len) VALUES(20,?1,zeroblob(32),0)",
            [codec],
        )?;
        connection.execute("UPDATE store_meta SET objects=objects+1", [])?;
        assert_eq!(SqliteSessionStore::open(&path).err(), Some(expected));
        connection.execute(
            "DELETE FROM runtime_objects WHERE id=last_insert_rowid()",
            [],
        )?;
        connection.execute("UPDATE store_meta SET objects=objects-1", [])?;
    }
    SqliteSessionStore::audit(path)?;
    Ok(())
}

#[test]
fn bounded_admission_refuses_feed_framing_and_count_drift() -> TestResult {
    let directory = tempfile::tempdir()?;
    let path = legacy(directory.path())?;
    SqliteSessionStore::bootstrap_stopped(&path, |_| {})?;
    let connection = Connection::open(&path)?;
    let original: Vec<u8> =
        connection.query_row("SELECT record FROM session_feed LIMIT 1", [], |row| {
            row.get(0)
        })?;
    for (bytes, refusal) in [
        (
            b"{\"version\":2,\"record\":null}".as_slice(),
            SessionStoreError::UnsupportedFeedVersion(2),
        ),
        (b"not an envelope".as_slice(), SessionStoreError::corrupt()),
    ] {
        connection.execute("UPDATE session_feed SET record=?1", [bytes])?;
        assert_eq!(SqliteSessionStore::open(&path).err(), Some(refusal));
    }
    connection.execute("UPDATE session_feed SET record=?1", [original])?;
    let object: i64 =
        connection.query_row("SELECT id FROM runtime_objects LIMIT 1", [], |row| {
            row.get(0)
        })?;
    connection.execute(
        "UPDATE runtime_objects SET byte_len=byte_len || 'x' WHERE id=?1",
        [object],
    )?;
    assert_eq!(
        SqliteSessionStore::open(&path).err(),
        Some(SessionStoreError::corrupt())
    );
    connection.execute(
        "UPDATE runtime_objects SET byte_len=CAST(byte_len AS INTEGER) WHERE id=?1",
        [object],
    )?;
    connection.execute("UPDATE store_meta SET object_bytes=object_bytes+1", [])?;
    assert_eq!(
        SqliteSessionStore::open(&path).err(),
        Some(SessionStoreError::corrupt())
    );
    connection.execute(
        "UPDATE admission_counts SET object_bytes=object_bytes+1",
        [],
    )?;
    assert_eq!(
        SqliteSessionStore::audit(&path),
        Err(SessionStoreError::corrupt())
    );
    Ok(())
}

#[test]
fn unsupported_retained_payload_refuses_from_a_read_only_wal_snapshot() -> TestResult {
    let directory = tempfile::tempdir()?;
    let path = legacy(directory.path())?;
    SqliteSessionStore::bootstrap_stopped(&path, |_| {})?;
    drop(SqliteSessionStore::open(&path)?);
    let incumbent = Connection::open(&path)?;
    incumbent.execute_batch("PRAGMA wal_autocheckpoint=0; PRAGMA foreign_keys=ON")?;
    let epoch = PREVIOUS_RUNTIME_FORMAT_EPOCH - 1;
    let body = b"null";
    let digest = Digest::sha256(
        DigestDomain::Blob,
        epoch,
        &[
            b"plakous-agent/runtime-object-v3",
            &[20],
            &epoch.to_be_bytes(),
            body,
        ],
    )?;
    let tx = incumbent.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO runtime_objects(kind,codec,digest,byte_len) VALUES(20,?1,?2,4)",
        rusqlite::params![epoch, digest.bytes().as_slice()],
    )?;
    tx.execute(
        "INSERT INTO runtime_object_bytes VALUES(last_insert_rowid(),?1)",
        [body.as_slice()],
    )?;
    tx.execute(
        "INSERT INTO gc_candidates VALUES(20,?1,?2)",
        rusqlite::params![epoch, digest.bytes().as_slice()],
    )?;
    tx.execute(
        "UPDATE store_meta SET objects=objects+1,object_bytes=object_bytes+4",
        [],
    )?;
    tx.commit()?;
    let wal = path.with_file_name("runtime.sqlite3-wal");
    let database_before = fs::read(&path)?;
    let wal_before = fs::read(&wal)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o640))?;
    assert_eq!(
        SqliteSessionStore::open(&path).err(),
        Some(SessionStoreError::UnsupportedRuntimeEpoch(epoch)),
    );
    assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o777, 0o640);
    assert_eq!(fs::read(path)?, database_before);
    assert_eq!(fs::read(wal)?, wal_before);
    Ok(())
}

#[tokio::test]
async fn a_stale_writer_cannot_mutate_after_epoch_activation() -> TestResult {
    let directory = tempfile::tempdir()?;
    let path = legacy(directory.path())?;
    SqliteSessionStore::bootstrap_stopped(&path, |_| {})?;
    let store = SqliteSessionStore::open(&path)?;
    let retained = store.load_branch(selector()?, "reopen").await?;
    let next = RUNTIME_FORMAT_EPOCH + 1;
    Connection::open(&path)?.execute("UPDATE runtime_writer SET epoch=?1", [next])?;
    assert!(matches!(
        store.acquire(AcquireBranch::new(selector()?, fixture::id(86_000)?, NonZeroU64::MIN)).await,
        Err(SessionStoreError::UnsupportedRuntimeEpoch(epoch)) if epoch == next
    ));
    assert_eq!(store.load_branch(selector()?, "reopen").await?, retained);
    Ok(())
}
