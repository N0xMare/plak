"""Tests for slim structure gates: layer policy, workspace edges, and feature closures."""

from __future__ import annotations

import copy
import importlib.util
import re
import sys
import subprocess
import tempfile
import tomllib
import unittest
from unittest.mock import patch
from dataclasses import replace
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "tools/checks"))


def load_module(name: str, path: Path):  # type: ignore[no-untyped-def]
    """Import one repository script without making scripts a package."""
    spec = importlib.util.spec_from_file_location(name, path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot load {path}")
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


policy_module = load_module("plakous_policy", ROOT / "tools/checks/plakous_policy.py")
workspace_policy = load_module("check_workspace", ROOT / "tools/checks/check_workspace.py")
feature_policy = load_module("check_features", ROOT / "tools/checks/check_features.py")
density_policy = load_module("check_prod_loc", ROOT / "tools/checks/check_prod_loc.py")


class WorkspaceExclusionTests(unittest.TestCase):
    def test_operator_state_must_be_excluded(self):
        self.assertEqual(workspace_policy.validate_workspace_excludes({'exclude': []}, set()),
                         ['workspace must exclude .plak operator state and fork worktrees'])

    def test_operator_state_and_reviewed_packages_are_allowed(self):
        manifest = {'exclude': ['.plak', 'vendor/monty-typeshed', 'tests/fixtures/example']}
        self.assertEqual(workspace_policy.validate_workspace_excludes(
            manifest, {'tests/fixtures/example'}), [])

    def test_other_nested_packages_remain_forbidden(self):
        findings = workspace_policy.validate_workspace_excludes(
            {'exclude': ['.plak', 'hidden-package']}, set())
        self.assertEqual(findings, [
            "workspace excludes unsupported nested packages: ['hidden-package']",
        ])


class TrackedHygieneTests(unittest.TestCase):
    # Synthetic private names; the real list is an untracked local file.
    HOST = "privhost1"
    TOOL = "lane-wait.sh"
    PRIVATE = frozenset(
        name.encode() for name in (HOST, "privhost-dev", "secret-project", TOOL, "deploy.sh")
    )

    def findings(self, text: str) -> list[str]:
        return workspace_policy.hygiene_findings("doc.md", text.encode(), self.PRIVATE)

    def test_absolute_home_paths_are_refused(self):
        for text in ("see /Use" "rs/alice/dev/plak", "cd /ho" "me/alice/src", "`/ho" "me/alice`"):
            with self.subTest(text=text):
                findings = self.findings(text)
                self.assertEqual(len(findings), 1)
                self.assertIn("doc.md:1: absolute home path", findings[0])

    def test_placeholder_users_and_relative_home_segments_pass(self):
        for text in (
            "/home/user/project",
            "/home/you/.plak",
            'PMUX_CELL_HOME = "/home/pmux"',
            '"$root/home/agents/lead.toml"',
            "/tmp/home/secrets/token",
            "for home in /root /home/*; do",
        ):
            with self.subTest(text=text):
                self.assertEqual(self.findings(text), [])

    def test_private_names_are_refused(self):
        for text in (
            f"{self.TOOL} x1 {self.HOST}",
            "on privhost-dev.",
            "the secret-project tree",
            "ssh privhost1.local",
            "scripts/deploy.sh",
        ):
            with self.subTest(text=text):
                self.assertTrue(self.findings(text))
        findings = self.findings(f"{self.TOOL} x1 {self.HOST}")
        self.assertEqual(findings, [
            f"doc.md:1: private host or tool name {self.TOOL!r}",
            f"doc.md:1: private host or tool name {self.HOST!r}",
        ])

    def test_lookalike_words_and_binary_files_pass(self):
        for text in ("aprivhost1", f"{self.HOST}x", "redeploy.sh", "my-deploy.sh", "privhost3", "privhost1-b"):
            with self.subTest(text=text):
                self.assertEqual(self.findings(text), [])
        binary = b"\0" + self.HOST.encode()
        self.assertEqual(workspace_policy.hygiene_findings("blob.bin", binary, self.PRIVATE), [])

    def test_private_names_load_from_untracked_local_file(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.assertEqual(workspace_policy.load_hygiene_private_names(root), frozenset())
            names_file = root / workspace_policy.HYGIENE_PRIVATE_NAMES_FILE
            names_file.parent.mkdir(parents=True)
            names_file.write_text(f"{self.HOST}\n\n  {self.TOOL}  \n")
            self.assertEqual(
                workspace_policy.load_hygiene_private_names(root),
                frozenset({self.HOST.encode(), self.TOOL.encode()}),
            )
            (root / "leak.md").write_text(f"run {self.TOOL}\n")
            with patch.object(workspace_policy, "hygiene_candidate_files", return_value=["leak.md"]):
                errors = workspace_policy.validate_tracked_hygiene(root)
        self.assertEqual(len(errors), 1)
        self.assertTrue(errors[0].startswith("leak.md:1: private host or tool name"))

    def test_without_local_names_file_only_home_paths_are_checked(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "doc.md").write_text(f"{self.HOST}\ncd /ho" "me/alice\n")
            with patch.object(workspace_policy, "hygiene_candidate_files", return_value=["doc.md"]):
                errors = workspace_policy.validate_tracked_hygiene(root)
        self.assertEqual(len(errors), 1)
        self.assertTrue(errors[0].startswith("doc.md:2: absolute home path"))

    def test_tree_carries_no_private_name_digests(self):
        source = (Path(workspace_policy.__file__)).read_text()
        self.assertNotIn("DIGESTS", source)
        self.assertIsNone(re.search(r'"[0-9a-f]{64}"', source))

    def test_validation_reports_line_in_every_candidate_file(self):
        formerly_pending = "bin/plak/src/carry.rs"
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            for relative in ("clean.md", "leak.md", formerly_pending):
                (root / relative).parent.mkdir(parents=True, exist_ok=True)
            (root / "clean.md").write_text("nothing here\n")
            (root / "leak.md").write_text(f"ok\nrun {self.TOOL}\n")
            (root / formerly_pending).write_text(f"{self.HOST}\n")
            files = ["clean.md", "leak.md", formerly_pending]
            with patch.object(workspace_policy, "hygiene_candidate_files", return_value=files):
                errors = workspace_policy.validate_tracked_hygiene(root, self.PRIVATE)
        self.assertEqual(len(errors), 2)
        self.assertTrue(errors[0].startswith("leak.md:2: private host or tool name"))
        self.assertTrue(errors[1].startswith(f"{formerly_pending}:1: private host or tool name"))


class CommitTraceEdgeTests(unittest.TestCase):
    def test_traced_stores_reject_sql_returning_case_insensitively(self):
        for package, prefix in workspace_policy.COMMIT_TRACE_SOURCE_PREFIXES.items():
            with self.subTest(package=package):
                errors = workspace_policy.validate_commit_trace_sql(
                    package, {prefix + "database.rs": 'fn write() { let sql = r#"INSERT INTO rows DEFAULT VALUES rEtUrNiNg id"#; }'}
                )
                self.assertEqual(len(errors), 1)
                self.assertIn("Commit trace contract", errors[0])
                self.assertIn("spec 05 section 2.6", errors[0])

    def test_returning_in_comments_and_test_code_is_not_a_store_sql_clause(self):
        source = '''
        // RETURNING is unsupported.
        /* returning */
        fn write() { let sql = "INSERT INTO rows DEFAULT VALUES"; }
        #[cfg(test)]
        mod tests { const SQL: &str = "INSERT INTO rows DEFAULT VALUES RETURNING id"; }
        '''
        self.assertEqual(workspace_policy.validate_commit_trace_sql("plakous-store", {"src/sqlite.rs": source}), [])

    def test_tree_trace_edge_requires_optional_dependency_and_exact_gate(self):
        tree = {
            "dependencies": {"plakous-store": {"optional": True}},
            "features": {"commit-trace": ["dep:plakous-store", "plakous-store/commit-trace"]},
        }
        manifests = {"plakous-recursive-subagents-store": tree}
        self.assertEqual(workspace_policy.validate_commit_trace_edge(manifests), [])
        tree["dependencies"]["plakous-store"]["optional"] = False
        self.assertTrue(workspace_policy.validate_commit_trace_edge(manifests))
        tree["dependencies"]["plakous-store"]["optional"] = True
        tree["features"]["commit-trace"] = []
        self.assertTrue(workspace_policy.validate_commit_trace_edge(manifests))


class RustLexCacheTests(unittest.TestCase):
    def test_cached_tokens_are_independent_and_keyed_by_exact_source(self):
        source = 'use plakous::Agent; // not code\n'
        workspace_policy._cached_rust_tokens.cache_clear()
        expected = workspace_policy.rust_tokens(source)
        first = workspace_policy.rust_tokens(source)
        first.clear()
        self.assertEqual(workspace_policy.rust_tokens(source), expected)
        self.assertGreaterEqual(workspace_policy._cached_rust_tokens.cache_info().hits, 2)
        self.assertNotEqual(workspace_policy.rust_tokens(source + 'fn added() {}'), expected)

    def test_blanking_cache_matches_uncached_output_and_is_content_keyed(self):
        source = 'fn f() { let s = r#"not code"#; /* comment */ }'
        workspace_policy._blank_rust_non_code.cache_clear()
        blank = workspace_policy._blank_rust_non_code(source)
        self.assertEqual(blank, workspace_policy._blank_rust_non_code.__wrapped__(source))
        self.assertEqual(blank, workspace_policy._blank_rust_non_code(source))
        self.assertEqual(workspace_policy._blank_rust_non_code.cache_info().hits, 1)
        self.assertNotEqual(workspace_policy._blank_rust_non_code(source + '\nfn added() {}'), blank)

Package = policy_module.Package
Policy = policy_module.Policy
Workspace = policy_module.Workspace


def synthetic_package(
    name: str,
    layer: str,
    directory: str,
    *,
    package_class: str = "published-internal",
    neutral: bool = False,
    dependencies: tuple[str, ...] = (),
    dev_dependencies: tuple[str, ...] = (),
    features: dict[str, list[str]] | None = None,
) -> Package:
    manifest = {
        "package": {
            "name": name,
            "metadata": {"plakous": {"layer": layer, "class": package_class, "neutral": neutral}},
        },
        "dependencies": {dependency: {"path": "../x"} for dependency in dependencies},
        "features": features or {},
        "lints": {"workspace": True},
    }
    metadata = {
        "name": name,
        "dependencies": [{"name": dependency, "kind": None} for dependency in dependencies]
        + [{"name": dependency, "kind": "dev"} for dependency in dev_dependencies],
        "features": features or {},
        "targets": [],
    }
    return Package(
        name=name,
        manifest_path=ROOT / directory / "Cargo.toml",
        directory=directory,
        layer=layer,
        package_class=package_class,
        neutral=neutral,
        manifest=manifest,
        metadata=metadata,
    )


def synthetic_policy(**overrides: object) -> Policy:
    base = {
        "layers": ("core", "common", "facade", "host", "bin", "test"),
        "edges": {
            "core": ("core",),
            "common": ("common",),
            "facade": ("core", "common"),
            "host": ("facade", "host"),
            "bin": ("facade", "host"),
            "test": ("*",),
        },
        "edge_exceptions": {},
        "layout": {
            "core": ("crates/plakous/{short}",),
            "common": ("crates/plakous/common/{short}",),
            "facade": ("crates/plakous/plakous",),
            "host": ("crates/plak/{short}",),
            "bin": ("crates/plak/{short}",),
            "test": ("tests/**",),
        },
        "classes": {
            "core": ("supported",),
            "common": ("published-internal",),
            "facade": ("supported",),
            "host": ("published-internal",),
            "bin": ("supported", "published-internal"),
            "test": ("repository-only",),
        },
        "production_dependencies": {},
        "facade_features": {},
        "modules": {},
    }
    base.update(overrides)
    return Policy(**base)  # type: ignore[arg-type]


def synthetic_workspace(*packages: Package, policy: Policy | None = None) -> Workspace:
    return Workspace(
        root=ROOT,
        policy=policy or synthetic_policy(),
        packages={package.name: package for package in packages},
        root_manifest={},
    )


def small_tree() -> list[Package]:
    return [
        synthetic_package(
            "plakous-model", "core", "crates/plakous/model", package_class="supported", neutral=True
        ),
        synthetic_package(
            "plakous-agent",
            "core",
            "crates/plakous/agent",
            package_class="supported",
            neutral=True,
            dependencies=("plakous-model",),
        ),
        synthetic_package("plakous-http", "common", "crates/plakous/common/http"),
        synthetic_package(
            "plakous",
            "facade",
            "crates/plakous/plakous",
            package_class="supported",
            dependencies=("plakous-agent", "plakous-model", "plakous-http"),
        ),
        synthetic_package(
            "plakous-hosted", "host", "crates/plak/hosted", dependencies=("plakous",)
        ),
        synthetic_package(
            "plak",
            "bin",
            "crates/plak/plak",
            package_class="supported",
            dependencies=("plakous", "plakous-hosted"),
        ),
    ]


class LayerPolicyTests(unittest.TestCase):
    """The policy-derived validators over synthetic workspaces."""

    def test_clean_synthetic_workspace_passes_every_layer_validator(self) -> None:
        workspace = synthetic_workspace(*small_tree())
        for validator in (
            workspace_policy.validate_package_metadata,
            workspace_policy.validate_layout,
            workspace_policy.validate_layer_edges,
            workspace_policy.validate_neutral_dependencies,
        ):
            with self.subTest(validator=validator.__name__):
                self.assertEqual(validator(workspace), [])

    def test_matrix_violation_is_detected(self) -> None:
        packages = small_tree()
        # A core crate reaching a common crate is outside the matrix.
        packages[1] = replace(
            packages[1],
            metadata={
                **packages[1].metadata,
                "dependencies": [
                    {"name": "plakous-model", "kind": None},
                    {"name": "plakous-http", "kind": None},
                ],
            },
        )
        errors = workspace_policy.validate_layer_edges(synthetic_workspace(*packages))
        self.assertEqual(len(errors), 1, errors)
        self.assertIn("plakous-agent (core) -> plakous-http (common)", errors[0])
        self.assertIn("no edge-exception", errors[0])

    def test_dev_dependencies_are_not_layer_edges(self) -> None:
        packages = small_tree()
        packages[0] = replace(
            packages[0],
            metadata={
                **packages[0].metadata,
                "dependencies": [{"name": "plak", "kind": "dev"}],
            },
        )
        self.assertEqual(
            workspace_policy.validate_layer_edges(synthetic_workspace(*packages)), []
        )

    def test_edge_exception_admits_the_edge_and_stale_exceptions_fail(self) -> None:
        packages = small_tree()
        packages[1] = replace(
            packages[1],
            metadata={
                **packages[1].metadata,
                "dependencies": [
                    {"name": "plakous-model", "kind": None},
                    {"name": "plakous-http", "kind": None},
                ],
            },
        )
        policy = synthetic_policy(edge_exceptions={"plakous-agent": ("plakous-http",)})
        self.assertEqual(
            workspace_policy.validate_layer_edges(synthetic_workspace(*packages, policy=policy)),
            [],
        )
        stale = synthetic_policy(
            edge_exceptions={"plakous-model": ("plakous-http",), "ghost": ("plakous",)}
        )
        errors = workspace_policy.validate_layer_edges(
            synthetic_workspace(*small_tree(), policy=stale)
        )
        self.assertTrue(any("is stale" in error for error in errors), errors)
        self.assertTrue(any("unknown package 'ghost'" in error for error in errors), errors)

    def test_core_package_restriction_admits_only_the_named_package(self) -> None:
        policy = synthetic_policy(
            edges={
                "core": ("core",),
                "common": ("core:plakous-model",),
                "facade": ("core", "common"),
                "host": ("facade", "host"),
                "bin": ("facade", "host"),
                "test": ("*",),
            }
        )
        packages = small_tree()
        packages[2] = replace(
            packages[2],
            metadata={
                **packages[2].metadata,
                "dependencies": [{"name": "plakous-model", "kind": None}],
            },
        )
        self.assertEqual(
            workspace_policy.validate_layer_edges(synthetic_workspace(*packages, policy=policy)),
            [],
        )
        packages[2] = replace(
            packages[2],
            metadata={
                **packages[2].metadata,
                "dependencies": [{"name": "plakous-agent", "kind": None}],
            },
        )
        errors = workspace_policy.validate_layer_edges(
            synthetic_workspace(*packages, policy=policy)
        )
        self.assertTrue(any("plakous-http (common) -> plakous-agent (core)" in e for e in errors))

    def test_layout_template_mismatch_is_detected(self) -> None:
        packages = small_tree()
        packages[2] = replace(packages[2], directory="crates/plakous/http")
        errors = workspace_policy.validate_layout(synthetic_workspace(*packages))
        self.assertEqual(len(errors), 1, errors)
        self.assertIn("plakous-http", errors[0])
        self.assertIn("crates/plakous/common/http", errors[0])

    def test_layout_templates_expand_short_and_name_and_globs(self) -> None:
        package = synthetic_package("plak-tui", "bin", "crates/plak/tui")
        self.assertEqual(package.short, "tui")
        for template, expected in (
            ("crates/plak/{short}", True),
            ("bin/{name}", False),
            ("crates/plak/**", True),
            ("crates/**", True),
            ("tests/**", False),
        ):
            with self.subTest(template=template):
                policy = synthetic_policy(
                    layout={**synthetic_policy().layout, "bin": (template,)}
                )
                self.assertEqual(policy_module.layout_matches(policy, package), expected)
        facade = synthetic_package("plakous", "facade", "crates/plakous")
        self.assertEqual(facade.short, "plakous")
        policy = synthetic_policy(layout={**synthetic_policy().layout, "facade": ("crates/plakous",)})
        self.assertTrue(policy_module.layout_matches(policy, facade))

    def test_classification_is_derived_and_illegal_classes_fail(self) -> None:
        workspace = synthetic_workspace(*small_tree())
        classes = workspace_policy.package_classifications(workspace)
        self.assertEqual(classes["supported"], {"plakous-model", "plakous-agent", "plakous", "plak"})
        self.assertEqual(classes["published-internal"], {"plakous-http", "plakous-hosted"})
        self.assertEqual(classes["repository-only"], set())
        packages = small_tree()
        packages[2] = replace(packages[2], package_class="supported")
        errors = workspace_policy.validate_package_metadata(synthetic_workspace(*packages))
        self.assertTrue(any("not legal for layer 'common'" in error for error in errors), errors)
        packages[2] = replace(packages[2], package_class="consolidate")
        errors = workspace_policy.validate_package_metadata(synthetic_workspace(*packages))
        self.assertTrue(any("unknown class" in error for error in errors), errors)
        packages[2] = replace(packages[2], package_class="published-internal", layer="mystery")
        errors = workspace_policy.validate_package_metadata(synthetic_workspace(*packages))
        self.assertTrue(any("unknown layer" in error for error in errors), errors)

    def test_neutral_package_rejects_provider_dependencies(self) -> None:
        packages = small_tree()
        packages[0] = replace(
            packages[0],
            metadata={
                **packages[0].metadata,
                "dependencies": [{"name": "async-openai", "kind": "dev"}],
            },
        )
        errors = workspace_policy.validate_neutral_dependencies(synthetic_workspace(*packages))
        self.assertEqual(errors, ["plakous-model: provider dependency forbidden: async-openai"])

    def test_neutral_source_rule_flags_provider_names_except_exemptions(self) -> None:
        sources = {
            "src/lib.rs": 'const ROUTE: &str = "openai.responses";',
            "src/clean.rs": "// anthropic in a comment only\nfn neutral() {}",
        }
        errors = workspace_policy.validate_neutral_sources("plakous-model", sources)
        self.assertEqual(
            errors, ["plakous-model:src/lib.rs: provider name 'openai' in neutral package"]
        )

    def test_production_dependency_pins_are_exact_and_layer_bound(self) -> None:
        policy = synthetic_policy(
            production_dependencies={
                "plakous-hosted": ("plakous",),
                "plak": ("plakous", "plakous-hosted"),
                "plakous-http": (),
            }
        )
        errors = workspace_policy.validate_production_dependencies(
            synthetic_workspace(*small_tree(), policy=policy)
        )
        self.assertEqual(
            errors,
            ["production-dependencies row for 'plakous-http' (layer 'common') is not a host/bin package"],
        )
        missing = synthetic_policy(production_dependencies={"plak": ("plakous",)})
        errors = workspace_policy.validate_production_dependencies(
            synthetic_workspace(*small_tree(), policy=missing)
        )
        self.assertTrue(any("plakous-hosted" in e and "need a" in e for e in errors), errors)
        self.assertTrue(any("plak: direct dependencies" in e for e in errors), errors)

    def test_facade_feature_pins_apply_to_every_host_bin_facade_edge(self) -> None:
        packages = small_tree()
        host = packages[4]
        host_metadata = copy.deepcopy(host.metadata)
        host_metadata["dependencies"][0].update(
            {"uses_default_features": False, "features": ["sessions-sqlite"]}
        )
        packages[4] = replace(host, metadata=host_metadata)
        policy = synthetic_policy(
            facade_features={"plakous-hosted": ("sessions-sqlite",), "plak": ("sessions-sqlite",)}
        )
        errors = workspace_policy.validate_facade_features(
            synthetic_workspace(*packages, policy=policy)
        )
        self.assertEqual(
            errors,
            ["plak must enable exactly plakous/{sessions-sqlite} with dependency defaults disabled"],
        )
        errors = workspace_policy.validate_facade_features(
            synthetic_workspace(*packages, policy=synthetic_policy())
        )
        self.assertTrue(all("without a" in error for error in errors), errors)
        self.assertEqual(len(errors), 2)

    def test_grouped_facade_imports_are_checked_as_separate_paths(self) -> None:
        sources = {"src/jobs.rs": "use plakous::tools::{JobStatus, JobView};"}
        self.assertEqual(
            workspace_policy.validate_cli_security_sources("plakous-hosted", sources),
            [],
        )
        sources["src/jobs.rs"] = "use plakous::tools::{JobStatus, NotApproved};"
        errors = workspace_policy.validate_cli_security_sources("plakous-hosted", sources)
        self.assertTrue(any("NotApproved" in error for error in errors), errors)

    def test_non_facade_globs_do_not_trigger_facade_symbol_checks(self) -> None:
        sources = {
            "src/globs.rs": """
                use super::*;
                use plakous_hosted::sessions::*;
                use plakous_host_protocol::topology::*;
                use {plakous::tools::JobStatus, plakous_host_protocol::topology::*};
            """,
        }
        self.assertEqual(
            workspace_policy.validate_cli_security_sources("plakous-hosted", sources),
            [],
        )

    def test_grouped_facade_globs_are_rejected_at_their_own_path(self) -> None:
        sources = {
            "src/globs.rs": "use {plakous_host_protocol::topology::*, plakous::{tools::JobStatus, tools::*}};",
        }
        errors = workspace_policy.validate_cli_security_sources("plakous-hosted", sources)
        self.assertEqual(len(errors), 1, errors)
        self.assertIn("tools::*", errors[0])
        self.assertNotIn("topology", errors[0])

    def test_tools_job_stop_reason_is_enrolled(self) -> None:
        self.assertEqual(
            workspace_policy.validate_cli_security_sources(
                "plakous-hosted",
                {"src/jobs.rs": "use plakous::tools::{JobStatus, JobStopReason, JobView};"},
            ),
            [],
        )

    def test_facade_only_symbol_rule(self) -> None:
        sources = {
            "src/lib.rs": "use plakous::agent::Agent;\nfn f() { plakous::VERSION; }",
            "src/leak.rs": "use plakous_model::ModelId;\nfn g() { plakous_tools::x(); }",
            "src/field.rs": "fn h(r: &R) { r.plakous_locator; let plakous_thing = 1; }",
            "src/nested.rs": "use {plakous_agent::Agent, std::io};",
        }
        errors = workspace_policy.validate_facade_only_sources("plakous-hosted", sources)
        self.assertEqual(
            errors,
            [
                "plakous-hosted:src/leak.rs: names Plakous component crates ['plakous-model', "
                "'plakous-tools']; host/bin sources reach L0 only through `plakous`",
                "plakous-hosted:src/nested.rs: names Plakous component crates ['plakous-agent']; "
                "host/bin sources reach L0 only through `plakous`",
            ],
        )
        self.assertEqual(
            workspace_policy.validate_facade_only_sources(
                "plakous-hosted",
                {"src/js_host.rs": "use plakous_host_protocol::envelope::SCHEMA_V;"},
                ("plakous-host-protocol",),
            ),
            [],
        )

    def test_module_rules_apply_per_file_once_the_module_exists(self) -> None:
        package = synthetic_package("plak", "bin", "bin/plak", package_class="supported")
        rules = {
            "tui": {
                "forbid-crates": ("plakous", "plakous-*"),
                "allow-crates": ("plakous-host-protocol",),
                "may-use": ("crate::tui", "crate::config::SettingsSnapshot"),
            }
        }
        sources = {
            "src/main.rs": "use plakous::agent::Agent;",
            "src/tui/mod.rs": "use std::io;\nuse ratatui::Frame;\nuse crate::tui::theme::Theme;\nuse crate::config::SettingsSnapshot;\nuse plakous_host_protocol::SessionIntent;",
            "src/tui/bad.rs": "use plakous::agent::Agent;\nuse crate::runtime::Host;\nfn f() { plakous_model::x(); }",
        }
        errors = workspace_policy.validate_module_rules(package, sources, rules)
        self.assertEqual(
            errors,
            [
                "plak:src/tui/bad.rs: module 'tui' may not use crate 'plakous'",
                "plak:src/tui/bad.rs: module 'tui' may not use 'crate::runtime::Host'",
                "plak:src/tui/bad.rs: module 'tui' names forbidden crate 'plakous-model'",
            ],
        )
        self.assertEqual(
            workspace_policy.validate_module_rules(package, {"src/main.rs": sources["src/main.rs"]}, rules),
            [],
        )

    def test_module_rules_see_through_brace_groups_and_bare_paths(self) -> None:
        """A brace group is many paths, and a bare `plakous_hosted::` path is a use."""
        package = synthetic_package("plak", "bin", "bin/plak", package_class="supported")
        rules = {
            "tui": {
                "forbid-crates": ("plakous", "plakous-*", "plakous-hosted"),
                "allow-crates": ("plakous-host-protocol",),
                "may-use": ("crate::tui", "crate::config::SettingsSnapshot"),
            }
        }
        sources = {
            # Flattened, this reads as `crate::tui::X::shell::Y` and passes the
            # `crate::tui` prefix; expanded, `crate::shell::Y` is refused.
            "src/tui/braces.rs": "use crate::{tui::X, shell::Y};",
            # No `use` declares this one.
            "src/tui/bare.rs": "fn f() { plakous_hosted::surface_protocol::x(); }",
            "src/tui/door.rs": (
                "use crate::{config::SettingsSnapshot, tui::theme::Theme};\n"
                "use plakous_host_protocol::{SessionIntent, views::presentation::UsageVertical};\n"
                "fn g() { crate::tui::theme::dark(); }"
            ),
        }
        self.assertEqual(
            workspace_policy.validate_module_rules(package, sources, rules),
            [
                "plak:src/tui/bare.rs: module 'tui' names forbidden crate 'plakous-hosted'",
                "plak:src/tui/braces.rs: module 'tui' may not use 'crate::shell::Y'",
            ],
        )

    def test_module_rules_refuse_a_forbidden_crate_inside_a_use_brace_group(self) -> None:
        """`use {plakous_model::a, std::io}` names a forbidden crate once."""
        package = synthetic_package("plak", "bin", "bin/plak", package_class="supported")
        rules = {
            "tui": {
                "forbid-crates": ("plakous", "plakous-*", "plakous-hosted"),
                "allow-crates": ("plakous-host-protocol",),
                "may-use": ("crate::tui",),
            }
        }
        sources = {"src/tui/mix.rs": "use {plakous_model::ModelId, std::io};"}
        # Reported once, by the `use` arm: the bare-path scan skips `use` trees
        # so one import never produces two errors.
        self.assertEqual(
            workspace_policy.validate_module_rules(package, sources, rules),
            ["plak:src/tui/mix.rs: module 'tui' may not use crate 'plakous_model'"],
        )

    def test_policy_loader_rejects_malformed_tables(self) -> None:
        for manifest, message in (
            ({}, "lacks"),
            ({"workspace": {"metadata": {"plakous": {"layers": ["a"], "edges": {"b": []}, "layout": {"a": []}, "classes": {"a": []}}}}}, "unknown layers"),
            ({"workspace": {"metadata": {"plakous": {"layers": ["a"], "edges": {"a": ["zzz"]}, "layout": {"a": []}, "classes": {"a": []}}}}}, "unknown layer 'zzz'"),
            ({"workspace": {"metadata": {"plakous": {"layers": ["a"], "edges": {"a": []}, "layout": {"a": []}, "classes": {"a": ["mystery"]}}}}}, "unknown classes"),
        ):
            with self.subTest(message=message):
                with self.assertRaisesRegex(ValueError, message):
                    policy_module.load_policy(manifest)
        with self.assertRaisesRegex(ValueError, "lacks \\[package.metadata.plakous\\]"):
            policy_module.package_metadata({"package": {}}, "x")
        with self.assertRaisesRegex(ValueError, "unknown"):
            policy_module.package_metadata(
                {"package": {"metadata": {"plakous": {"layer": "core", "class": "supported", "tier": 1}}}},
                "x",
            )


class CargoTreeDiagnosticsTests(unittest.TestCase):
    def test_rquickjs_checks_report_cargo_failures_not_missing_linkage(self):
        failed = subprocess.CompletedProcess([], 101, '',
                                             'error: monty-pool =1.1.0 unavailable offline\n')
        with patch.object(workspace_policy.subprocess, 'run', return_value=failed):
            errors = workspace_policy.rquickjs_linkage_errors(ROOT)
        self.assertEqual(len(errors), 4)
        for error in errors:
            self.assertIn('cargo tree --offline', error)
            self.assertIn('exit 101', error)
            self.assertIn('monty-pool =1.1.0 unavailable offline', error)
            self.assertNotIn('must link', error)

    def test_rquickjs_success_uses_stdout_not_diagnostics(self):
        absent = subprocess.CompletedProcess([], 0, '', 'warning: rquickjs not reachable')
        worker = subprocess.CompletedProcess([], 0, 'rquickjs v0.11.0\n', '')
        with patch.object(workspace_policy.subprocess, 'run',
                          side_effect=[absent, absent, worker, worker]):
            self.assertEqual(workspace_policy.rquickjs_linkage_errors(ROOT), [])

    def test_selected_package_linkage_uses_forward_graphs(self):
        absent = subprocess.CompletedProcess([], 0, 'not-rquickjs v0.11.0\n', '')
        worker = subprocess.CompletedProcess([], 0, 'rquickjs v0.11.0\n', '')
        with patch.object(workspace_policy.subprocess, 'run',
                          side_effect=[absent, absent, worker, worker]) as run:
            self.assertEqual(workspace_policy.rquickjs_linkage_errors(ROOT), [])
        for call in run.call_args_list[:3]:
            self.assertNotIn('-i', call.args[0])
            self.assertIn('--prefix', call.args[0])

    def test_rquickjs_linkage_violations_are_still_rejected(self):
        linked = subprocess.CompletedProcess([], 0, 'rquickjs v0.11.0\n', '')
        absent = subprocess.CompletedProcess([], 0, '', '')
        joint = subprocess.CompletedProcess([], 0, 'rquickjs v0.11.0\n  plak v0.1.0\n', '')
        with patch.object(workspace_policy.subprocess, 'run',
                          side_effect=[linked, linked, absent, joint]):
            errors = workspace_policy.rquickjs_linkage_errors(ROOT)
        self.assertEqual(len(errors), 4)
        self.assertIn('plakous must not link', errors[0])
        self.assertIn('plak must not link', errors[1])
        self.assertIn('plakous-js-runtime must link', errors[2])
        self.assertIn('building both workers', errors[3])

    def test_cargo_tree_failure_without_stderr_reports_exit_status(self):
        failed = subprocess.CompletedProcess([], 101, '', '')
        errors = []
        with patch.object(workspace_policy.subprocess, 'run', return_value=failed):
            output = workspace_policy.cargo_tree_output(ROOT, ['-p', 'plak'], errors)
        self.assertIsNone(output)
        self.assertEqual(len(errors), 1)
        self.assertIn('cargo tree --offline -p plak', errors[0])
        self.assertIn('exit 101', errors[0])


class RepositoryPolicyTests(unittest.TestCase):
    def test_atomic_polyfill_exception_is_checked_for_both_supported_targets(self):
        clean = subprocess.CompletedProcess([], 0, "monty v1.1.0\nheapless v0.7.17\n", "")
        with patch.object(workspace_policy.subprocess, "run", return_value=clean) as run:
            self.assertEqual(workspace_policy.supported_target_atomic_polyfill_errors(), [])
        self.assertEqual(run.call_count, 2)
        targets = set()
        for call in run.call_args_list:
            args = call.args[0]
            targets.add(args[args.index("--target") + 1])
            self.assertEqual(args[args.index("--edges") + 1], "normal,build")
            self.assertIn("--locked", args)
            self.assertIn("--all-features", args)
        self.assertEqual(targets, {"x86_64-unknown-linux-musl", "aarch64-apple-darwin"})

    def test_atomic_polyfill_exception_fails_closed_for_reachability_or_tool_failure(self):
        reachable = subprocess.CompletedProcess([], 0, "atomic-polyfill v1.0.3\n", "")
        failed = subprocess.CompletedProcess([], 101, "", "error: monty-pool =1.1.0 unavailable offline")
        with patch.object(workspace_policy.subprocess, "run", side_effect=[reachable, failed]):
            errors = workspace_policy.supported_target_atomic_polyfill_errors()
        self.assertEqual(len(errors), 2)
        self.assertIn("reachable", errors[0])
        self.assertIn("--target aarch64-apple-darwin", errors[1])
        self.assertIn("exit 101", errors[1])
        self.assertIn("monty-pool =1.1.0 unavailable offline", errors[1])

    """The policy as it stands on the committed tree."""

    @classmethod
    def setUpClass(cls) -> None:
        cls.workspace = policy_module.load_workspace(ROOT)

    def test_every_package_carries_layer_metadata_and_the_tree_is_clean(self) -> None:
        workspace = self.workspace
        self.assertEqual(len(workspace.packages), 39)
        for validator in (
            workspace_policy.validate_package_metadata,
            workspace_policy.validate_layout,
            workspace_policy.validate_layer_edges,
            workspace_policy.validate_harness_pack_independence,
            workspace_policy.validate_production_dependencies,
            workspace_policy.validate_facade_features,
            workspace_policy.validate_neutral_dependencies,
        ):
            with self.subTest(validator=validator.__name__):
                self.assertEqual(validator(workspace), [])

    def test_product_explicit_api_features_cannot_be_borrowed_from_hosted(self) -> None:
        expected = {"native-subscriptions", "openai", "anthropic", "xai",
                    "openrouter", "sessions-sqlite"}
        self.assertEqual(set(self.workspace.policy.facade_features["plak"]), expected)
        self.assertEqual(workspace_policy.validate_facade_features(self.workspace), [])
        product = self.workspace.packages["plak"]
        for provider in ("openai", "anthropic", "xai"):
            with self.subTest(provider=provider):
                metadata = copy.deepcopy(product.metadata)
                edge = next(dependency for dependency in metadata["dependencies"]
                            if dependency["name"] == "plakous" and dependency.get("kind") is None)
                edge["features"].remove(provider)
                packages = dict(self.workspace.packages)
                packages["plak"] = replace(product, metadata=metadata)
                errors = workspace_policy.validate_facade_features(
                    replace(self.workspace, packages=packages))
                self.assertEqual(len(errors), 1)
                self.assertTrue(errors[0].startswith("plak must enable exactly plakous/"), errors)

    def test_instruction_text_may_not_depend_on_a_harness_pack(self) -> None:
        # `harness -> harness` exists so a composition crate may compose packs;
        # the exception is that instruction text may not reach one.
        self.assertEqual(
            self.workspace.policy.harness_pack_independent,
            ("plakous-prompts",),
        )
        prompts = self.workspace.packages["plakous-prompts"]
        harness_dependencies = {
            name
            for name in prompts.production_dependency_names()
            if name in self.workspace.packages
            and self.workspace.packages[name].layer == "harness"
        }
        self.assertEqual(harness_dependencies, set())
        self.assertEqual(
            workspace_policy.validate_harness_pack_independence(self.workspace), []
        )

    def test_all_current_packages_have_one_accepted_classification(self) -> None:
        expected = {
            "supported": {
                "plak",
                "plakous",
                "plakous-agent",
                "plakous-model",
                "plakous-tools",
            },
            "published-internal": {
                "plakous-compose",
                "plakous-host-protocol",
                "plakous-hosted",
                "plakous-tool-files",
                "plakous-tool-plan",
                "plakous-tool-process",
                "plakous-tool-search",
                "plakous-tool-web",
                "plakous-prompts",
                "plakous-workspace",
                "plakous-recursive-subagents",
                "plakous-recursive-subagents-store",
                "plakous-anthropic",
                "plakous-attempt",
                "plakous-auth",
                "plakous-chat-completions",
                "plakous-code-mode",
                "plakous-exchange",
                "plakous-js-eval",
                "plakous-js-runtime",
                "plakous-mcp",
                "plakous-py-eval",
                "plakous-py-worker",
                "plakous-http",
                "plakous-messages",
                "plakous-openai",
                "plakous-openrouter",
                "plakous-responses",
                "plakous-responses-dialect",
                "plakous-store",
                "plakous-xai",
            },
            "repository-only": {
                "plakous-mcp-fake-server",
                "plakous-offline-crash-harness",
                "plakous-test-support",
            },
        }
        actual = workspace_policy.package_classifications(self.workspace)
        self.assertEqual(actual, expected)
        self.assertEqual(sum(map(len, actual.values())), 39)

    def test_layers_and_neutral_packages_are_as_designed(self) -> None:
        by_layer = {
            layer: {package.name for package in self.workspace.in_layers(layer)}
            for layer in self.workspace.policy.layers
        }
        self.assertEqual(by_layer["core"], {"plakous-model", "plakous-tools", "plakous-agent"})
        self.assertEqual(by_layer["facade"], {"plakous"})
        self.assertEqual(
            by_layer["host"],
            {
                "plakous-host-protocol",
                "plakous-hosted",
                "plakous-workspace",
                "plakous-recursive-subagents",
                "plakous-recursive-subagents-store",
            },
        )
        self.assertEqual(by_layer["bin"], {"plak"})
        self.assertEqual(
            by_layer["harness"],
            {
                "plakous-compose",
                "plakous-mcp",
                "plakous-prompts",
                "plakous-tool-files",
                "plakous-tool-plan",
                "plakous-tool-process",
                "plakous-tool-search",
                "plakous-tool-web",
            },
        )
        self.assertEqual(
            {package.name for package in self.workspace.packages.values() if package.neutral},
            {
                "plakous-model",
                "plakous-tools",
                "plakous-agent",
                "plakous-host-protocol",
                "plakous-workspace",
                "plakous-recursive-subagents",
                "plakous-recursive-subagents-store",
            },
        )

    def test_the_product_home_is_never_discovered_inside_a_library(self) -> None:
        """`PlakPaths::from_env` is composition-root only.

        The rule the checker enforces, restated against the real tree so a new
        `from_env` in the hosted crate or the agent constructor fails here as
        well as in `check_workspace`.
        """
        offenders: list[str] = []
        for package in self.workspace.packages.values():
            sources, _ = workspace_policy.package_sources(package, ROOT)
            for path, text in sources.items():
                if "PlakPaths::from_env" in text:
                    offenders.append(f"{package.name}:{path}")
        self.assertEqual(
            sorted(offenders),
            [
                "plak:src/commands/login.rs",
                "plak:src/shell/home.rs",
            ],
            "the product home may be discovered only in the composition root, "
            "which since 3.4 step A holds the CLI too",
        )
        self.assertEqual(
            workspace_policy.validate_product_home_discovery(
                "plakous-hosted",
                {"src/lib.rs": "let paths = PlakPaths::from_env()?;"},
            ),
            [
                "plakous-hosted:src/lib.rs: PlakPaths::from_env is composition-root "
                "only; the product home is a fact a caller states, not one a "
                "library discovers"
            ],
        )

    def test_hosted_dependency_closure_has_no_terminal_ui(self) -> None:
        closure = subprocess.run(
            ["cargo", "tree", "--locked", "--offline", "-p", "plakous-hosted",
             "--edges", "normal", "--prefix", "none", "--format", "{p}"],
            cwd=ROOT, check=True, capture_output=True, text=True,
        ).stdout
        names = {line.split()[0] for line in closure.splitlines() if line.strip()}
        self.assertNotIn("ratatui", names)
        self.assertNotIn("plak", names)

    def test_internal_edges_are_exact_where_the_design_pins_them(self) -> None:
        edges = workspace_policy.internal_production_edges(self.workspace)
        self.assertEqual(edges["plakous-agent"], {"plakous-model", "plakous-tools"})
        self.assertEqual(edges["plakous-store"], {"plakous-agent"})
        self.assertEqual(
            edges["plak"],
            {
                "plakous",
                "plakous-host-protocol",
                "plakous-hosted",
                # `test-hooks` only, and optional: the deterministic offline
                # model the repository gate drives the real binary against. No
                # default or release build resolves it.
                "plakous-offline-crash-harness",
                "plakous-recursive-subagents",
                "plakous-recursive-subagents-store",
            },
        )
        self.assertEqual(
            edges["plakous-hosted"],
            {
                "plakous-host-protocol",
                "plakous-recursive-subagents",
                "plakous",
                "plakous-workspace",
            },
        )
        self.assertEqual(edges["plakous-host-protocol"], set())
        self.assertEqual(edges["plakous-workspace"], {"plakous-host-protocol"})
        self.assertEqual(
            edges["plakous-recursive-subagents"],
            {"plakous", "plakous-host-protocol", "plakous-workspace"},
        )
        self.assertEqual(
            edges["plakous-recursive-subagents-store"],
            {"plakous-recursive-subagents", "plakous-store"},
        )
        self.assertEqual(
            self.workspace.policy.edge_exceptions,
            {
                "plak": (
                    "plakous-host-protocol",
                    "plakous-hosted",
                    # The only `bin -> test` edge in the workspace, and it
                    # resolves only under `plak/test-hooks`. See the comment on
                    # the row in the root manifest.
                    "plakous-offline-crash-harness",
                    "plakous-recursive-subagents",
                    "plakous-recursive-subagents-store",
                ),
                "plakous-hosted": (
                    "plakous-host-protocol",
                    "plakous-recursive-subagents",
                    "plakous-workspace",
                ),
                "plakous-workspace": ("plakous-host-protocol",),
                "plakous-recursive-subagents": (
                    "plakous-host-protocol",
                    "plakous-workspace",
                ),
                "plakous-recursive-subagents-store": ("plakous-recursive-subagents", "plakous-store"),
            },
        )

    def test_security_pins_name_existing_host_bin_sources(self) -> None:
        keys = (
            set(workspace_policy.EXPECTED_PRODUCT_ENVIRONMENT_CALLS)
            | set(workspace_policy.EXPECTED_PRODUCT_PROCESS_COMMANDS)
            | set(workspace_policy.EXPECTED_PRODUCT_PROCESS_IMPORTS)
            | set(workspace_policy.ALLOWED_CLI_MACROS_BY_SOURCE)
            | set(workspace_policy.EXPECTED_PRODUCT_GENERATED_MACROS)
        )
        for package_name, relative in sorted(keys):
            with self.subTest(pin=f"{package_name}:{relative}"):
                package = self.workspace.packages[package_name]
                self.assertIn(package.layer, policy_module.ABOVE_FACADE_LAYERS)
                self.assertTrue((ROOT / package.relative(relative)).is_file())
        self.assertEqual(
            set(workspace_policy.ALLOWED_PLAKOUS_SYMBOLS_BY_PACKAGE),
            {
                package.name
                for package in self.workspace.in_layers(*policy_module.ABOVE_FACADE_LAYERS)
            },
        )

    def test_local_production_lint_policy_rejects_manifest_mutations(self) -> None:
        workspace_manifests = workspace_policy.workspace_manifest_tables(self.workspace)
        self.assertEqual(
            workspace_policy.validate_local_production_lints(
                workspace_manifests
            ),
            [],
        )

        missing_inheritance = copy.deepcopy(workspace_manifests)
        missing_inheritance["plakous-code-mode"].pop("lints")
        errors = workspace_policy.validate_local_production_lints(
            missing_inheritance
        )
        self.assertTrue(any("plakous-code-mode" in error for error in errors), errors)

        weakened_js = copy.deepcopy(workspace_manifests)
        weakened_js["plakous-js-eval"]["lints"]["rust"]["unsafe_code"] = "allow"
        errors = workspace_policy.validate_local_production_lints(
            weakened_js
        )
        self.assertTrue(any("plakous-js-eval" in error for error in errors), errors)

    def test_supported_compile_fixture_inventory_and_dependencies_are_exact(self) -> None:
        fixtures = workspace_policy.supported_compile_fixtures(self.workspace)
        self.assertEqual(
            set(fixtures),
            {
                "agent-consumer",
                "facade-consumer",
                "js-embedder-consumer",
                "model-consumer",
                "tools-consumer",
            },
        )
        for package in self.workspace.packages.values():
            if package.package_class == "supported" and package.layer != "bin":
                self.assertIn(workspace_policy.fixture_name_for(package), fixtures)
        self.assertEqual(
            fixtures["facade-consumer"]["manifest"]["dependencies"]["plakous"]["path"],
            "../../../crates/plakous",
        )
        for fixture_name, contract in fixtures.items():
            fixture_root = ROOT / f"tests/fixtures/{fixture_name}"
            with (fixture_root / "Cargo.toml").open("rb") as stream:
                manifest = tomllib.load(stream)
            files, inventory_errors = workspace_policy.non_target_file_inventory(fixture_root)
            self.assertEqual(
                workspace_policy.validate_supported_compile_fixture(
                    fixture_name, manifest, files, contract, inventory_errors
                ),
                [],
            )

    def test_supported_compile_fixture_rejects_manifest_mutations(self) -> None:
        contract = workspace_policy.supported_compile_fixtures(self.workspace)["facade-consumer"]
        expected_files = set(contract["files"])
        mutations = []

        wrong_path = copy.deepcopy(contract["manifest"])
        wrong_path["dependencies"]["plakous"]["path"] = "../../../crates/core/model"
        mutations.append(wrong_path)

        wrong_version = copy.deepcopy(contract["manifest"])
        wrong_version["dependencies"]["plakous"]["version"] = "0.2.0"
        mutations.append(wrong_version)

        wrong_features = copy.deepcopy(contract["manifest"])
        wrong_features["dependencies"]["plakous"]["features"] = ["full"]
        mutations.append(wrong_features)

        default_features = copy.deepcopy(contract["manifest"])
        default_features["dependencies"]["plakous"]["default-features"] = False
        mutations.append(default_features)

        repository_test_support = copy.deepcopy(contract["manifest"])
        repository_test_support["dependencies"]["plakous-test-support"] = {
            "path": "../../support/plakous-test-support"
        }
        mutations.append(repository_test_support)

        extra_runtime_feature = copy.deepcopy(contract["manifest"])
        extra_runtime_feature["dependencies"]["tokio"]["features"].append("fs")
        mutations.append(extra_runtime_feature)

        build_script = copy.deepcopy(contract["manifest"])
        build_script["package"]["build"] = "build.rs"
        mutations.append(build_script)

        extra_target = copy.deepcopy(contract["manifest"])
        extra_target["bin"] = [{"name": "escape", "path": "src/escape.rs"}]
        mutations.append(extra_target)

        for manifest in mutations:
            errors = workspace_policy.validate_supported_compile_fixture(
                "facade-consumer", manifest, expected_files, contract
            )
            self.assertTrue(any("normalized manifest differs" in error for error in errors))

    def test_supported_compile_fixture_rejects_build_and_source_inventory_drift(self) -> None:
        contract = workspace_policy.supported_compile_fixtures(self.workspace)["model-consumer"]
        manifest = contract["manifest"]
        for extra_file in ("build.rs", "src/bin/escape.rs", "src/extra.rs"):
            errors = workspace_policy.validate_supported_compile_fixture(
                "model-consumer",
                manifest,
                set(contract["files"]) | {extra_file},
                contract,
            )
            self.assertTrue(any("non-target files" in error for error in errors), errors)

    def test_supported_compile_fixture_rejects_symlink_source_file(self) -> None:
        contract = workspace_policy.supported_compile_fixtures(self.workspace)["model-consumer"]
        with tempfile.TemporaryDirectory() as directory:
            fixture_root = Path(directory)
            (fixture_root / "src").mkdir()
            (fixture_root / "Cargo.toml").write_text("manifest", encoding="utf-8")
            (fixture_root / "Cargo.lock").write_text("lock", encoding="utf-8")
            replacement = fixture_root / "replacement.rs"
            replacement.write_text("fn main() {}", encoding="utf-8")
            (fixture_root / "src/main.rs").symlink_to(replacement)

            files, inventory_errors = workspace_policy.non_target_file_inventory(
                fixture_root
            )
            errors = workspace_policy.validate_supported_compile_fixture(
                "model-consumer",
                contract["manifest"],
                files,
                contract,
                inventory_errors,
            )
        self.assertTrue(any("symlink file is forbidden: src/main.rs" in e for e in errors))
        self.assertTrue(any("non-target files" in error for error in errors), errors)

    def test_supported_compile_fixture_rejects_symlink_source_directory(self) -> None:
        contract = workspace_policy.supported_compile_fixtures(self.workspace)["model-consumer"]
        with tempfile.TemporaryDirectory() as directory:
            fixture_root = Path(directory)
            outside = fixture_root / "outside"
            outside.mkdir()
            (outside / "main.rs").write_text("fn main() {}", encoding="utf-8")
            (fixture_root / "src").symlink_to(outside, target_is_directory=True)
            (fixture_root / "Cargo.toml").write_text("manifest", encoding="utf-8")
            (fixture_root / "Cargo.lock").write_text("lock", encoding="utf-8")

            files, inventory_errors = workspace_policy.non_target_file_inventory(
                fixture_root
            )
            errors = workspace_policy.validate_supported_compile_fixture(
                "model-consumer",
                contract["manifest"],
                files,
                contract,
                inventory_errors,
            )
        self.assertTrue(any("symlink directory is forbidden: src" in e for e in errors))
        self.assertTrue(any("non-target files" in error for error in errors), errors)

    def test_application_rejects_an_extra_direct_dependency(self) -> None:
        errors = workspace_policy.validate_single_dependency(
            "plak",
            {"plakous", "reqwest"},
            "plakous",
        )
        self.assertTrue(any("direct dependencies" in error for error in errors))

    def test_consumer_requires_the_public_facade(self) -> None:
        errors = workspace_policy.validate_single_dependency(
            "facade consumer",
            {"plakous-model"},
            "plakous",
        )
        self.assertTrue(any("expected ['plakous']" in error for error in errors))

    def test_production_edges_exclude_test_and_build_dependencies(self) -> None:
        package = synthetic_package("x", "core", "crates/plakous/x")
        package = replace(
            package,
            metadata={
                **package.metadata,
                "dependencies": [
                    {"name": "plakous-model", "kind": None},
                    {"name": "plakous-agent", "kind": "dev"},
                    {"name": "codegen-helper", "kind": "build"},
                ],
            },
        )
        self.assertEqual(package.production_dependency_names(), {"plakous-model"})
        self.assertEqual(
            workspace_policy.production_dependency_names(package.metadata), {"plakous-model"}
        )


class SourceScanTests(unittest.TestCase):
    """Token-aware production source scanning and the security pins."""

    def test_cfg_test_module_does_not_hide_later_production_code(self) -> None:
        source = """
fn before() {}
#[cfg(test)]
mod tests {
    const ONLY_A_TEST: &str = "env::vars_os() { }";
}
fn after() { env::vars_os(); }
"""
        production = workspace_policy.production_rust_source(source)
        self.assertNotIn("ONLY_A_TEST", production)
        self.assertIn("fn after() { env::vars_os(); }", production)

    def test_cfg_all_test_predicate_is_test_only(self) -> None:
        source = """
#[cfg(all(test, feature = "ui"))]
mod prop_tests { fn decoy() { std::env::vars_os(); } }
#[cfg(all(test, feature = "ui"))]
mod perf_gate;
fn production() {}
"""
        production = workspace_policy.production_rust_source(source)
        self.assertNotIn("vars_os", production)
        self.assertIn("fn production()", production)
        self.assertIsNotNone(
            workspace_policy.CFG_TEST_MODULE_DECLARATION.search(
                workspace_policy._blank_rust_non_code(source)
            )
        )

    def test_cfg_test_text_in_a_literal_does_not_truncate_production(self) -> None:
        source = 'const TEXT: &str = "#[cfg(test)]";\nfn after() { env::vars(); }\n'
        self.assertIn("env::vars()", workspace_policy.production_rust_source(source))

    def test_cfg_test_function_and_raw_identifier_module_are_not_production(self) -> None:
        source = """
#[cfg(test)]
fn decoy() { std :: env :: vars_os(); }
#[cfg(test)]
mod r#tests { fn decoy() { std::process::Command::new("codex"); } }
fn production() {}
"""
        production = workspace_policy.production_rust_source(source)
        self.assertNotIn("vars_os", production)
        self.assertNotIn("Command", production)
        self.assertIn("fn production()", production)

    @staticmethod
    def cli_security_fixture(*, main_extra: str = "") -> dict[str, dict[str, str]]:
        """Package -> package-relative path -> source, matching the pins exactly."""
        return {
            "plak": {
                "src/lib.rs": f"""
use std::env;
use std::process::ExitCode;
fn main_reads() {{
    let _ = env::var_os("XDG_RUNTIME_DIR");
    let _ = env::var_os("NO_COLOR");
}}
fn reload(executable: &std::path::Path) {{
    let _ = std::process::Command::new(&executable);
}}
{main_extra}
""",
                "src/main.rs": "use std::process::ExitCode;",
                "src/commands/support.rs": """
use std::env;
fn edit_config_file(name: &str) {
    let _ = env::var(name);
    let _ = std::process::Command::new("/bin/sh");
}
""",
                "src/oauth_ui.rs": """
use std::process::{Command, Stdio};
fn clipboard(prog: &str) {
    let _ = Command::new(prog);
    let _ = Stdio::null();
}
fn ssh() {
    let _ = std::env::var_os("SSH_CONNECTION");
    let _ = std::env::var_os("SSH_CLIENT");
    let _ = std::env::var_os("SSH_TTY");
}
""",
                "src/home/paths.rs": """
fn paths() {
    let _ = std::env::var_os("PLAK_HOME");
    let _ = std::env::var_os("HOME");
    let _ = std::env::var_os("XDG_STATE_HOME");
    let _ = std::env::var_os("HOME");
    let _ = std::env::var_os("HOME");
    let _ = std::env::var_os("PLAK_TEST_TMPDIR");
}
""",
                "src/carry.rs": """
fn carry(executable: &str, snapshot: &str) {
    let _ = std::process::Command::new(&executable);
    let _ = std::process::Command::new(executable);
    let _ = std::process::Command::new(snapshot);
}
""",
                "src/home/secrets.rs": """
fn secrets(name: &str) {
    let _ = std::env::var_os(name);
    let _ = std::env::var(name);
    let _ = std::env::var_os(name);
    let _ = std::env::var(name);
    let _ = std::env::var_os(name);
}
""",
            },
            "plakous-hosted": {
                "src/agent.rs": "fn product_agent() {}",
                "src/agent_tree.rs": """
fn mint() {}
""",
            },
            "plakous-workspace": {
                "src/git_redirect_fence.rs": """
fn discover_real_git() {
    let _ = std::env::var_os("PATH");
    let _ = std::env::var_os("PATH");
}
""",
                "src/workspace.rs": """
fn workspace() {
    let _ = std::env::var_os("HOME");
}
""",
                "src/occupancy_vcs.rs": """
use std::process::{Command, Output};
fn vcs() {
    let _ = Command::new("sh");
    let _ = Command::new("git");
    let _ = std::env::var("PATH");
    let _ = std::env::var("HOME");
    let _ = std::env::var_os("EMAIL");
    let _: Option<Output> = None;
}
""",
                "src/vcs_fixtures.rs": """
use std::process::Command;
fn fixtures() {
    let _ = Command::new("git");
    let _ = Command::new("git");
    let _ = Command::new("git");
    let _ = Command::new("git");
    let _ = Command::new("git");
    let _ = Command::new("git");
    let _ = Command::new("git");
    let _ = Command::new("git");
    let _ = Command::new("jj");
    let _ = Command::new("jj");
    let _ = Command::new("jj");
    let _ = Command::new("jj");
    let _ = Command::new("jj");
    let _ = Command::new("jj");
    let _ = Command::new("git");
    let _ = Command::new("git");
    let _ = Command::new("git");
}
""",
            },
        }

    @staticmethod
    def validate_fixture(fixture: dict[str, dict[str, str]]) -> list[str]:
        errors: list[str] = []
        for package, sources in fixture.items():
            errors.extend(workspace_policy.validate_cli_security_sources(package, sources))
        return errors

    def test_token_aware_cli_policy_accepts_comment_and_literal_decoys(self) -> None:
        sources = self.cli_security_fixture(
            main_extra='// std::process::Command; env::vars_os();\nconst DECOY: &str = "Command::new";'
        )
        self.assertEqual(self.validate_fixture(sources), [])

    def test_keyword_before_bang_is_not_a_macro(self) -> None:
        sources = self.cli_security_fixture(
            main_extra="fn k(a: bool, r: std::ops::Range<u8>) { if !(r).contains(&1) {} while !(a) {} }"
        )
        self.assertEqual(self.validate_fixture(sources), [])

    def test_token_aware_cli_policy_rejects_alias_and_whitespace_bypasses(self) -> None:
        for source in (
            "use std::process::Command as Runner; fn x() { Runner::new(\"codex\"); }",
            "fn x() { std :: process :: Command :: new(\"codex\"); }",
            "use std::process::{Child, Command}; fn x() { Command::new(\"sh\"); }",
            "use std::{process::{Child, Command}, io};",
            "use std::process::{Child, *};",
            "use std::env as e; fn x() { e::vars_os(); }",
            "fn x() { let read = std::env::vars_os; let _ = read(); }",
            "fn x() { let read = std::env::var; let _ = read(\"EVIL\"); }",
            "use std::env::*; fn x() { let _ = vars_os(); }",
            "use std::process as p; fn x() { p::Command::new(\"sh\"); }",
            "use plakous::Command as Runner; fn x() { Runner::new(\"sh\"); }",
            "fn x() { plakous::Command::new(\"sh\"); }",
            "use plakous::tools::ReexportedRunner as Runner; fn x() { Runner::new(\"sh\"); }",
            "use plakous::tools::Runner; fn x() { Runner::new(\"sh\"); }",
            "fn x() { plakous::tools::ReexportedRunner::new(\"sh\"); }",
            'fn vars() {} fn x() { let _ = std::env::var("EVIL"); }',
        ):
            with self.subTest(source=source):
                errors = self.validate_fixture(self.cli_security_fixture(main_extra=source))
                self.assertTrue(errors)

    def test_transient_probe_bridge_and_gate_macros_are_source_specific(self) -> None:
        cases = [
            ("src/agent/build.rs", "fn x() { plakous::sessions::internal::reachability_probe(&runtime); }"),
            ("src/agent/children.rs", "fn x() { tokio::pin!(ready); tokio::select! { value = ready => value } }"),
        ]
        for path, source in cases:
            with self.subTest(path=path):
                self.assertEqual(workspace_policy.validate_cli_security_sources("plakous-hosted", {path: source}), [])
                self.assertTrue(workspace_policy.validate_cli_security_sources("plakous-hosted", {"src/x.rs": source}))
                self.assertTrue(workspace_policy.validate_cli_security_sources("plak", {path: source}))

    def test_plakous_symbol_allowlist_is_per_package(self) -> None:
        errors = workspace_policy.validate_cli_security_sources(
            "plak-tui", {"src/lib.rs": "use plakous::agent::Agent;"}
        )
        self.assertTrue(any("unapproved plakous facade symbols" in e for e in errors), errors)

        self.assertEqual(
            workspace_policy.validate_cli_security_sources(
                "plakous-hosted", {"src/x.rs": "use plakous::agent::Agent;"}
            ),
            [],
        )
        self.assertEqual(
            workspace_policy.validate_cli_security_sources(
                "plakous-hosted",
                {"src/x.rs": 'let _ = plakous::anthropic::claude_max_output_tokens("claude-sonnet-5");'},
            ),
            [],
        )
        self.assertEqual(
            workspace_policy.validate_cli_security_sources(
                "plakous-hosted",
                {"src/x.rs": "use plakous::agent::{AbandonedTurnClaim, AbandonedTurnReport, AbandonmentCause};\nuse plakous::tools::plan_restoration_note;\nlet _ = plakous::xai::grok_max_output_tokens(\"grok-4.6\");"},
            ),
            [],
        )
        errors = workspace_policy.validate_cli_security_sources(
            "plakous-hosted",
            {"src/x.rs": "let _ = plakous::anthropic::claude_context_window(\"claude-sonnet-5\");"},
        )
        self.assertTrue(any("unapproved plakous facade symbols" in e for e in errors), errors)

    def test_hosted_openrouter_admission_uses_provider_protocol_constants(self) -> None:
        for protocol in ("CHAT", "RESPONSES", "MESSAGES"):
            with self.subTest(protocol=protocol):
                symbol = f"OPENROUTER_{protocol}_PROTOCOL_ID"
                self.assertEqual(
                    workspace_policy.validate_cli_security_sources(
                        "plakous-hosted",
                        {"src/openrouter.rs": f"let _ = plakous::openrouter::{symbol};"},
                    ),
                    [],
                )

    def test_reload_rehearsal_facade_symbols_are_scoped_to_one_cli_file(self) -> None:
        source = "use plakous::sessions::rehearsal::{OfflineReopenOutcome, inspect_sqlite};"
        self.assertEqual(
            workspace_policy.validate_cli_security_sources(
                "plak", {"src/commands/reload_rehearsal.rs": source}
            ),
            [],
        )
        for package, path in (
            ("plak", "src/commands/other.rs"),
            ("plakous-hosted", "src/commands/reload_rehearsal.rs"),
        ):
            with self.subTest(package=package, path=path):
                errors = workspace_policy.validate_cli_security_sources(package, {path: source})
                self.assertTrue(
                    any("unapproved plakous facade symbols" in error for error in errors), errors
                )
        errors = workspace_policy.validate_cli_security_sources(
            "plak",
            {"src/commands/reload_rehearsal.rs": "use plakous::sessions::OfflineReopenInspection;"},
        )
        self.assertTrue(any("unapproved plakous facade symbols" in error for error in errors), errors)

    def test_preview_facade_bridge_is_enrolled_only_for_hosted(self) -> None:
        for source in (
            "use plakous::tools::preview_redaction;",
            'let _ = plakous::tools::preview_redaction::text("safe");',
        ):
            self.assertEqual(
                workspace_policy.validate_cli_security_sources(
                    "plakous-hosted", {"src/x.rs": source}
                ),
                [],
            )
            for package in ("plak", "plakous-host-protocol", "plakous-workspace"):
                with self.subTest(package=package, source=source):
                    errors = workspace_policy.validate_cli_security_sources(
                        package, {"src/x.rs": source}
                    )
                    self.assertTrue(any("unapproved plakous facade symbols" in e for e in errors), errors)
        errors = workspace_policy.validate_cli_security_sources(
            "plakous-hosted", {"src/x.rs": "use plakous::tools::other_redactor;"}
        )
        self.assertTrue(any("unapproved plakous facade symbols" in e for e in errors), errors)

    def test_host_job_config_and_compaction_refusal_symbols_are_enrolled(self) -> None:
        for package in ("plak", "plakous-hosted"):
            with self.subTest(package=package):
                sources = {
                    "src/x.rs": (
                        "use plakous::tools::JobConfig; "
                        "use plakous::agent::CompactionRefusalReason; "
                        "fn config() { let _ = plakous::tools::JobConfig::default(); "
                        "let _ = plakous::tools::JobConfig::new(30, 36000); }"
                    )
                }
                self.assertEqual(
                    workspace_policy.validate_cli_security_sources(package, sources), []
                )
                errors = workspace_policy.validate_cli_security_sources(
                    package, {"src/x.rs": "use plakous::JobHost;"}
                )
                self.assertTrue(
                    any("unapproved plakous facade symbols" in error for error in errors),
                    errors,
                )

    def test_cli_policy_rejects_procfs_and_foreign_process_environment_access(self) -> None:
        for source in (
            'fn leak() { let _ = std::fs::read("/proc/self/environ"); }',
            'const LEAK: &str = concat!("/proc/self/", "environ");',
            'unsafe extern "C" { fn getenv(name: *const i8) -> *mut i8; }',
            'unsafe extern "C" { fn system(command: *const i8) -> i32; }',
            '#[link(name = "c")] unsafe extern "C" { '
            '#[link_name = "system"] fn harmless(command: *const i8) -> i32; }',
        ):
            with self.subTest(source=source):
                errors = self.validate_fixture(self.cli_security_fixture(main_extra=source))
                self.assertTrue(errors)

    def test_token_aware_cli_policy_rejects_concat_escape_and_macro_bypasses(self) -> None:
        for source in (
            'const FLAG: &str = concat!("--api", "-key");',
            r'const FLAG: &str = "--api\x2dkey";',
            'fn flag() { let _ = ["--api", "-key"].concat(); }',
            'delegate!(std :: process :: Command);',
            'plakous::emit_delegated_login!();',
            'unwrap_cfg! { #[cfg(test)] fn hidden() { std::process::Command::new("sh"); } }',
            'include!("generated.rs");',
        ):
            with self.subTest(source=source):
                errors = self.validate_fixture(self.cli_security_fixture(main_extra=source))
                self.assertTrue(errors)

    def test_harmless_local_name_matching_ambient_reader_is_accepted(self) -> None:
        sources = self.cli_security_fixture(
            main_extra=(
                "fn vars() -> usize { 0 }\n"
                "struct Command; impl Command { fn new(_: &str) -> Self { Self } }\n"
                'fn local_command() { let _ = Command::new("safe"); }'
            )
        )
        self.assertEqual(self.validate_fixture(sources), [])

    def test_product_capabilities_are_exact_per_file(self) -> None:
        mutations = (
            ("plak", "src/oauth_ui.rs", '\nfn extra() { let _ = Command::new("sh"); }'),
            (
                "plak",
                "src/home/paths.rs",
                '\nfn drift() { let _ = std::env::var_os("XDG_CONFIG_HOME"); }',
            ),
            ("plak", "src/home/secrets.rs", '\nfn enumerate() { let _ = std::env::vars_os(); }'),
        )
        for package, path, extra in mutations:
            with self.subTest(path=path):
                sources = self.cli_security_fixture()
                sources[package][path] += extra
                self.assertTrue(self.validate_fixture(sources))

    def test_pins_are_keyed_by_package_not_path(self) -> None:
        """The same relative path under another package carries no pin."""
        product_paths = self.cli_security_fixture()["plak"]["src/home/paths.rs"]
        errors = workspace_policy.validate_cli_security_sources(
            "plak-tui", {"src/home/paths.rs": product_paths}
        )
        self.assertTrue(any("environment reader calls" in error for error in errors), errors)

    def test_product_session_authority_check_is_token_aware(self) -> None:
        harmless = {
            "src/view.rs": """
// CompactionThresholds and fork_session are retired.
const NOTE: &str = "estimate_tokens keep_from_index";
#[cfg(test)]
fn maybe_compact_session() {}
fn render_transcript() {}
"""
        }
        self.assertEqual(
            workspace_policy.validate_product_session_authority_sources("plak", harmless), []
        )

        for identifier in sorted(
            workspace_policy.FORBIDDEN_PRODUCT_SESSION_AUTHORITY_IDENTIFIERS
        ):
            with self.subTest(identifier=identifier):
                sources = {"src/authority.rs": f"fn {identifier}() {{}}"}
                errors = workspace_policy.validate_product_session_authority_sources(
                    "plak", sources
                )
                self.assertTrue(errors)

    def test_neutral_provider_scan_ignores_decoys_and_rejects_tokens_and_concat(self) -> None:
        self.assertEqual(
            workspace_policy.provider_names_in_rust(
                '// openai\nconst NOTE: &str = "not a provider";'
            ),
            set(),
        )
        self.assertEqual(
            workspace_policy.provider_names_in_rust(
                'const PROVIDER: &str = concat!("open", "ai");'
            ),
            {"openai"},
        )
        self.assertEqual(
            workspace_policy.provider_names_in_rust("macro_call!(anthropic);"),
            {"anthropic"},
        )

    def test_production_source_discovery_covers_custom_path_and_test_names(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "package/src"
            build = root / "package/build"
            source.mkdir(parents=True)
            build.mkdir(parents=True)
            (source / "main.rs").write_text(
                '#[path = "../shared.rs"]\nmod shared;\n'
                '#[cfg_attr(all(), path = "../cfg_shared.inc")]\nmod cfg_shared;\n'
                'include!("../included.inc");\n'
                'include!("../linked.inc");\n',
                encoding="utf-8",
            )
            (source / "tests.rs").write_text("pub fn production() {}\n", encoding="utf-8")
            (source / "foo_tests.rs").write_text(
                "pub fn also_production() {}\n", encoding="utf-8"
            )
            (root / "package/shared.rs").write_text("pub fn shared() {}\n", encoding="utf-8")
            (root / "package/cfg_shared.inc").write_text(
                "pub fn cfg_shared() {}\n", encoding="utf-8"
            )
            (root / "package/included.inc").write_text(
                "pub fn included() {}\n", encoding="utf-8"
            )
            custom = build / "custom.rs"
            custom.write_text("mod helper;\nfn main() {}\n", encoding="utf-8")
            (build / "helper.rs").write_text("pub fn helper() {}\n", encoding="utf-8")
            integration = root / "package/tests/ordinary.rs"
            integration.parent.mkdir()
            integration.write_text("fn integration_only() {}\n", encoding="utf-8")
            outside = root / "outside.rs"
            outside.write_text("pub fn hidden() {}\n", encoding="utf-8")
            (source / "linked.rs").symlink_to(outside)
            outside_directory = root / "outside-directory"
            outside_directory.mkdir()
            (outside_directory / "hidden.rs").write_text(
                "pub fn hidden_too() {}\n", encoding="utf-8"
            )
            (source / "linked-directory").symlink_to(
                outside_directory, target_is_directory=True
            )
            (root / "package/linked.inc").symlink_to(outside)
            package = {
                "targets": [
                    {"kind": ["bin"], "src_path": str(source / "main.rs")},
                    {"kind": ["custom-build"], "src_path": str(custom)},
                    {"kind": ["test"], "src_path": str(integration)},
                ]
            }

            files, errors = workspace_policy.production_rust_sources(package, root)
            relative = {path.relative_to(root).as_posix() for path in files}

        self.assertEqual(
            relative,
            {
                "package/build/custom.rs",
                "package/build/helper.rs",
                "package/cfg_shared.inc",
                "package/included.inc",
                "package/shared.rs",
                "package/src/foo_tests.rs",
                "package/src/main.rs",
                "package/src/tests.rs",
            },
        )
        self.assertGreaterEqual(
            sum("symbolic link" in error for error in errors), 3, errors
        )

    def test_out_of_line_cfg_test_module_file_is_not_production(self) -> None:
        """A `#[cfg(test)] mod x;` file is a whole-file test body, not shipping code."""

        def discovered(declaration: str) -> set[str]:
            with tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                source = root / "package/src"
                (source / "agent_tree").mkdir(parents=True)
                (source / "lib.rs").write_text("mod agent_tree;\n", encoding="utf-8")
                (source / "agent_tree/mod.rs").write_text(
                    "mod mail;\n" + declaration + "mod test_support;\n",
                    encoding="utf-8",
                )
                (source / "agent_tree/mail.rs").write_text(
                    "pub fn mail() {}\n", encoding="utf-8"
                )
                (source / "agent_tree/test_support.rs").write_text(
                    'use std::process::Command;\n'
                    'pub fn git() {\n    Command::new("git");\n}\n',
                    encoding="utf-8",
                )
                package = {
                    "targets": [
                        {"kind": ["lib"], "src_path": str(source / "lib.rs")}
                    ]
                }
                files, errors = workspace_policy.production_rust_sources(package, root)
                self.assertEqual(errors, [])
                return {path.relative_to(root).as_posix() for path in files}

        self.assertEqual(
            discovered("#[cfg(test)]\n"),
            {
                "package/src/lib.rs",
                "package/src/agent_tree/mod.rs",
                "package/src/agent_tree/mail.rs",
            },
        )
        self.assertEqual(
            discovered(""),
            {
                "package/src/lib.rs",
                "package/src/agent_tree/mod.rs",
                "package/src/agent_tree/mail.rs",
                "package/src/agent_tree/test_support.rs",
            },
        )

    def test_cfg_test_module_directory_form_is_skipped_whole(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "package/src"
            (source / "fixtures").mkdir(parents=True)
            (source / "lib.rs").write_text(
                "#[cfg(test)]\nmod fixtures;\n", encoding="utf-8"
            )
            (source / "fixtures/mod.rs").write_text("mod inner;\n", encoding="utf-8")
            (source / "fixtures/inner.rs").write_text(
                'pub fn inner() { std::process::Command::new("jj"); }\n',
                encoding="utf-8",
            )
            package = {
                "targets": [{"kind": ["lib"], "src_path": str(source / "lib.rs")}]
            }
            files, errors = workspace_policy.production_rust_sources(package, root)

        self.assertEqual(errors, [])
        self.assertEqual(
            {path.relative_to(root).as_posix() for path in files},
            {"package/src/lib.rs"},
        )

    def test_nonliteral_include_target_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "crate/src"
            source.mkdir(parents=True)
            main = source / "lib.rs"
            main.write_text('include!(concat!("hidden", ".inc"));\n', encoding="utf-8")

            _files, errors = workspace_policy.discover_rust_files(
                root, [(main, False)]
            )

        self.assertTrue(any("direct string literal" in error for error in errors), errors)


class FeaturePolicyTests(unittest.TestCase):
    """Exercise static feature-topology contracts."""

    @classmethod
    def setUpClass(cls) -> None:
        cls.workspace = policy_module.load_workspace(ROOT)
        cls.manifests = {
            name: package.manifest for name, package in cls.workspace.packages.items()
        }
        cls.layers = {name: package.layer for name, package in cls.workspace.packages.items()}
        cls.digests = feature_policy.digest_packages(cls.workspace)

    def contracts(self, manifests: dict[str, dict[str, object]]) -> list[str]:
        return feature_policy.validate_feature_contracts(manifests, self.digests, self.layers)

    def test_repository_feature_contracts_are_exact(self) -> None:
        self.assertEqual(self.contracts(self.manifests), [])
        self.assertEqual(
            self.digests,
            {
                name
                for name, layer in self.layers.items()
                if layer in feature_policy.DIGEST_LAYERS
            },
        )
        self.assertEqual(set(feature_policy.EXPECTED_SECURITY_SURFACE_DIGESTS), self.digests)

    def test_dependency_graph_is_derived_from_the_manifests(self) -> None:
        edges = feature_policy.direct_workspace_edges(self.manifests)
        self.assertEqual(
            edges["plakous"],
            {
                "plakous-agent",
                "plakous-compose",
                "plakous-model",
                "plakous-prompts",
                "plakous-store",
                "plakous-tool-files",
                "plakous-tool-plan",
                "plakous-tool-process",
                "plakous-tool-search",
                "plakous-tool-web",
                "plakous-tools",
            },
        )
        self.assertEqual(edges["plakous-agent"], {"plakous-model", "plakous-tools"})
        self.assertEqual(edges["plakous-store"], {"plakous-agent"})
        self.assertIn("plakous-responses-dialect", edges["plakous-openai"])
        self.assertEqual(edges["plakous-code-mode"], set())
        self.assertNotIn("plakous-dispatch", edges)
        self.assertEqual(self.workspace.policy.edge_exceptions.get("plakous-code-mode"), None)
        self.assertIn(
            ("plakous", "sessions-sqlite", "plakous-store/sqlite"),
            feature_policy.REQUIRED_DEPENDENCY_FEATURE_EDGES,
        )

    def test_optional_edge_drift_is_caught_by_the_digest(self) -> None:
        manifests = copy.deepcopy(self.manifests)
        manifests["plakous-openai"]["dependencies"]["plakous-http"]["optional"] = True
        errors = self.contracts(manifests)
        self.assertTrue(any("plakous-openai" in e and "security surface changed" in e for e in errors))

    def test_dependency_feature_forwarding_drift_is_rejected(self) -> None:
        manifests = copy.deepcopy(self.manifests)
        edges = manifests["plakous-openai"]["features"]["native-auth"]
        edges.remove("plakous-auth/http-refresh")
        errors = self.contracts(manifests)
        self.assertTrue(any("feature-forwarding" in error for error in errors))

    def test_host_bin_features_forward_only_to_the_facade_or_host(self) -> None:
        manifests = copy.deepcopy(self.manifests)
        manifests["plak"]["dependencies"]["plakous-store"] = {"path": "x", "optional": True}
        manifests["plak"]["features"]["otel"].append("plakous-store/sqlite")
        errors = self.contracts(manifests)
        self.assertTrue(any("forwards below the facade" in error for error in errors), errors)
        manifests = copy.deepcopy(self.manifests)
        manifests["plak"]["features"]["otel"].append("plakous/xai")
        errors = self.contracts(manifests)
        self.assertFalse(any("forwards below the facade" in error for error in errors), errors)

    def test_target_specific_dependency_edge_is_not_omitted(self) -> None:
        manifests = copy.deepcopy(self.manifests)
        manifests["plakous-model"]["target"] = {
            "cfg(windows)": {
                "dependencies": {
                    "plakous-auth": {"path": "../plakous-auth"},
                }
            }
        }
        self.assertIn(
            "plakous-auth", feature_policy.direct_workspace_edges(manifests)["plakous-model"]
        )
        errors = self.contracts(manifests)
        self.assertTrue(any("security surface changed" in error for error in errors), errors)

    def test_target_specific_renamed_feature_edge_is_not_omitted(self) -> None:
        manifests = copy.deepcopy(self.manifests)
        manifests["plakous-xai"]["target"] = {
            "cfg(windows)": {
                "dependencies": {
                    "compat-on-windows": {
                        "package": "plakous-auth",
                        "path": "../plakous-auth",
                        "optional": True,
                    }
                }
            }
        }
        manifests["plakous-xai"]["features"]["native-auth"].append(
            "compat-on-windows/external-compat"
        )
        errors = self.contracts(manifests)
        self.assertTrue(any("feature-forwarding" in error for error in errors), errors)
        self.assertTrue(any("security surface changed" in error for error in errors), errors)

    def test_dependency_capability_and_external_edge_drift_are_rejected(self) -> None:
        mutations = []
        requested_feature = copy.deepcopy(self.manifests)
        requested_feature["plakous-openai"]["dependencies"]["plakous-auth"][
            "features"
        ] = ["external-compat"]
        mutations.append(requested_feature)
        default_features = copy.deepcopy(self.manifests)
        default_features["plakous-openai"]["dependencies"]["plakous-auth"][
            "default-features"
        ] = True
        mutations.append(default_features)
        external_dependency = copy.deepcopy(self.manifests)
        external_dependency["plakous-model"]["dependencies"]["reqwest"] = "0.12"
        mutations.append(external_dependency)
        feature_alias = copy.deepcopy(self.manifests)
        feature_alias["plakous"]["features"]["auth"].append("openrouter")
        mutations.append(feature_alias)
        for manifests in mutations:
            with self.subTest(digest=feature_policy.security_surface_digest(manifests["plakous"])):
                errors = self.contracts(manifests)
                self.assertTrue(any("security surface changed" in error for error in errors), errors)

    def test_closure_inventory_is_per_package_never_a_cross_product(self) -> None:
        packages = feature_policy.closure_packages(self.manifests)
        self.assertIn("plakous", packages)
        self.assertIn("plakous-store", packages)
        self.assertIn("plak", packages)
        self.assertNotIn("plakous-model", packages)
        profile_counts = {
            package: len(
                feature_policy.feature_sets(
                    feature_policy.optional_features(self.manifests[package]),
                    "full" in self.manifests[package]["features"],
                )
            )
            for package in packages
        }
        self.assertEqual(profile_counts["plakous"], 2**9 + 2)
        # sqlite, perf-instrumentation, repository-only commit-trace.
        self.assertEqual(profile_counts["plakous-store"], 2**3 + 1)
        # git-cli, jj-lib, maintainer, otel, test-hooks, test-support.
        self.assertEqual(profile_counts["plak"], 2**6 + 1)
        self.assertNotIn("plakous-js-eval", profile_counts)
        self.assertNotIn("plakous-js-runtime", profile_counts)
        for count in profile_counts.values():
            self.assertLessEqual(count, 2**feature_policy.MAX_OPTIONAL_FEATURES + 2)

    def test_expected_closure_walks_feature_tables_and_optional_edges(self) -> None:
        manifests = self.manifests
        minimal = feature_policy.expected_closure("plakous", (), False, manifests)
        self.assertEqual(
            minimal,
            {
                "plakous",
                "plakous-agent",
                "plakous-compose",
                "plakous-model",
                "plakous-prompts",
                "plakous-store",
                "plakous-tool-files",
                "plakous-tool-plan",
                "plakous-tool-process",
                "plakous-tool-search",
                "plakous-tool-web",
                "plakous-tools",
            },
        )
        native = feature_policy.expected_closure(
            "plakous", ("native-subscriptions",), False, manifests
        )
        self.assertIn("plakous-code-mode", native)
        self.assertIn("plakous-js-eval", native)
        self.assertIn("plakous-auth", native)
        self.assertNotIn("plakous-openrouter", native)
        store = feature_policy.expected_closure("plakous-store", ("sqlite",), False, manifests)
        self.assertEqual(store, {"plakous-store", "plakous-agent", "plakous-model", "plakous-tools"})
        sidecar = feature_policy.expected_closure("plakous-js-eval", ("embedded",), False, manifests)
        self.assertEqual(sidecar, {"plakous-js-eval"})
        self.assertIn("plakous-hosted", feature_policy.expected_closure("plak", (), True, manifests))


class DensityScopeTests(unittest.TestCase):
    """The layer-derived density scopes."""

    @classmethod
    def setUpClass(cls) -> None:
        cls.workspace = policy_module.load_workspace(ROOT)

    def test_every_workspace_package_has_exactly_one_scope(self) -> None:
        for package in self.workspace.packages.values():
            with self.subTest(package=package.name):
                self.assertIsNotNone(density_policy.scope_of(package))
        scoped_layers = [layer for layers in density_policy.SCOPE_LAYERS.values() for layer in layers]
        self.assertEqual(len(scoped_layers), len(set(scoped_layers)))
        self.assertEqual(set(scoped_layers), set(self.workspace.policy.layers))

    def test_scopes_follow_the_layer_metadata(self) -> None:
        scopes = {
            package.name: density_policy.scope_of(package)
            for package in self.workspace.packages.values()
        }
        self.assertEqual(scopes["plakous-model"], "l0")
        self.assertEqual(scopes["plakous"], "l0")
        self.assertEqual(scopes["plakous-openai"], "below_facade")
        self.assertEqual(scopes["plakous-code-mode"], "below_facade")
        self.assertEqual(scopes["plakous-hosted"], "host")
        self.assertEqual(scopes["plak"], "product")
        self.assertEqual(scopes["plakous-test-support"], "test")


if __name__ == "__main__":
    unittest.main()
