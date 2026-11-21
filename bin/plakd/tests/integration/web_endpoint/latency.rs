//! Opt-in latency probe, using only the real host and an offline scripted model.
//! Run with the test-support build and --run-ignored; PLAK_LATENCY_SECONDS
//! overrides the default ten-minute switching run for shorter comparisons.

use super::*;
use std::time::{Duration, Instant};

const CONSOLE_CSP: &str = concat!(
    "default-src 'self'; script-src 'self'; connect-src 'self'; ",
    "require-trusted-types-for 'script'; frame-ancestors 'none'; object-src 'none'; base-uri 'none'"
);
const SAMPLE_INTERVAL: Duration = Duration::from_secs(2);

fn welcome(socket: &mut WebSocket<UnixStream>) {
    send(
        socket,
        &ClientFrame::Hello(Hello {
            proto: WIRE_VERSION,
            binary_id: "latency-probe".into(),
            caps: Vec::new(),
        }),
    );
    assert_eq!(next_frame(socket)["frame"], "welcome");
}

fn open(fixture: &Fixture) -> Web {
    let (web, ready) = launch_on_with_env(
        fixture,
        &socket_dir(fixture).join("plak.sock"),
        &[],
        &[("PLAK_LOG", "info")],
        CONSOLE_CSP,
    );
    assert!(ready, "{:?}", web.lines.try_iter().collect::<Vec<_>>());
    web
}

fn create(socket: &mut WebSocket<UnixStream>, id: u64) -> (String, NodeAddress) {
    send(
        socket,
        &ClientFrame::Request {
            id,
            op: Op::NewSession {
                runner: plakous_host_protocol::wire::RunnerLabel::local(),
                profile: None,
                model: None,
                workspace: WORKSPACE.with(|workspace| workspace.borrow().clone()),
                template: None,
                request_id: format!("00000000-0000-4000-8000-{id:012}"),
                accounts: Vec::new(),
            },
        },
    );
    let result = response(socket, id);
    assert_eq!(result["ok"]["answer"], "created", "{result}");
    (
        result["ok"]["session"].as_str().unwrap().to_owned(),
        serde_json::from_value(result["ok"]["node"].clone()).unwrap(),
    )
}

/// Fixture-only data: seed two archived projections, large enough for two
/// wire pages each without dispatching any provider calls to build history.
fn seed_history(fixture: &Fixture, sessions: [&str; 2]) {
    let path = std::fs::read_dir(fixture.home().join("state"))
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("product.sqlite3"))
        .find(|path| path.is_file())
        .unwrap();
    let mut db = rusqlite::Connection::open(path).unwrap();
    let tx = db.transaction().unwrap();
    let row = serde_json::to_string(
        &plakous_host_protocol::views::transcript::TranscriptRow::assistant("x".repeat(16 * 1024)),
    )
    .unwrap();
    for id in sessions {
        assert_eq!(
            tx.execute("UPDATE sessions SET archived=1 WHERE id=?1", [id])
                .unwrap(),
            1
        );
        let mut put = tx.prepare(
            "INSERT INTO transcript_rows(session_id,anchor,lane,ordinal,row) VALUES (?1,1,0,?2,?3)"
        ).unwrap();
        for ordinal in 1..=640 {
            put.execute(rusqlite::params![id, ordinal, row]).unwrap();
        }
    }
    tx.commit().unwrap();
}

#[derive(Default)]
struct Samples {
    ping: Vec<Duration>,
    accepted: Vec<Duration>,
    console: [Vec<Duration>; 3],
    pages: usize,
}

const READS: [&str; 3] = ["profiles_list", "templates_list", "accounts_status"];

impl Samples {
    fn measure(
        &mut self,
        browsing: &mut WebSocket<UnixStream>,
        streaming: &mut WebSocket<UnixStream>,
        root: &NodeAddress,
        attachment: u64,
        round: usize,
    ) {
        let attach_id = (round as u64) * 4 + 10;
        let ping_id = attach_id + 1;
        let read_id = attach_id + 2;
        let send_id = attach_id + 3;
        let command = READS[round % READS.len()];
        let send_at = Instant::now();
        send(
            streaming,
            &ClientFrame::Request {
                id: send_id,
                op: Op::Intent {
                    client_id: Some(uuid::Uuid::new_v4().to_string().parse().unwrap()),
                    attachment,
                    intent: SessionIntent::Prompt(Draft::from("stream fixture")),
                    blobs: Vec::new(),
                },
            },
        );
        let acceptance = response(streaming, send_id);
        assert_eq!(acceptance["ok"]["answer"], "accepted", "{acceptance}");
        self.accepted.push(send_at.elapsed());
        let node = attach(browsing, root, attach_id);
        let ping_at = Instant::now();
        send(
            browsing,
            &ClientFrame::Request {
                id: ping_id,
                op: Op::Ping,
            },
        );
        let console_at = Instant::now();
        browsing.send(Message::text(json!({
            "frame": "request", "id": read_id, "op": {"op": "host", "request": {"request": command}}
        }).to_string())).unwrap();
        self.receive(
            browsing,
            node,
            [(ping_id, ping_at), (read_id, console_at)],
            round,
        );
        loop {
            let observation = next_frame(streaming);
            if observation["frame"] == "event"
                && observation.to_string().contains("probe stream finished")
            {
                break;
            }
            assert_ne!(observation["frame"], "resync_required");
        }
    }

    fn receive(
        &mut self,
        socket: &mut WebSocket<UnixStream>,
        attachment: u64,
        [(ping_id, ping_at), (read_id, console_at)]: [(u64, Instant); 2],
        round: usize,
    ) {
        let read = round % READS.len();
        let mut pages = 0;
        let mut last = false;
        let (mut pong, mut console) = (false, false);
        while !(last && pong && console) {
            let text = match socket.read().expect("the same socket still serves") {
                Message::Text(text) => text,
                Message::Ping(_) => {
                    socket.flush().unwrap();
                    continue;
                }
                other => panic!("unexpected transport frame {other:?}"),
            };
            let received = Instant::now();
            let line = text.as_str();
            if line.starts_with(r#"{"frame":"snapshot","#) {
                assert!(line.starts_with(&format!(
                    r#"{{"frame":"snapshot","attachment":{attachment},"#
                )));
                pages += 1;
                last = line.ends_with(r#","last":true}"#);
                continue;
            }
            let frame: Value = serde_json::from_str(line).unwrap();
            assert_ne!(frame["frame"], "resync_required", "attachment fell behind");
            match frame["id"].as_u64() {
                Some(id) if id == ping_id => {
                    assert_eq!(frame["outcome"]["ok"]["answer"], "pong", "{frame}");
                    self.ping.push(received - ping_at);
                    pong = true;
                }
                Some(id) if id == read_id => {
                    assert_eq!(
                        frame["outcome"]["ok"]["response"]["response"], READS[read],
                        "{frame}"
                    );
                    self.console[read].push(received - console_at);
                    console = true;
                }
                _ => {}
            }
        }
        assert!(pages >= 2, "probe needs a multi-page baseline, got {pages}");
        self.pages += pages;
    }
}

fn report(name: &str, samples: &mut [Duration], percentile: usize) {
    samples.sort_unstable();
    let index = (samples.len() * percentile).div_ceil(100) - 1;
    eprintln!(
        "{name}: n={}, p{percentile}={:.2} ms, max={:.2} ms",
        samples.len(),
        samples[index].as_secs_f64() * 1000.0,
        samples.last().unwrap().as_secs_f64() * 1000.0,
    );
}

#[test]
#[ignore = "opt-in latency probe; measures instead of enforcing wall-clock thresholds"]
fn host_latency_probe() {
    let seconds =
        std::env::var("PLAK_LATENCY_SECONDS").map_or(600, |value| value.parse::<u64>().unwrap());
    assert!(seconds >= 6);
    let stream: Vec<_> = (0..40)
        .flat_map(|_| [json!({"text":"delta "}), json!({"stall":25})])
        .collect();
    let fixture = Fixture::new(&json!({
        "version": 1,
        "repeat_last_step": true,
        "steps": [{"stream": stream, "output": {"text": "probe stream finished"}}]
    }));
    let config = fixture.home().join("config.toml");
    let mut text = std::fs::read_to_string(&config).unwrap();
    write!(text, "\n[web]\nallow = [\"{LOGIN}\"]\norigin = \"{ORIGIN}\"\naccounts_read = true\nconfig_read = true\n").unwrap();
    write_private(&config, &text);
    let mut web = open(&fixture);
    let mut seed = upgrade(&web, &cookie(&web));
    welcome(&mut seed);
    let (first, a) = create(&mut seed, 1);
    let (second, b) = create(&mut seed, 2);
    let (_, live) = create(&mut seed, 3);
    drop(seed);
    interrupt_and_wait(&mut web);
    seed_history(&fixture, [&first, &second]);

    let mut web = open(&fixture);
    let cookie = cookie(&web);
    let mut browsing = upgrade(&web, &cookie);
    let mut streaming = upgrade(&web, &cookie);
    welcome(&mut browsing);
    welcome(&mut streaming);
    send(
        &mut browsing,
        &ClientFrame::Request {
            id: 1,
            op: Op::WatchSessions {
                include_archived: true,
            },
        },
    );
    assert_eq!(response(&mut browsing, 1)["ok"]["answer"], "watching");
    let attachment = attach(&mut streaming, &live, 1);
    let mut samples = Samples::default();
    let start = Instant::now();
    let elapsed = Duration::from_secs(seconds);
    let mut round = 0;
    while start.elapsed() < elapsed || round < READS.len() {
        let began = Instant::now();
        samples.measure(
            &mut browsing,
            &mut streaming,
            [&a, &b][round % 2],
            attachment,
            round,
        );
        round += 1;
        std::thread::sleep(SAMPLE_INTERVAL.saturating_sub(began.elapsed()));
    }
    drop((browsing, streaming));
    interrupt_and_wait(&mut web);
    let log = std::fs::read_to_string(fixture.home().join("logs/plak.log")).unwrap();
    assert!(!log.contains("client fell behind"));
    eprintln!(
        "{round} switches, {} pages, {:.1}s, no resync, no replacement, no console deadline",
        samples.pages,
        start.elapsed().as_secs_f64()
    );
    report("ping", &mut samples.ping, 99);
    report("intent accepted", &mut samples.accepted, 99);
    for (name, values) in READS.into_iter().zip(&mut samples.console) {
        report(name, values, 95);
    }
}
