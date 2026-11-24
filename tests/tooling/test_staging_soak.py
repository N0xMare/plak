"""The client oracle refuses the failure shapes the staging soak is meant to catch."""

import sys
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch as mocked

PERF = Path(__file__).resolve().parents[2] / "tools" / "perf"
sys.path.insert(0, str(PERF))
from staging_soak import run_exercise, window_reasons
from staging_soak_evidence import FAST_PING_P99_MS, FAST_SEND_P99_MS, classify_cycle
from staging_soak_measure import check_load, decode_stamp
from staging_soak_stream import CHECKPOINT_MISSING, LATE_PREFIX, PROVISIONAL, RootTrace, chunk


def item(anchor, text):
    return {
        "key": {"anchor": anchor, "lane": "runtime", "ordinal": 0},
        "row": {"kind": "assistant", "text": text},
    }


def baseline(*items):
    return {
        "frame": "snapshot",
        "scope": "root",
        "page": 0,
        "last": True,
        "revision": 10,
        "items": list(items),
    }


def event(seq, kind, body):
    return {"frame": "event", "seq": seq, "observation": {"kind": kind, "body": body}}


def patch(seq, *rows, previous=10, deletes=()):
    return event(
        seq,
        "transcript_patch",
        {
            "scope": "root",
            "from_revision": previous,
            "to_revision": 20,
            "upserts": rows,
            "deletes": list(deletes),
        },
    )


def tip_only():
    prefix, tip = chunk(0, 0), chunk(0, 1)
    trace = RootTrace(2)
    trace.observe(baseline())
    trace.observe(event(2, "assistant_delta", tip))
    trace.observe(patch(3, item(55, prefix + tip)))
    return trace.receipt(3, 1, "root", {0: prefix + tip}, trace.checkpoints)


def clean_window(**updates):
    return {
        "side": "B",
        "steps": 8,
        "first_event_n": 8,
        "scheduled_steps": 8,
        "streamed_steps": 8,
        "attach_n": 7,
        "scheduled_attaches": 7,
        "ping_n": 10,
        "heavy_attach_while_streaming_n": 2,
        "request_failed": 0,
        "fell_behind": 0,
        "active_reader_closes": 0,
        "warmup_clean": True,
        "stalled_reader_backpressured": True,
        "persisted_successful_steps": 8,
        "reference_mismatches": 0,
        "roots_with_final_text": 2,
        "reconcile_failures": 0,
        "attachments": [],
        "attach_fast_ms_p95": 2.0,
        "cpu_total_ms_per_s": 100.0,
        "rss_mib_per_min": 0.0,
        "rss_end_mib": 10.0,
        "send_frame_fast_n": 10,
        "send_frame_fast_unstamped": 0,
        "send_fast_negative": 0,
        "send_frame_fast_ms_p50": 0.5,
        "send_frame_fast_ms_p99": 2.0,
        "ping_ms_p99": 1.0,
        **updates,
    }


def reasons(window, *, known=False, stamps=True):
    args = SimpleNamespace(
        seconds=4,
        clients=7,
        slow=1,
        work_roots=2,
        quick=False,
        require_candidate_send_stamps=stamps,
    )
    return window_reasons(window, args, allow_known_defect=known)


class ReconciliationTests(unittest.TestCase):
    def test_attach_baseline_supplies_the_prefix_once(self):
        prefix, tail = chunk(0, 0), chunk(0, 1)
        provisional = item(PROVISIONAL, prefix)
        trace = RootTrace(2)
        trace.observe(baseline(provisional))
        trace.observe(event(2, "reasoning_delta", {"text": "not assistant prose"}))
        trace.observe(event(3, "assistant_delta", tail))
        trace.observe(patch(4, item(55, prefix + tail), deletes=[provisional["key"]]))
        report = trace.receipt(3, 1, "root", {0: prefix + tail}, trace.checkpoints)
        self.assertEqual(report["mismatches"], 0)
        self.assertEqual(report["baselines"], 1)
        self.assertEqual(report["patches"], 1)
        self.assertEqual(report["authority_checks"], 2)
        self.assertEqual(report["completed_texts"], 1)

    def test_tip_only_fails_even_when_the_boundary_repairs_it(self):
        report = tip_only()
        self.assertEqual(report["first_issue"]["reason"], "late_attach_missing_live_prefix")
        self.assertEqual(report["reconcile_reasons"], {LATE_PREFIX: 1, CHECKPOINT_MISSING: 1})
        self.assertEqual(report["paired_prefixes"], 1)
        self.assertEqual(report["unseeded_deltas"], 1)
        self.assertEqual(report["evicted_deltas"], 0)
        self.assertEqual(report["completed_texts"], 1)

    def test_interior_delta_loss_and_revision_gap_are_distinct(self):
        trace = RootTrace(3, continuous=True)
        trace.observe(baseline())
        trace.observe(event(2, "assistant_delta", chunk(0, 0)))
        trace.observe(event(4, "assistant_delta", chunk(0, 2)))
        trace.observe(patch(5, item(55, chunk(0, 0)), previous=11))
        self.assertEqual(trace.counts.evicted_deltas, 1)
        self.assertEqual(trace.counts.gap_frames, 1)
        self.assertEqual(trace.counts.revision_gaps, 1)
        self.assertEqual(trace.revision, 10)

    def test_authoritative_comparison_catches_a_short_but_valid_local_baseline(self):
        trace = RootTrace(3)
        trace.observe(baseline(item(PROVISIONAL, chunk(0, 0))))
        authority = RootTrace(3)
        authority.observe(baseline(item(PROVISIONAL, chunk(0, 0) + chunk(0, 1))))
        report = trace.receipt(3, 1, "root", {}, authority.checkpoints)
        self.assertEqual(report["first_issue"]["reason"], "host_checkpoint_mismatch")

    def test_canonical_text_must_match_the_durable_end(self):
        trace = RootTrace(2)
        trace.observe(baseline(item(55, chunk(0, 0) + chunk(0, 1))))
        report = trace.receipt(3, 1, "root", {0: chunk(0, 0)}, trace.checkpoints)
        self.assertEqual(report["first_issue"]["reason"], "durable_text_mismatch")

    def test_a_missing_host_revision_is_not_conclusive(self):
        trace = RootTrace(2)
        trace.observe(baseline())
        report = trace.receipt(3, 1, "root", {}, {})
        self.assertEqual(report["first_issue"]["reason"], "host_checkpoint_not_observed")

    def test_only_matched_late_prefix_repairs_can_be_known(self):
        attachment = tip_only()
        window = clean_window(reconcile_failures=2, attachments=[attachment])
        self.assertEqual(reasons(window), (["reconcile mismatches: 2"], [], 0))
        self.assertEqual(reasons(window, known=True), ([], [], 1))
        self.assertEqual(
            classify_cycle([], [], 1, nightly=True, timing_qualified=True, aa=False),
            ("known_defect", 76),
        )
        for fields in (
            {"continuous": True},
            {"evicted_deltas": 1},
            {"revision_gaps": 1},
            {"paired_prefixes": 0},
            {"reconcile_reasons": {LATE_PREFIX: 1, "host_checkpoint_mismatch": 1}},
            {"reconcile_reasons": {LATE_PREFIX: 1, "durable_text_mismatch": 1}},
        ):
            with self.subTest(fields=fields):
                invalid = window | {"attachments": [attachment | fields]}
                self.assertEqual(reasons(invalid, known=True)[2], 0)
        self.assertEqual(reasons(window | {"reference_mismatches": 1}, known=True)[2], 0)

    def test_a_prefix_without_its_repair_is_not_known(self):
        trace = RootTrace(2)
        trace.observe(baseline())
        trace.observe(event(2, "assistant_delta", chunk(0, 1)))
        attachment = trace.receipt(3, 1, "root", {}, trace.checkpoints)
        self.assertEqual(
            reasons(clean_window(reconcile_failures=1, attachments=[attachment]), known=True)[2], 0
        )


class LatencyTests(unittest.TestCase):
    def test_reports_the_actual_slow_frame_and_ping_values(self):
        blocking, failures, _ = reasons(
            clean_window(
                send_frame_fast_ms_p99=FAST_SEND_P99_MS + 1, ping_ms_p99=FAST_PING_P99_MS + 2
            )
        )
        self.assertEqual(blocking, [])
        self.assertIn(f"send_frame_fast_ms_p99: {FAST_SEND_P99_MS + 1:.3f} >", failures[0])
        self.assertIn(f"ping_ms_p99: {FAST_PING_P99_MS + 2:.3f} >", failures[1])

    def test_pong_only_is_permitted_for_a_predecessor_but_never_full_frame_evidence(
        self,
    ):
        legacy = clean_window(
            **{
                "send_frame_fast_n": 0,
                "send_frame_fast_unstamped": 40,
                "send_pong_fast_n": 10,
                "send_fast_negative": 0,
                "send_frame_fast_ms_p50": None,
                "send_frame_fast_ms_p99": None,
                "send_pong_fast_ms_p50": 0.25,
                "send_pong_fast_ms_p99": 0.75,
                "ping_ms_p99": 1.0,
            }
        )
        self.assertEqual(reasons(legacy, stamps=False), ([], [], 0))
        self.assertIn("0 stamped, 40 unstamped", reasons(legacy)[0][0])

    def test_missing_samples_and_partial_stamps_are_not_timing_observations(self):
        failures, timing, _ = reasons(clean_window(ping_ms_p99=None, send_frame_fast_unstamped=1))
        self.assertIn("fast text enqueue coverage: 10 stamped, 1 unstamped", failures)
        self.assertTrue(any("ping_ms_p99: no samples" in reason for reason in failures))
        self.assertEqual(timing, [])

    def test_incomplete_load_and_absent_backpressure_are_refusals(self):
        failures, _, _ = reasons(clean_window(request_failed=1, stalled_reader_backpressured=False))
        self.assertEqual(
            failures, ["incomplete offered load", "stalled reader never backpressured"]
        )

    def test_nested_load_excursion_is_inconclusive_with_the_measurement(self):
        async def overloaded(*_):
            with mocked("staging_soak_measure.os.getloadavg", return_value=(2.25, 1.0, 1.0)):
                try:
                    check_load(2.0)
                except RuntimeError as error:
                    raise ExceptionGroup("outer", [ExceptionGroup("tasks", [error])]) from error

        verdict = {}
        with mocked("staging_soak.exercise", side_effect=overloaded):
            self.assertEqual(run_exercise(None, verdict), 75)
        self.assertEqual(verdict["result"], "inconclusive")
        self.assertEqual(verdict["load_excursion"], {"measured": 2.25, "limit": 2.0})

    def test_not_yet_qualified_timing_is_observation_only_for_nightly(self):
        for nightly, qualified, expected in (
            (True, False, ("timing_observation", 75)),
            (True, True, ("fail", 1)),
            (False, False, ("fail", 1)),
        ):
            self.assertEqual(
                classify_cycle(
                    [], ["latency"], 0, nightly=nightly, timing_qualified=qualified, aa=False
                ),
                expected,
            )

    def test_stamp_decodes_rust_bit_order_and_all_space_zero(self):
        self.assertEqual(decode_stamp(b"{}\t" + b" " * 62 + b"\t"), 0x8000_0000_0000_0001)
        self.assertEqual(decode_stamp(b"{}" + b" " * 64), 0)
        self.assertIsNone(decode_stamp(b" " * 64))
        self.assertIsNone(decode_stamp(b"{}" + b" " * 65))
        self.assertIsNone(decode_stamp(b"{}" + b" " * 63 + b"\n"))
