use super::{BenchResult, Database, SCHEMA, conformance, elapsed_ns};
use plakous_agent::internal::{
    ArchiveSession, CreateSession, RuntimeContextState, RuntimeDurability, SessionRef,
    SessionStore, Timestamp,
};
use plakous_store::SqliteSessionStore;
use serde_json::json;
use std::num::NonZeroU64;
use std::time::Instant;

const ROOTS: usize = 44;
const CHILDREN: usize = 40;
const ARCHIVED: usize = 28;

pub(super) async fn run() -> BenchResult<()> {
    let db = Database::new()?;
    let store = SqliteSessionStore::open(&db.path)?;
    let mut selectors = Vec::new();
    for index in 0..ROOTS + CHILDREN {
        let segments = match index {
            0 => 4_096,
            1..=CHILDREN => 1_024 + u32::try_from(index % 4)? * 256,
            _ => 32,
        };
        let session = conformance::id(800_000 + index as u128 * 4)?;
        let branch = conformance::id(800_001 + index as u128 * 4)?;
        let selector = SessionRef::new(session, branch);
        let context = RuntimeContextState::genesis(session, branch, [0x53; 32])?;
        let checkpoint = conformance::checkpoint_with_payload(
            conformance::topology(
                session,
                branch,
                conformance::id(800_002 + index as u128 * 4)?,
                None,
                0,
                None,
                1,
            )?,
            segments,
            RuntimeDurability::Required,
            context,
            false,
            &format!(" sample {index} {}", "x".repeat(4_600)),
        )?;
        let receipt = store
            .create(CreateSession::new(
                selector,
                Timestamp::new(50, 0)?,
                conformance::id(800_003 + index as u128 * 4)?,
                NonZeroU64::new(3_600).ok_or("ttl")?,
                conformance::bundle(&checkpoint, None)?,
            ))
            .await?;
        store.release(receipt.lease().clone()).await?;
        if index >= ROOTS + CHILDREN - ARCHIVED {
            store
                .archive(ArchiveSession::new(session, receipt.session().revision()))
                .await?;
        }
        selectors.push(selector);
    }
    store.shutdown().await?;
    let store = SqliteSessionStore::open(&db.path)?;
    let root = selectors[0];
    let started = Instant::now();
    let branch = store.load_branch(root, "benchmark").await?;
    let cold_actor_load_ns = elapsed_ns(started);
    println!(
        "{}",
        json!({"schema": SCHEMA, "kind": "branch-read-fixture",
            "roots": ROOTS, "children": CHILDREN, "archived_roots": ARCHIVED,
            "database_bytes": std::fs::metadata(&db.path)?.len(),
            "cold_root_objects": branch.checkpoint().bundle().stats().object_count(),
            "cold_actor_load_ns": cold_actor_load_ns,
        })
    );
    for (operation, targets, repetitions, metadata_only) in [
        ("warm_root", &selectors[..1], 1, false),
        ("child_lifecycle", &selectors[1..=CHILDREN], 1, true),
        ("root_lifecycle", &selectors[..1], 40, true),
        ("warm_root_after_observations", &selectors[..1], 1, false),
    ] {
        let started = Instant::now();
        for _ in 0..repetitions {
            for &target in targets {
                if metadata_only {
                    std::hint::black_box(store.load_branch_metadata(target, "benchmark").await?);
                } else {
                    std::hint::black_box(store.load_branch(target, "benchmark").await?);
                }
            }
        }
        let elapsed_ns = elapsed_ns(started);
        let observations = repetitions * targets.len();
        let full_loads = if metadata_only { 0 } else { observations };
        println!(
            "{}",
            json!({"schema": SCHEMA, "kind": "branch-read-operation",
                "operation": operation, "observations": observations,
                "full_loads": full_loads, "elapsed_ns": elapsed_ns,
            })
        );
    }
    store.shutdown().await?;
    Ok(())
}
