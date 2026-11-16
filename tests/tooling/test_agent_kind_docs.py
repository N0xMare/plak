"""Keep the JSONL command/event references aligned with their literal codecs."""

from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[2]


def read(path: str) -> str:
    return (ROOT / path).read_text(encoding="utf-8")


class AgentKindDocumentationTests(unittest.TestCase):
    def test_inbound_kinds_match_parser_help_and_crate_docs(self):
        parser = read("bin/plak/src/agent_jsonl.rs")
        arms = parser.split("match kind.as_str() {", 1)[1].split(
            "other => ParseOutcome::Diagnostic", 1
        )[0]
        expected = set(re.findall(r'^\s*"([a-z_]+)"\s*=>', arms, re.MULTILINE))
        self.assertTrue(expected)
        help_text = read("bin/plak/help/agent.txt")
        commands = help_text.split("COMMANDS (stdin):", 1)[1].split("EVENTS (stdout):", 1)[0]
        self.assertEqual(set(re.findall(r"\b[a-z_]+\b", commands)), expected)
        docs = read("crates/hosting/host-protocol/src/lib.rs")
        inbound = docs.split("# Inbound kinds", 1)[1].split("# Outbound kinds", 1)[0]
        documented = set()
        for line in inbound.splitlines():
            if not line.startswith("//! | "):
                continue
            cell = line.split("|", 2)[1].strip()
            if cell != "`kind`":
                documented.update(re.findall(r"`([a-z_]+)`", cell))
        self.assertEqual(documented, expected)

    def test_outbound_kinds_match_emitters_help_and_crate_docs(self):
        expected = set()
        for path in [
            "crates/hosting/host-protocol/src/jsonl.rs",
            "crates/hosting/host-protocol/src/envelope.rs",
            "bin/plak/src/agent_jsonl.rs",
        ]:
            expected.update(
                re.findall(
                    r'fields\.insert\(\s*"kind"\.(?:into|to_owned)\(\),\s*'
                    r'Value::from\("([a-z_]+)"\)',
                    read(path),
                )
            )
        self.assertTrue(expected)
        help_text = read("bin/plak/help/agent.txt")
        events = help_text.split("EVENTS (stdout):", 1)[1].split("NOTES:", 1)[0]
        self.assertEqual(set(re.findall(r"\b[a-z_]+\b", events)), expected)
        docs = read("crates/hosting/host-protocol/src/lib.rs")
        outbound = docs.split("# Outbound kinds", 1)[1].split("//! The handshake", 1)[0]
        self.assertEqual(set(re.findall(r"`([a-z_]+)`", outbound)), expected)
