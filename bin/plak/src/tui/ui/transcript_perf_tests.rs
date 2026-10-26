//! Opt-in real-publisher transcript timings and ordinary copy regressions.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "test assertions")]

use std::time::Instant;

use super::test_support::{launch_basic, pin_clock};
use super::{App, ContentPosition};
use crate::surface_protocol::{ContentEvent, HostEventReceiver, PlakExtension};
use crate::tui::blocks::ScrollBlock;
use plakous_host_protocol::views::PresentationSnapshot;
use plakous_host_protocol::views::transcript::{
    Lane, RowKey, TranscriptItem, TranscriptRow, TranscriptScope,
};
use plakous_hosted::runtime::{Sender, test_kit};
use ratatui::{Terminal, backend::TestBackend};

const LOCAL_NOTE: &str = "probe local system note";

struct Probe {
    app: App,
    sender: Sender<PlakExtension>,
    receiver: HostEventReceiver,
    terminal: Terminal<TestBackend>,
}

impl Probe {
    fn new(count: u64) -> Self {
        let mut app = App::new(launch_basic(), true);
        pin_clock(&mut app);
        app.render_prefs.reduced_motion = true;
        app.render_prefs.group_tool_calls = false;
        let (events, receiver) = crate::tui::host_event_channel();
        let sender = Sender::over(events);
        let mut items = vec![TranscriptItem {
            key: RowKey::new(0, Lane::Runtime, 0),
            row: TranscriptRow::SessionHeader {
                session_id: "s1".into(),
                carried_from: None,
            },
        }];
        // Two constant tail rows also make the zero-history selection useful.
        items.extend((1..=count + 2).map(|anchor| TranscriptItem {
            key: RowKey::new(anchor, Lane::Runtime, 0),
            row: TranscriptRow::assistant(format!(
                "row {anchor:05}: This is a synthetic transcript line with enough ordinary words to be roughly a hundred columns wide."
            )),
        }));
        assert!(test_kit::replace_transcript(
            &sender,
            PresentationSnapshot {
                items,
                ..PresentationSnapshot::default()
            }
        ));
        assert!(sender.send(ContentEvent::SetBusy(true)));
        let mut probe = Self {
            app,
            sender,
            receiver,
            terminal: Terminal::new(TestBackend::new(100, 30)).unwrap(),
        };
        probe.frame();
        probe
    }

    fn frame(&mut self) {
        for event in self.receiver.try_iter() {
            self.app.apply_host_event(event);
        }
        self.terminal.draw(|frame| self.app.draw(frame)).unwrap();
    }

    fn tool(&mut self, id: &str, state: &str) {
        assert!(self.sender.send(ContentEvent::ToolLine {
            parent_call_id: None,
            cell_id: None,
            kind: crate::tui::presentation::ActivityKind::Tool,
            call_id: Some(id.into()),
            name: "read_file".into(),
            state: state.into(),
            subject: Some("probe.txt".into()),
            detail: None,
            duration_ms: None,
            started_at_ms: None,
            finished_at_ms: None,
            wait: None,
        }));
        self.frame();
    }

    fn call(&mut self, n: usize) -> usize {
        let id = format!("probe-call-{n}");
        self.tool(&id, "start");
        let first = usize::from(self.app.pane.selection.has_range());
        self.tool(&id, "done");
        first + usize::from(self.app.pane.selection.has_range())
    }

    fn select(&mut self, scroll: usize) {
        self.app.pane.scroll_from_bottom = scroll;
        self.frame();
        let layout = self.app.pane.transcript_cache.layout();
        let start = (self.app.pane.transcript_visible_start..layout.plain.len().saturating_sub(1))
            .find(|row| {
                layout.block_index[*row].is_some()
                    && layout.block_index[*row + 1] == layout.block_index[*row]
                    && !layout.plain[*row].is_empty()
                    && !layout.plain[*row + 1].is_empty()
            })
            .expect("two rendered text rows");
        self.app.pane.selection.begin(
            ContentPosition {
                row: start,
                column: 0,
            },
            self.app.last_transcript_width,
            self.app.pane.transcript_revision,
        );
        self.app.pane.selection.drag(ContentPosition {
            row: start + 1,
            column: 99,
        });
        self.app.pane.selection.capture_blocks(layout);
        self.frame();
    }

    fn pending_approval(&mut self) {
        self.tool("pending", "start");
        assert!(self.sender.send(ContentEvent::ApprovalRequired {
            approval_id: "probe-approval".into(),
            call_id: Some("pending".into()),
            tool_name: "read_file".into(),
            subject: Some("probe decision".into()),
            prompt: None,
            expires_at_ms: u64::MAX,
        }));
        self.frame();
        self.app
            .push_root_block(ScrollBlock::System(LOCAL_NOTE.into()));
        self.frame();
    }

    fn resync(&mut self) {
        assert!(test_kit::resync_transcript(
            &self.sender,
            &TranscriptScope::Root
        ));
        self.frame();
    }

    fn tail_order(&self) -> Vec<&'static str> {
        self.app
            .pane
            .blocks
            .iter()
            .filter_map(|block| match block {
                ScrollBlock::Tool {
                    call_id: Some(id), ..
                } if id == "pending" => Some("tool"),
                ScrollBlock::Approval { .. } => Some("approval"),
                ScrollBlock::System(text) if text == LOCAL_NOTE => Some("local"),
                _ => None,
            })
            .collect()
    }
}

#[test]
fn hosted_boundaries_and_busy_resync_keep_selection_copyable() {
    for count in [0, 8_000, 9_990] {
        let mut probe = Probe::new(count);
        probe.pending_approval();
        probe.select(300);
        for n in 0..5 {
            probe.call(n);
        }
        probe.resync();
        assert_eq!(probe.tail_order(), ["tool", "approval", "local"]);
        let copied = probe
            .app
            .take_transcript_selection_text()
            .expect("surviving selection copies");
        assert!(
            copied
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .contains("hundred columns wide."),
            "{copied:?}"
        );
    }
}

#[test]
#[ignore = "debug performance probe; run alone on the same lane for both trees"]
fn transcript_selection_perf_probe() {
    let mut copies_and_order = true;
    for count in [0, 8_000, 9_990] {
        let mut plain = Probe::new(count);
        let start = Instant::now();
        for n in 0..100 {
            plain.call(n);
        }
        let boundary = start.elapsed().as_secs_f64() * 10.0;

        let mut selected = Probe::new(count);
        selected.select(300);
        let start = Instant::now();
        let survived: usize = (0..20).map(|n| selected.call(n)).sum();
        let selected_ms = start.elapsed().as_secs_f64() * 50.0;

        let mut busy = Probe::new(count);
        busy.pending_approval();
        busy.select(300);
        let start = Instant::now();
        let mut resync_survived = 0;
        for _ in 0..20 {
            busy.resync();
            resync_survived += usize::from(busy.app.pane.selection.has_range());
        }
        let resync_ms = start.elapsed().as_secs_f64() * 50.0;
        let order = busy.tail_order();

        let mut copy = Probe::new(count);
        copy.select(300);
        for n in 0..5 {
            copy.call(n);
        }
        let copied = copy.app.take_transcript_selection_text();
        println!(
            "TRANSCRIPT_PERF history={count} boundary_ms={boundary:.3} selected_ms={selected_ms:.3} survived={survived}/40 resync_ms={resync_ms:.3} resync_survived={resync_survived}/20 order={order:?} copy={}",
            copied.is_some()
        );
        copies_and_order &= copied.is_some() && order == ["tool", "approval", "local"];
    }
    assert!(
        copies_and_order,
        "copy and tail-order contracts; see every history size above"
    );
}
