"""Unit tests for the streamlined `tools/gate/check_workspace.py` policy checker."""

from __future__ import annotations

import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "tools/gate"))

import check_workspace as checker  # noqa: E402


class CheckWorkspacePolicyTests(unittest.TestCase):
    def test_live_workspace_passes(self) -> None:
        self.assertEqual(checker._check_workspace(ROOT), 0)

    def test_lints_forbid_unsafe(self) -> None:
        self.assertEqual(
            checker.validate_local_production_lints({"pkg": {"lints": {"workspace": True}}}),
            [],
        )
        errors = checker.validate_local_production_lints({"pkg": {}})
        self.assertEqual(len(errors), 1)
        self.assertIn("workspace lint inheritance", errors[0])

    def test_commit_trace_forbids_sql_returning(self) -> None:
        clean = checker.validate_commit_trace_sql(
            "plakous-store", {"src/sqlite.rs": 'let q = "INSERT INTO t VALUES (1)";'}
        )
        self.assertEqual(clean, [])
        bad = checker.validate_commit_trace_sql(
            "plakous-store",
            {"src/sqlite.rs": 'let q = "INSERT INTO t VALUES (1) RETURNING id";'},
        )
        self.assertEqual(len(bad), 1)
        self.assertIn("RETURNING", bad[0])
        test_only = checker.validate_commit_trace_sql(
            "plakous-store",
            {
                "src/sqlite.rs": (
                    '#[cfg(test)] mod tests { fn t() { let _ = "INSERT INTO t RETURNING id"; } }'
                )
            },
        )
        self.assertEqual(test_only, [])

    def test_neutral_sources_forbid_provider_names(self) -> None:
        self.assertEqual(
            checker.validate_neutral_sources("plakous-model", {"src/lib.rs": "pub struct Foo;"}),
            [],
        )
        bad = checker.validate_neutral_sources(
            "plakous-model", {"src/lib.rs": 'pub const PROVIDER: &str = "openai";'}
        )
        self.assertEqual(len(bad), 1)
        self.assertIn("openai", bad[0])

    def test_tracked_hygiene_catches_home_paths_and_private_names(self) -> None:
        self.assertEqual(checker.hygiene_findings("a.rs", b'let p = "/home/user/proj";'), [])
        bad_home = checker.hygiene_findings("a.rs", b'let p = "/ho' + b'me/alice/proj";')
        self.assertEqual(len(bad_home), 1)
        self.assertIn("/ho" + "me/alice", bad_home[0])
        bad_priv = checker.hygiene_findings(
            "a.rs", b"host = secretbox.local\n", frozenset({b"secretbox"})
        )
        self.assertEqual(len(bad_priv), 1)
        self.assertIn("secretbox", bad_priv[0])

    def test_claude_oauth_state_comparison_pin(self) -> None:
        good = """
        fn parse_claude_oauth_authorization_input(state: Option<&str>, expected_state: &str) -> Option<()> {
            if let Some(state) = state
                && !bool::from(state.as_bytes().ct_eq(expected_state.as_bytes()))
            {
                return None;
            }
            Some(())
        }
        """
        self.assertEqual(checker.validate_claude_oauth_state_comparison(good), [])
        bad = good.replace(
            "!bool::from(state.as_bytes().ct_eq(expected_state.as_bytes()))",
            "state != expected_state",
        )
        self.assertEqual(len(checker.validate_claude_oauth_state_comparison(bad)), 1)

    def test_cli_security_sources_forbid_proc_environ_and_unreviewed_sites(self) -> None:
        self.assertEqual(
            checker.validate_cli_security_sources("plakd", {"src/clean.rs": "pub fn ok() {}"}),
            [],
        )
        bad_proc = checker.validate_cli_security_sources(
            "plakd", {"src/bad.rs": 'let p = "/proc/self/environ";'}
        )
        self.assertTrue(any("/proc/self/environ" in e for e in bad_proc), bad_proc)

        bad_env = checker.validate_cli_security_sources(
            "plakd", {"src/bad.rs": 'use std::env; fn f() { let _ = env::var("X"); }'}
        )
        self.assertTrue(any("environment reader outside" in e for e in bad_env), bad_env)

        bad_proc_spawn = checker.validate_cli_security_sources(
            "plakd",
            {"src/bad.rs": 'use std::process::Command; fn f() { let _ = Command::new("sh"); }'},
        )
        self.assertTrue(any("std::process is outside" in e for e in bad_proc_spawn), bad_proc_spawn)

        bad_foreign = checker.validate_cli_security_sources(
            "plakd", {"src/bad.rs": 'extern "C" { fn getenv(name: *const i8) -> *const i8; }'}
        )
        self.assertTrue(any("foreign" in e for e in bad_foreign), bad_foreign)

        bad_chdir = checker.validate_cli_security_sources(
            "plakd", {"src/bad.rs": 'use std::env; fn f() { let _ = env::set_current_dir("/"); }'}
        )
        self.assertTrue(any("set_current_dir" in e for e in bad_chdir), bad_chdir)

        bad_delegated = checker.validate_cli_security_sources(
            "plakd", {"src/bad.rs": 'let _ = "--api-key"; let _ = CodexLoginStore;'}
        )
        self.assertGreaterEqual(len(bad_delegated), 2)

    def test_facade_only_sources_forbid_component_crates(self) -> None:
        self.assertEqual(
            checker.validate_facade_only_sources(
                "plakd",
                {"src/a.rs": "use plakous::model::Turn; use plakous_hosted::Host;"},
                permitted=("plakous-hosted",),
            ),
            [],
        )
        bad = checker.validate_facade_only_sources(
            "plakd",
            {"src/a.rs": "use plakous_model::Turn;"},
            permitted=("plakous-hosted",),
        )
        self.assertEqual(len(bad), 1)
        self.assertIn("plakous-model", bad[0])

    def test_package_local_includes_and_workspace_excludes(self) -> None:
        self.assertEqual(
            checker.validate_package_local_includes(
                "plakd", {"src/a.rs": 'const X: &str = include_str!("data/help.txt");'}
            ),
            [],
        )
        bad_inc = checker.validate_package_local_includes(
            "plakd", {"src/a.rs": 'const X: &str = include_str!("../../Cargo.toml");'}
        )
        self.assertEqual(len(bad_inc), 1)
        self.assertIn("outside the package root", bad_inc[0])

        self.assertEqual(
            checker.validate_workspace_excludes(
                {"exclude": [".plak", "tests/fixtures/facade-consumer"]}
            ),
            [],
        )
        missing_plak = checker.validate_workspace_excludes({"exclude": []})
        self.assertTrue(any(".plak" in e for e in missing_plak))
        unknown_ex = checker.validate_workspace_excludes({"exclude": [".plak", "crates/secret"]})
        self.assertTrue(any("crates/secret" in e for e in unknown_ex))

    def test_product_session_authority_and_home_discovery_guards(self) -> None:
        bad_auth = checker.validate_product_session_authority_sources(
            "plakd", {"src/x.rs": "fn maybe_compact_session() {}"}
        )
        self.assertEqual(len(bad_auth), 1)
        self.assertEqual(
            checker.validate_product_home_discovery("plakd", {"src/x.rs": "PlakPaths::from_env()"}),
            [],
        )
        bad_home = checker.validate_product_home_discovery(
            "plakous-hosted", {"src/x.rs": "PlakPaths::from_env()"}
        )
        self.assertEqual(len(bad_home), 1)

    def test_portable_closure_findings_flag_native_crates_and_tokio_features(self) -> None:
        nodes = [
            checker._TreeNode(0, "plakous-model", "0.0.4", (), False),
            checker._TreeNode(1, "serde_derive", "1.0.0", (), False, proc_macro=True),
            checker._TreeNode(2, "libc", "0.2.0", (), False),
            checker._TreeNode(1, "tokio", "1.0.0", ("sync", "net"), False),
            checker._TreeNode(1, "rusqlite", "0.35.0", (), False),
        ]
        findings = checker.closure_findings("plakous-model", "default", nodes)
        self.assertEqual(len(findings), 2, findings)
        self.assertTrue(any("tokio v1.0.0 feature net" in f for f in findings))
        self.assertTrue(any("rusqlite v0.35.0" in f for f in findings))
        self.assertFalse(any("libc" in f for f in findings))

    def test_supported_compile_fixture_validator_detects_manifest_and_file_drift(self) -> None:
        contract = {"manifest": {"package": {"name": "x"}}, "files": {"Cargo.toml", "src/main.rs"}}
        self.assertEqual(
            checker.validate_supported_compile_fixture(
                "x-consumer", {"package": {"name": "x"}}, {"Cargo.toml", "src/main.rs"}, contract
            ),
            [],
        )
        drift = checker.validate_supported_compile_fixture(
            "x-consumer", {"package": {"name": "y"}}, {"Cargo.toml"}, contract
        )
        self.assertEqual(len(drift), 2, drift)


if __name__ == "__main__":
    unittest.main()
