//! Offline shaped-link exercise of a retained baseline plus a large patch replay.

use super::*;
use plakous_host_protocol::SessionObservation;
use plakous_host_protocol::views::presentation::PresentationSnapshot;
use plakous_host_protocol::views::transcript::{
    Lane, RowKey, TranscriptItem, TranscriptPatch, TranscriptRow, TranscriptScope,
};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

const ROW_BYTES: usize = 512 * 1024;
const PATCH_ROWS: u64 = 10;
const PATCHES: u64 = 64;
const RATE_BYTES: u64 = 16 * 1024 * 1024;

fn rows(first: u64, count: u64) -> Vec<TranscriptItem> {
    (first..first + count)
        .map(|key| TranscriptItem {
            key: RowKey::new(key, Lane::Runtime, 0),
            row: TranscriptRow::assistant("x".repeat(ROW_BYTES)),
        })
        .collect()
}

fn history() -> Vec<SessionObservation<PlakExtension>> {
    // 335 MiB of source rows. The visible suffix is only 15 MiB at any
    // revision, but one attach carries those 64 retained revisions too.
    let mut observations = vec![SessionObservation::PresentationSnapshot(
        PresentationSnapshot {
            items: rows(1, 30),
            ..PresentationSnapshot::empty(1)
        },
    )];
    for index in 0..PATCHES {
        let start = index * PATCH_ROWS + 1;
        observations.push(SessionObservation::TranscriptPatch(TranscriptPatch {
            scope: TranscriptScope::Root,
            from_revision: index + 1,
            to_revision: index + 2,
            upserts: rows(start + 30, PATCH_ROWS),
            deletes: (start..start + PATCH_ROWS)
                .map(|key| RowKey::new(key, Lane::Runtime, 0))
                .collect(),
        }));
    }
    observations
}

async fn shaped_pair() -> (Server, Client, tokio::task::JoinHandle<()>) {
    const PIPE: usize = 64 * 1024;
    let (server, source) = tokio::io::duplex(PIPE);
    let (destination, client) = tokio::io::duplex(PIPE);
    let (mut from_server, mut to_server) = tokio::io::split(source);
    let (mut from_client, mut to_client) = tokio::io::split(destination);
    let shaper = tokio::spawn(async move {
        let downstream = async {
            let mut buffer = vec![0; PIPE];
            let delay = Duration::from_millis(25);
            let mut next = Instant::now() + delay;
            while let Ok(read) = from_server.read(&mut buffer).await {
                if read == 0 {
                    break;
                }
                let now = Instant::now();
                if now.saturating_duration_since(next) > delay {
                    next = now + delay;
                }
                let nanos = u64::try_from(read).unwrap() * 1_000_000_000 / RATE_BYTES;
                next += Duration::from_nanos(nanos);
                tokio::time::sleep_until(next).await;
                if to_client.write_all(&buffer[..read]).await.is_err() {
                    break;
                }
            }
            let _ = to_client.shutdown().await;
        };
        tokio::select! {
            biased;
            () = downstream => {}
            _ = tokio::io::copy(&mut from_client, &mut to_server) => {}
        }
    });
    (
        WebSocketStream::from_raw_socket(ProgressIo::new(server), Role::Server, None).await,
        WebSocketStream::from_raw_socket(client, Role::Client, None).await,
        shaper,
    )
}

fn process_cpu() -> Duration {
    use rustix::time::{ClockId, clock_gettime};
    let time = clock_gettime(ClockId::ProcessCPUTime);
    Duration::new(
        time.tv_sec.try_into().unwrap(),
        time.tv_nsec.try_into().unwrap(),
    )
}

async fn receive_attach(client: &mut Client, started: Instant) -> (Option<Duration>, u64) {
    let mut first = None;
    let mut baseline_complete = false;
    let mut patches = 0;
    while let Some(Ok(message)) = client.next().await {
        match message {
            Message::Text(line) => {
                let raw = line.as_bytes();
                if raw.starts_with(br#"{"frame":"snapshot","#) {
                    first.get_or_insert_with(|| started.elapsed());
                    baseline_complete |= raw.ends_with(br#","last":true}"#);
                }
                if raw.starts_with(br#"{"frame":"event","#)
                    && raw[..raw.len().min(256)]
                        .windows(18)
                        .any(|part| part == b"\"transcript_patch\"")
                {
                    patches += 1;
                    if patches == PATCHES {
                        assert!(
                            baseline_complete,
                            "the baseline must finish before its replay"
                        );
                        client
                            .send(Message::text(r#"{"frame":"bye"}"#))
                            .await
                            .unwrap();
                        return (first, patches);
                    }
                }
            }
            Message::Ping(_) => client.flush().await.unwrap(),
            Message::Close(_) => break,
            _ => {}
        }
    }
    (first, patches)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "several hundred MiB over a shaped link per attempt"]
async fn large_attach_baseline_pressure_probe() {
    let (_home, mut fixture) = context();
    fixture.host = principal_tests::archived_history(history);
    let mut complete = 0;
    for round in 1..=2 {
        let outbox = Outbox::default();
        let snapshots = outbox.clone();
        let mut connection = fixture
            .host
            .connect(
                outbox.clone(),
                Face::Remote,
                plakous_hosted::host_service::wire::HostPrincipal::Web("fixture".into()),
                || false,
            )
            .buffered_output()
            .with_snapshot_output(move |attachment, snapshot| {
                snapshots.snapshot(attachment, snapshot)
            });
        let (server, mut client, shaper) = shaped_pair().await;
        let started = Instant::now();
        let cpu = process_cpu();
        client.send(Message::text(ATTACH)).await.unwrap();
        let (ended, (first, patches)) = tokio::time::timeout(Duration::from_secs(120), async {
            tokio::join!(
                attached(server, &fixture, &mut connection, &outbox),
                receive_attach(&mut client, started)
            )
        })
        .await
        .unwrap();
        shaper.abort();
        let spent = process_cpu().checked_sub(cpu).unwrap();
        eprintln!(
            "baseline_pressure round={round} ended={:?} first_page_ms={:?} patches={patches} wall_ms={} process_cpu_ms={}",
            ended.as_str(),
            first.map(|time| time.as_millis()),
            started.elapsed().as_millis(),
            spent.as_millis(),
        );
        complete +=
            usize::from(matches!(ended, Ended::Bye) && first.is_some() && patches == PATCHES);
    }
    assert_eq!(
        complete, 2,
        "both attaches must finish without reconnecting"
    );
}
