"""No test binds a pinned OAuth loopback callback port.

Production browser logins bind exactly the redirect ports registered for the
public clients (Codex 1455/1457, xAI 56121). Tests bind an ephemeral port
through the manager's loopback seam, so parallel runs never contend for them.
These checks pin that seam and refuse a test that reaches a pinned port.
"""

import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
PINNED_PORT = re.compile(r"\b(?:1455|1457|56_?121)\b")
BIND_CALL = re.compile(r"\b(?:bind|bind_loopback_listener)\s*\(")
BEGIN_BROWSER_LOGIN = re.compile(r"\.begin_browser_login\s*\(")
CODEX_LOCAL_LOGIN_ARGS = re.compile(r'\[\s*"login"\s*,\s*"codex"\s*(?P<rest>[^\]]*)\]')
# The two managers guard every in-crate test with `cfg(test)`, and the facade
# switches its own unit tests to the ephemeral seam.
BROWSER_LOGIN_CALLERS = (
    "crates/providers/openai/src/native_auth/",
    "crates/providers/xai/src/native_auth",
    "crates/plakous/src/subscriptions.rs",
)
SCANNED_ROOTS = ("bin", "crates", "tests")


def rust_sources() -> list[Path]:
    return sorted(
        path
        for root in SCANNED_ROOTS
        for path in (ROOT / root).rglob("*.rs")
        if "target" not in path.relative_to(ROOT).parts
    )


def call_arguments(text: str, start: int) -> str:
    """Return the balanced argument text of the call whose `(` ends at `start`."""
    depth = 1
    index = start
    while index < len(text) and depth:
        depth += {"(": 1, ")": -1}.get(text[index], 0)
        index += 1
    return text[start:index - 1]


def pinned_port_binds(text: str) -> list[str]:
    return [
        arguments
        for match in BIND_CALL.finditer(text)
        if PINNED_PORT.search(arguments := call_arguments(text, match.end()))
    ]


def read(relative: str) -> str:
    return (ROOT / relative).read_text(encoding="utf-8")


class PinnedCallbackPortTests(unittest.TestCase):
    def test_no_source_binds_a_pinned_port_literal(self) -> None:
        findings = [
            f"{path.relative_to(ROOT)}: bind({arguments.strip()})"
            for path in rust_sources()
            for arguments in pinned_port_binds(path.read_text(encoding="utf-8"))
        ]
        self.assertEqual(findings, [])

    def test_browser_logins_begin_only_behind_the_guarded_seam(self) -> None:
        findings = []
        for path in rust_sources():
            relative = path.relative_to(ROOT).as_posix()
            if relative.startswith(BROWSER_LOGIN_CALLERS):
                continue
            if BEGIN_BROWSER_LOGIN.search(path.read_text(encoding="utf-8")):
                findings.append(relative)
        self.assertEqual(findings, [])

    def test_product_tests_never_spawn_a_local_codex_login(self) -> None:
        # In-process argument parsing is fine; a spawned `plak` binds for real.
        findings = [
            str(path.relative_to(ROOT))
            for path in rust_sources()
            if path.relative_to(ROOT).as_posix().startswith("bin/plak/tests/")
            for match in CODEX_LOCAL_LOGIN_ARGS.finditer(path.read_text(encoding="utf-8"))
            if '"--remote"' not in match.group("rest")
        ]
        self.assertEqual(findings, [])

    def test_listeners_bind_the_endpoint_seam_and_tests_guard_it(self) -> None:
        codex = read("crates/providers/openai/src/native_auth/login.rs")
        self.assertIn("bind_loopback_listener(self.endpoints.loopback_ports)", codex)
        self.assertIn("refuse_pinned_test_loopback(self.endpoints.loopback_ports);", codex)
        self.assertIn("ports, CODEX_OAUTH.loopback_ports,", codex)
        xai = read("crates/providers/xai/src/native_auth.rs")
        self.assertIn("bind_loopback_listener(self.core.endpoints.loopback_ports)", xai)
        self.assertIn("refuse_pinned_test_loopback(self.core.endpoints.loopback_ports);", xai)
        self.assertIn("ports, XAI_OAUTH.loopback_ports,", xai)
        facade = read("crates/plakous/src/subscriptions.rs")
        self.assertRegex(
            facade,
            r"#\[cfg\(test\)\]\s*\{\s*manager = manager\.with_ephemeral_loopback_for_tests\(\);",
        )

    def test_scanner_finds_pinned_binds_in_every_spelling(self) -> None:
        for source in (
            'TcpListener::bind(("127.0.0.1", 56_121)).await',
            'TcpListener::bind("127.0.0.1:1455").await',
            "bind_loopback_listener(&[1457]).await",
            'std::net::TcpListener::bind(format!("localhost:{}", 1455))',
        ):
            with self.subTest(source=source):
                self.assertEqual(len(pinned_port_binds(source)), 1)
        for source in (
            'TcpListener::bind(("127.0.0.1", 0)).await',
            "bind_loopback_listener(self.endpoints.loopback_ports)",
            'let port = 1455; listener.bind_ephemeral("127.0.0.1:0")',
        ):
            with self.subTest(source=source):
                self.assertEqual(pinned_port_binds(source), [])
        self.assertIsNotNone(CODEX_LOCAL_LOGIN_ARGS.search('["login", "codex"]'))
        remote = CODEX_LOCAL_LOGIN_ARGS.search('["login", "codex", "--remote"]')
        self.assertIn('"--remote"', remote.group("rest"))


if __name__ == "__main__":
    unittest.main()
