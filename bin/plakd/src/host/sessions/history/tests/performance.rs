use super::*;
use plakous_host_protocol::views::presentation::ActivityKind;
use plakous_host_protocol::views::transcript::{ThinkingRow, ToolItem};

fn tool(n: u64) -> TranscriptRow {
    TranscriptRow::Tool(ToolItem {
        wire_operation: None,
        call_id: format!("call-{n}"),
        name: "read_file".into(),
        exposed_name: None,
        kind: ActivityKind::Tool,
        state: "done".into(),
        subject: Some(format!("record-{n}.txt")),
        detail: Some("text detail ☕\n".repeat(64)),
        started_at_ms: Some(1),
        finished_at_ms: Some(2),
        parent_call_id: None,
        cell_id: None,
        wait: None,
    })
}

// Large enough to exercise restore-time normalization of the entire prefix,
// not just SQLite's cheap byte-count path for assistant prose.
#[test]
#[ignore = "manual offline history latency measurement"]
fn heavy_history_measurement() {
    let fixture = Fixture::new();
    let starts = (0..500).map(|turn| turn * 20 + 1).collect();
    let turns = Turns {
        starts,
        retained_from: 1,
        older_from: None,
    };
    fixture.seed_items(
        10_000,
        (1..=10_000).map(|n| {
            let row = if n % 2 == 0 {
                tool(n)
            } else {
                TranscriptRow::Thinking(ThinkingRow {
                    location_key: format!("thought-{n}"),
                    text: "Café reasoning. ".repeat(48),
                    elapsed_ms: Some(2),
                })
            };
            (key(n, 0), row)
        }),
    );
    let first_start = Instant::now();
    let (cursor, first, total) = transcript_history::read(&fixture.paths, &fixture.id, |history| {
        let page = first(&history, &turns)?;
        assert_eq!(page.older.rows, 9_980);
        let total = page.older.bytes;
        let cursor = page.cursor.clone().unwrap();
        Ok((
            cursor,
            framing::baseline(page, target(), LiveTail::default())?,
            total,
        ))
    })
    .unwrap();
    let first_bytes = wire_bytes(&first);
    let first_time = first_start.elapsed();
    let older_start = Instant::now();
    let older = transcript_history::read(&fixture.paths, &fixture.id, |history| {
        let (_, _, retention, before) = decode_cursor(&cursor)?;
        framing::history(
            older_page(
                &history,
                &turns,
                retention,
                before,
                PAGE_TARGET_BYTES as u64,
            )?,
            7,
            2,
        )
    })
    .unwrap();
    let older_bytes = wire_bytes(&older);
    let older_time = older_start.elapsed();
    eprintln!(
        "heavy history: 10,000 rows, 5,000 tools + 5,000 thoughts; {total} preceding body bytes; first {first_bytes} wire bytes in {first_time:?}; older {older_bytes} wire bytes in {older_time:?}"
    );
}

fn wire_bytes(frames: &[HostFrameV13]) -> usize {
    frames
        .iter()
        .map(|frame| serde_json::to_vec(frame).unwrap().len() + 1)
        .sum()
}
