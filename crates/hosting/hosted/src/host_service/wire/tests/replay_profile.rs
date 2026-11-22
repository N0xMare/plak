//! Opt-in memory and replay probe for three attachments to one resident view.

use super::*;
use plakous_host_protocol::views::transcript::{Lane, RowKey, TranscriptItem, TranscriptPatch};
use plakous_host_protocol::{Core, intent_channel};
use std::sync::{Barrier, mpsc};
use std::time::{Duration, Instant};

const PATCHES: u64 = 64;
const PATCH_ROWS: u64 = 10;
const VIEW_ROWS: u64 = 30;
const ROW_BYTES: usize = 512 * 1024;
const SLOW_BYTES_PER_SEC: u64 = 16 * 1024 * 1024;

fn rows(start: u64, count: u64) -> Vec<TranscriptItem> {
    (start..start + count)
        .map(|key| TranscriptItem {
            key: RowKey::new(key, Lane::Runtime, 0),
            row: TranscriptRow::assistant("x".repeat(ROW_BYTES)),
        })
        .collect()
}

fn snapshot(revision: u64, start: u64) -> SessionObservation<Core> {
    SessionObservation::PresentationSnapshot(PresentationSnapshot {
        items: rows(start, VIEW_ROWS),
        ..PresentationSnapshot::empty(revision)
    })
}

fn cpu_ticks() -> u64 {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
    let (_, fields) = stat.rsplit_once(')').unwrap();
    fields
        .split_whitespace()
        .skip(11)
        .take(2)
        .map(|ticks| ticks.parse::<u64>().unwrap())
        .sum()
}

fn rss() -> (u64, u64) {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let read = |key: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(key))
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .parse()
            .unwrap()
    };
    (read("VmRSS:"), read("VmHWM:"))
}

struct Link {
    started: Instant,
    first_page: Option<Duration>,
    slow: bool,
    bytes: u64,
}

impl Write for Link {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.slow {
            let nanos = u64::try_from(bytes.len()).unwrap() * 1_000_000_000 / SLOW_BYTES_PER_SEC;
            std::thread::sleep(Duration::from_nanos(nanos));
        }
        self.bytes += u64::try_from(bytes.len()).unwrap();
        if self.first_page.is_none() && bytes.starts_with(br#"{"frame":"snapshot","#) {
            self.first_page = Some(self.started.elapsed());
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
#[ignore = "Linux process metrics and a throttled several-hundred-MiB replay"]
fn three_resident_attachments_replay_profile() {
    let (actions, intents) = intent_channel::<Core>();
    let service = HostService::new(actions.clone(), root(), ATTACHMENT_CAPACITY);
    let cpu_initial = cpu_ticks();
    service.observe(snapshot(1, 1));
    for index in 0..PATCHES {
        let start = index * PATCH_ROWS + 1;
        service.observe(SessionObservation::TranscriptPatch(TranscriptPatch {
            scope: TranscriptScope::Root,
            from_revision: index + 1,
            to_revision: index + 2,
            upserts: rows(start + VIEW_ROWS, PATCH_ROWS),
            deletes: (start..start + PATCH_ROWS)
                .map(|key| RowKey::new(key, Lane::Runtime, 0))
                .collect(),
        }));
    }
    let cpu_attaching = cpu_ticks();
    eprintln!(
        "replay_profile seeded rss_kib={:?} cpu_ticks={}",
        rss(),
        cpu_attaching - cpu_initial
    );

    let start = Barrier::new(4);
    let (admitted, admissions) = mpsc::channel();
    std::thread::scope(|scope| {
        let clients: Vec<_> = (0..3)
            .map(|index| {
                let start = &start;
                let admitted = admitted.clone();
                let service = service.clone();
                let actions = actions.clone();
                scope.spawn(move || {
                    let started = Instant::now();
                    let link = Link {
                        started,
                        first_page: None,
                        slow: index == 2,
                        bytes: 0,
                    };
                    let mut connection =
                        WireConnection::on_service(service.clone(), link, actions, Leave::Detach);
                    assert!(matches!(
                        connection.request(1, Op::Attach { node: root() }),
                        Some(Outcome::Ok(Answer::Attached { .. }))
                    ));
                    admitted.send(()).unwrap();
                    start.wait();
                    connection.flush().unwrap();
                    let link = &connection.stdout.inner;
                    eprintln!(
                        "replay_profile client={index} first_page_ms={} replay_ms={} bytes={}",
                        link.first_page.unwrap().as_millis(),
                        started.elapsed().as_millis(),
                        link.bytes
                    );
                    connection
                })
            })
            .collect();
        for _ in 0..3 {
            admissions.recv_timeout(Duration::from_secs(15)).unwrap();
        }
        eprintln!("replay_profile admitted rss_kib={:?}", rss());
        let mut resnapshots = 0;
        while let Ok(intent) = intents.try_recv() {
            assert!(matches!(
                &*intent,
                SessionIntent::ResyncTranscript {
                    scope: TranscriptScope::Root
                }
            ));
            resnapshots += 1;
        }
        if resnapshots != 0 {
            service.observe(snapshot(PATCHES + 1, PATCHES * PATCH_ROWS + 1));
        }
        eprintln!(
            "replay_profile fresh_requests={resnapshots} draining rss_kib={:?}",
            rss()
        );
        start.wait();
        let clients: Vec<_> = clients
            .into_iter()
            .map(|client| client.join().unwrap())
            .collect();
        eprintln!(
            "replay_profile finished rss_kib={:?} attach_cpu_ticks={}",
            rss(),
            cpu_ticks() - cpu_attaching
        );
        assert_eq!(clients.len(), 3);
    });
}
