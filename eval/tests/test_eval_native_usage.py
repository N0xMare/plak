"""Canonical vendor accounting from retained, content-free native usage records."""

from __future__ import annotations

import hashlib
import json
from pathlib import Path
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "eval"))
from harbor_adapter.native_usage import extract_agent_usage  # noqa: E402
from harbor_adapter.pricing import from_harbor_context  # noqa: E402
from harbor_adapter.termination import load_json  # noqa: E402


class NativeUsageTests(unittest.TestCase):
    def test_pi_adds_cache_write_to_prompt_and_preserves_reasoning(self):
        with tempfile.TemporaryDirectory() as directory:
            agent = Path(directory)
            records = [
                {"type": "message_end", "message": {"role": "assistant", "usage": {
                    "input": 10, "cacheRead": 20, "cacheWrite": 5,
                    "output": 4, "reasoning": 2}}},
                {"type": "agent_end"},
            ]
            source = agent / "pi.txt"
            source.write_text("\n".join(json.dumps(r) for r in records) + "\n")
            usage = extract_agent_usage(agent, "pi")
            self.assertEqual((usage.input_tokens, usage.cache_read_tokens,
                              usage.cache_write_tokens, usage.output_tokens), (35, 20, 5, 4))
            self.assertEqual(usage.reasoning_tokens, 2)
            self.assertEqual(usage.status, "complete")
            self.assertEqual(usage.source_sha256, hashlib.sha256(source.read_bytes()).hexdigest())
            cost = from_harbor_context("gpt-6-luna", n_input_tokens=usage.input_tokens,
                n_cache_tokens=usage.cache_read_tokens, n_cache_write_tokens=usage.cache_write_tokens,
                n_output_tokens=usage.output_tokens)
            self.assertIsNotNone(cost.usd)

    def test_opencode_deduplicates_step_identity_and_counts_write(self):
        with tempfile.TemporaryDirectory() as directory:
            agent = Path(directory)
            event = {"type": "step_finish", "part": {"id": "one", "sessionID": "session",
                     "tokens": {"input": 10, "output": 4, "reasoning": 2,
                                "cache": {"read": 20, "write": 5}}}}
            (agent / "opencode.txt").write_text(json.dumps(event) + "\n" + json.dumps(event) + "\n")
            usage = extract_agent_usage(agent, "opencode")
            self.assertEqual(usage.usage_records, 1)
            self.assertEqual((usage.input_tokens, usage.cache_read_tokens,
                              usage.cache_write_tokens, usage.output_tokens), (35, 20, 5, 6))
            self.assertEqual(usage.reasoning_tokens, 2)
            self.assertEqual(usage.status, "complete")

    def test_opencode_requires_explicit_reasoning_bucket_even_when_output_is_present(self):
        with tempfile.TemporaryDirectory() as directory:
            agent = Path(directory)
            source = agent / "opencode.txt"
            for reasoning in (None, -1, True, "2"):
                tokens = {"input": 10, "output": 4,
                          "cache": {"read": 0, "write": 0}}
                if reasoning is not None:
                    tokens["reasoning"] = reasoning
                source.write_text(json.dumps({"type": "step_finish", "part": {
                    "id": "one", "sessionID": "session", "tokens": tokens}}) + "\n")
                usage = extract_agent_usage(agent, "opencode")
                self.assertIsNone(usage.output_tokens)
                self.assertIn("output", usage.missing_buckets)
                self.assertEqual(usage.status, "partial")
            tokens["reasoning"] = 0
            source.write_text(json.dumps({"type": "step_finish", "part": {
                "id": "one", "sessionID": "session", "tokens": tokens}}) + "\n")
            usage = extract_agent_usage(agent, "opencode")
            self.assertEqual((usage.output_tokens, usage.reasoning_tokens), (4, 0))
            self.assertEqual(usage.status, "complete")

    def test_opencode_trajectory_does_not_mislabel_visible_output_as_total(self):
        with tempfile.TemporaryDirectory() as directory:
            agent = Path(directory)
            (agent / "trajectory.json").write_text(json.dumps({
                "schema_version": "ATIF-v1.7",
                "final_metrics": {"total_prompt_tokens": 10,
                                  "total_cached_tokens": 0,
                                  "total_completion_tokens": 4},
                "steps": [{"metrics": {"extra": {"reasoning_tokens": 2}}}],
            }))
            usage = extract_agent_usage(agent, "opencode")
            self.assertIsNone(usage.output_tokens)
            self.assertIsNone(usage.reasoning_tokens)
            self.assertEqual(usage.status, "partial")

    def test_reasoning_cannot_exceed_canonical_completion_total(self):
        with tempfile.TemporaryDirectory() as directory:
            agent = Path(directory)
            (agent / "pi.txt").write_text(json.dumps({"type": "message_end", "message": {
                "role": "assistant", "usage": {"input": 1, "cacheRead": 0,
                                              "cacheWrite": 0, "output": 1,
                                              "reasoning": 2}}}) + "\n" +
                json.dumps({"type": "agent_end"}) + "\n")
            usage = extract_agent_usage(agent, "pi")
            self.assertEqual((usage.output_tokens, usage.reasoning_tokens), (1, 2))
            self.assertEqual(usage.status, "partial")

    def test_codex_missing_write_stays_partial(self):
        with tempfile.TemporaryDirectory() as directory:
            agent = Path(directory)
            (agent / "trajectory.json").write_text(json.dumps({"schema_version": "ATIF-v1.7", "final_metrics": {
                "total_prompt_tokens": 30, "total_cached_tokens": 10,
                "total_completion_tokens": 3, "extra": {"reasoning_output_tokens": 1}}}))
            usage = extract_agent_usage(agent, "codex")
            self.assertIsNone(usage.cache_write_tokens)
            self.assertEqual((usage.output_tokens, usage.reasoning_tokens), (3, 1))
            self.assertEqual(usage.status, "partial")
            self.assertIn("cache_write", usage.missing_buckets)

    def test_grok_terminal_metrics_supply_zero_write(self):
        with tempfile.TemporaryDirectory() as directory:
            agent = Path(directory)
            (agent / "trajectory.json").write_text(json.dumps({"schema_version": "ATIF-v1.7", "final_metrics": {
                "total_prompt_tokens": 30, "total_cached_tokens": 10,
                "total_completion_tokens": 3,
                "extra": {"total_cache_creation_input_tokens": 0,
                          "total_cache_read_input_tokens": 10,
                          "total_reasoning_tokens": 1}}}))
            usage = extract_agent_usage(agent, "grok-build")
            self.assertEqual(usage.cache_write_tokens, 0)
            self.assertEqual(usage.status, "complete")

    def test_malformed_native_usage_and_conflicting_duplicate_stay_partial(self):
        with tempfile.TemporaryDirectory() as directory:
            agent = Path(directory)
            (agent / "pi.txt").write_text(
                '{"type":"message_end","message":{"role":"assistant","usage":{"input":1,"output":1,"cacheRead":0,"cacheWrite":0}}}\n'
                '{"type":"message_end","message":\n'
                '{"type":"agent_end"}\n'
            )
            self.assertEqual(extract_agent_usage(agent, "pi").status, "partial")
            def event(write):
                return {"type": "step_finish", "part": {"id": "same", "sessionID": "s",
                    "tokens": {"input": 1, "output": 1, "reasoning": 0,
                               "cache": {"read": 0, "write": write}}}}
            (agent / "opencode.txt").write_text(json.dumps(event(0)) + "\n" + json.dumps(event(2)) + "\n")
            usage = extract_agent_usage(agent, "opencode")
            self.assertEqual(usage.status, "partial")
            self.assertEqual(usage.output_tokens, 1)

    def test_pmux_structured_usage_recovers_all_cache_buckets(self):
        with tempfile.TemporaryDirectory() as directory:
            agent = Path(directory)
            payload = {"usage": {"combined": {
                "input_tokens": 10, "cache_read_input_tokens": 20,
                "cache_creation_input_tokens": 5, "output_tokens": 4}}}
            source = agent / "pmux-stdout.txt"
            source.write_text(json.dumps(payload))
            usage = extract_agent_usage(agent, "pmux")
            self.assertEqual((usage.input_tokens, usage.cache_read_tokens,
                              usage.cache_write_tokens, usage.output_tokens), (35, 20, 5, 4))
            self.assertEqual(usage.status, "complete")
            self.assertEqual(usage.source_sha256, hashlib.sha256(source.read_bytes()).hexdigest())
            payload["usage"]["combined"].pop("cache_creation_input_tokens")
            source.write_text(json.dumps(payload))
            self.assertEqual(extract_agent_usage(agent, "pmux").status, "partial")

    def test_pmux_transcript_fallback_retains_counts_but_not_completeness(self):
        with tempfile.TemporaryDirectory() as directory:
            agent = Path(directory)
            transcripts = agent / "claude-transcripts"
            transcripts.mkdir()
            (transcripts / "one.jsonl").write_text(json.dumps({
                "type": "assistant", "message": {"id": "m1", "usage": {
                    "input_tokens": 10, "cache_read_input_tokens": 20,
                    "cache_creation_input_tokens": 5, "output_tokens": 4}}}) + "\n")
            usage = extract_agent_usage(agent, "pmux")
            self.assertEqual(usage.input_tokens, 35)
            self.assertEqual(usage.status, "partial")
            self.assertEqual(usage.schema, "claude.assistant.message.usage.jsonl")

    def test_local_archives_match_stock_harbor_core_counts(self):
        root = ROOT / ".plak/harbor/jobs"
        if not root.is_dir():
            self.skipTest("no archived jobs")
        for marker, harness in (("ab-codex", "codex"), ("ab-grok-build", "grok-build"),
                                ("ab-pi-openrouter", "pi"), ("ab-opencode-openrouter", "opencode")):
            trial = next(root.glob(f"{marker}/*/*/result.json"), None)
            if trial is None:
                self.skipTest("representative archive missing")
            result = load_json(trial).data
            native = extract_agent_usage(trial.parent / "agent", harness)
            stock = result["agent_result"]
            self.assertGreater(native.usage_records, 0, marker)
            expected_input = native.input_tokens
            if harness in ("pi", "opencode") and expected_input is not None:
                expected_input -= native.cache_write_tokens or 0
            self.assertEqual(stock["n_input_tokens"], expected_input, marker)
            self.assertEqual(stock["n_cache_tokens"], native.cache_read_tokens, marker)
            if harness == "opencode":
                # Pinned Harbor's OpenCode converter repeats the client's
                # visible-output subtotal; canonical completion adds reasoning.
                self.assertIsNotNone(native.reasoning_tokens)
                self.assertEqual(stock["n_output_tokens"] + native.reasoning_tokens,
                                 native.output_tokens, marker)
            else:
                self.assertEqual(stock["n_output_tokens"], native.output_tokens, marker)


if __name__ == "__main__":
    unittest.main()
