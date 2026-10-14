"""Rule unions, safety widening, real diff replays and the shared diff source."""

from pathlib import Path
import contextlib
import io
import json
import re
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "tools/checks"))
import gate_plan as GATE
from changed_packages import changed_paths, package_dirs

DIRS = {
    "bin/plak": "plak", "crates/core/model": "plakous-model",
    "crates/common/http": "plakous-http", "crates/plakous": "plakous",
    "crates/harness/prompts": "plakous-prompts",
}

# Rule-unit tests isolate discovery. Separate scratch-source and real-tree
# tests exercise discovery, including its deliberately conservative widening.
READERS = {"docs/spec/07-config-reference.md": {"plak"},
           "configs/reference/config.toml": {"plak"}}

# Path lists of two real development diffs (`git diff --name-only --no-renames`),
# kept as data: a published or git-free tree has no history to replay. One
# touches the facade; the other is a doc/config-reader-heavy orchestration diff.
HISTORICAL_DIFFS = {
    "facade-diff": [
        "bin/plak/src/home/state.rs",
        "bin/plak/src/home/state_artifacts.rs",
        "bin/plak/src/lib.rs",
        "bin/plak/src/product_config.rs",
        "bin/plak/src/product_doctor.rs",
        "bin/plak/src/telemetry_store.rs",
        "bin/plak/tests/cli_contract.rs",
        "bin/plak/tests/telemetry_contract.rs",
        "configs/reference/agents/reviewer.toml",
        "configs/reference/agents/root-agent.toml",
        "configs/reference/agents/scout.toml",
        "configs/reference/agents/worker.toml",
        "crates/common/attempt/src/attempt/mod.rs",
        "crates/common/attempt/src/attempt/retry.rs",
        "crates/common/attempt/src/lib.rs",
        "crates/common/http/src/control.rs",
        "crates/common/http/src/transport.rs",
        "crates/core/agent/src/event/types.rs",
        "crates/core/agent/src/runtime/compaction/live.rs",
        "crates/core/agent/src/runtime/driver/compaction.rs",
        "crates/core/agent/src/runtime/driver/model.rs",
        "crates/core/agent/src/runtime/durable/tests.rs",
        "crates/core/agent/src/runtime/observation/mod.rs",
        "crates/core/agent/src/telemetry.rs",
        "crates/core/model/src/event.rs",
        "crates/core/model/src/model.rs",
        "crates/hosting/hosted/src/runtime/events.rs",
        "crates/orchestration/recursive-subagents/src/epoch.rs",
        "crates/orchestration/recursive-subagents/src/tree/lifecycle.rs",
        "crates/orchestration/recursive-subagents/src/tree/mod.rs",
        "crates/orchestration/recursive-subagents/src/tree/workspaces.rs",
        "crates/orchestration/workspace/src/occupancy_vcs.rs",
        "crates/plakous/src/lib.rs",
        "crates/protocols/chat-completions/Cargo.toml",
        "crates/protocols/chat-completions/src/http.rs",
        "crates/protocols/messages/src/http.rs",
        "crates/protocols/responses-dialect/Cargo.toml",
        "crates/protocols/responses-dialect/src/attempt_host.rs",
        "crates/protocols/responses-dialect/src/attempt_host_tests.rs",
        "crates/protocols/responses-dialect/src/model_host.rs",
        "crates/providers/anthropic/src/model_tests.rs",
        "crates/providers/anthropic/tests/local_transport.rs",
        "crates/providers/openai/src/model.rs",
        "crates/providers/openai/src/platform_local.rs",
        "crates/providers/openrouter/src/execution.rs",
        "crates/providers/openrouter/tests/multi_wire_loopback.rs",
        "crates/providers/xai/src/model.rs",
        "crates/providers/xai/src/platform_model.rs",
        "docs/spec/00-overview.md",
        "docs/spec/01-plakous-library.md",
        "docs/spec/02-plak-product.md",
        "docs/spec/03-orchestration.md",
        "docs/spec/04-security-and-boundaries.md",
        "docs/spec/07-config-reference.md",
        "docs/user-guide/cli.md",
        "docs/user-guide/configuration.md",
        "docs/user-guide/troubleshooting.md",
        "tools/checks/check_features.py",
        "tools/checks/check_workspace.py",
    ],
    "reader-diff": [
        "bin/plak/src/product_config.rs",
        "configs/reference/agents/reviewer.toml",
        "configs/reference/agents/root-agent.toml",
        "configs/reference/agents/scout.toml",
        "configs/reference/agents/worker.toml",
        "crates/orchestration/recursive-subagents/src/epoch.rs",
        "crates/orchestration/recursive-subagents/src/tree/lifecycle.rs",
        "crates/orchestration/recursive-subagents/src/tree/mod.rs",
        "crates/orchestration/recursive-subagents/src/tree/workspaces.rs",
        "crates/orchestration/workspace/src/occupancy_vcs.rs",
        "docs/spec/03-orchestration.md",
        "docs/spec/04-security-and-boundaries.md",
        "docs/spec/07-config-reference.md",
        "docs/user-guide/configuration.md",
    ],
}


class GatePlanTests(unittest.TestCase):
    def test_check_always_runs_gate_tooling_self_tests(self):
        result = subprocess.run(
            [sys.executable, str(ROOT / "tools/checks/gate_plan.py"), "--tier", "check"],
            check=True, text=True, capture_output=True,
        )
        self.assertIn("tooling", result.stdout.strip().split(","))

    def test_unconditional_tier_cli_uses_the_canonical_ordered_lists(self):
        for tier, groups in (("platform", GATE.PLATFORM_GROUPS),
                             ("check", GATE.CHECK_GROUPS), ("full", GATE.GROUPS)):
            with self.subTest(tier=tier):
                result = subprocess.run(
                    [sys.executable, str(ROOT / "tools/checks/gate_plan.py"), "--tier", tier],
                    check=True, text=True, capture_output=True,
                )
                self.assertEqual(result.stdout.strip().split(","), list(groups))

    def test_git_free_reader_inventory_matches_export_sources(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            sources = {"pkg/src/lib.rs", "pkg/tests/read.rs", "new/layout/source.rs",
                       "pkg/benches/reader.rs", "pkg/examples/reader.rs"}
            outputs = {"target/generated.rs", "pkg/target/generated.rs", "target/.git/generated.rs",
                       ".dev-tools/generated.rs", "pkg/__pycache__/generated.rs"}
            for file in sources | outputs:
                path = root / file
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text('const A: &str = include_str!(unresolved!());')
            with patch.object(GATE.subprocess, "check_output", side_effect=AssertionError("must not use git")):
                self.assertEqual(set(GATE.reader_source_files(root)), sources)
                refs, unknown = GATE.reader_inputs((("pkg", "P"),), root)
            self.assertFalse(refs)
            self.assertEqual(sorted(entry.split(":")[0] for entry in unknown),
                             ["pkg/src/lib.rs", "pkg/tests/read.rs"])

    @patch.object(GATE, "reader_inputs", return_value=({}, []))
    def test_gate_inputs_select_whole_check_and_tooling(self, _readers):
        for path in ("scripts/dev-check.sh", "scripts/dev-tools.sh", "Justfile",
                     ".config/nextest.toml",
                     "tools/checks/gate_plan.py", "tools/checks/changed_packages.py",
                     "tools/checks/budget_filterset.py", "tools/checks/check_workspace.py",
                     "tools/checks/check_features.py", "tools/checks/plakous_policy.py",
                     "tools/checks/check_public_compile.py"):
            with self.subTest(path=path):
                plan = GATE.make_plan([path], DIRS)
                self.assertTrue(plan.workspace)
                self.assertTrue(set(GATE.CHECK_GROUPS) | {"tooling"} <= set(plan.groups))
                self.assertTrue(all(plan.checker_reasons.values()))

    def test_edited_planner_cannot_hide_weakened_rust_rule(self):
        with tempfile.TemporaryDirectory() as temporary, patch.dict(
                "os.environ", {"PATH": "/usr/bin:/bin"}):
            root = Path(temporary)
            checks = root / "tools/checks"
            checks.mkdir(parents=True)
            for name in ("gate_plan.py", "changed_packages.py"):
                shutil.copyfile(ROOT / "tools/checks" / name, checks / name)
            source = root / "bin/plak/src/cli.rs"
            source.parent.mkdir(parents=True)
            source.write_text("// baseline\n")
            (root / ".gitignore").write_text("__pycache__/\n")
            def git(*args):
                return subprocess.check_output(["git", *args], cwd=root, text=True)
            git("init", "-q")
            git("config", "user.name", "Test")
            git("config", "user.email", "test@example.invalid")
            git("add", ".")
            git("commit", "-qm", "baseline")
            planner = checks / "gate_plan.py"
            planner.write_text(planner.read_text().replace(
                '"rust": ("builds", "tests-parallel", "tests-serial", "contracts"),',
                '"rust": (),'))
            source.write_text("// changed\n")
            result = subprocess.check_output([sys.executable, "-c", '''
import json, sys
sys.path.insert(0, sys.argv[1])
import gate_plan as g
g.reader_inputs = lambda *args: ({}, [])
g.artifact_dependencies = lambda *args: {}
paths = g.changed_paths(g.REPO_ROOT)
assert paths == ["bin/plak/src/cli.rs", "tools/checks/gate_plan.py"], paths
print(json.dumps(g.make_plan(paths,
                             {"bin/plak": "plak"}).as_dict()))
''', str(checks)], text=True, cwd=ROOT)
            plan = json.loads(result)
            self.assertTrue(plan["workspace"])
            self.assertTrue(set(GATE.CHECK_GROUPS) | {"tooling"} <= set(plan["groups"]))
            self.assertTrue(all(plan["checkers"].values()))

    def test_reader_literal_escapes_and_non_test_targets(self):
        with tempfile.TemporaryDirectory() as temporary, patch.dict(
                "os.environ", {"PATH": "/usr/bin:/bin"}):
            root = Path(temporary)
            subprocess.run(["git", "init", "-q"], cwd=root, check=True)
            source = root / "pkg/src/lib.rs"
            source.parent.mkdir(parents=True)
            (root / "docs").mkdir()
            (root / "docs/read.md").write_text("read")
            (root / "docs/bytes.md").write_text("bytes")
            for directory in ("benches", "examples"):
                excluded = root / "pkg" / directory / "reader.rs"
                excluded.parent.mkdir()
                excluded.write_text('include_str!(unresolved!()); "../../docs/only-excluded.md";')
            source.write_text('''const Q: char = '"'; include_str!("../../docs/read.md"); const R: char = '"';
const B: u8 = b'"'; include_bytes!("../../docs/bytes.md"); const C: u8 = b'"';
const L: char = '\\''; const N: char = '\\n'; const U: char = '\\u{22}';
fn lifetime<'a>(x: &'a str) -> &'a str { x }
''')
            subprocess.run(["git", "add", "."], cwd=root, check=True)
            dirs = (("pkg", "reader"),)
            refs, unknown = GATE.reader_inputs(dirs, root)
            self.assertEqual(refs["docs/read.md"], {"reader"})
            self.assertEqual(refs["docs/bytes.md"], {"reader"})
            self.assertNotIn("docs/only-excluded.md", refs)
            self.assertFalse(unknown, unknown)
            for value in ("{root}/docs/read.md",
                          "{root}/docs/{name}.md", "{root}/missing/shared.txt"):
                with self.subTest(value=value):
                    source.write_text(f'const INPUT: &str = "{value}";')
                    GATE.reader_inputs.cache_clear()
                    discovered = GATE.reader_inputs(dirs, root)
                    self.assertTrue(discovered[1], discovered)
                    with patch.object(GATE, "reader_inputs", return_value=discovered):
                        plan = GATE.make_plan(["docs/read.md"], dict(dirs))
                    self.assertTrue(plan.workspace)
                    self.assertTrue(set(GATE.CHECK_GROUPS) <= set(plan.groups))
                    self.assertTrue(all(plan.checker_reasons.values()))

    def test_root_only_literals_are_not_repository_readers(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "pkg/src/lib.rs"
            source.parent.mkdir(parents=True)
            source.write_text('"."; "alias/.."; "../..";')
            refs, unknown = GATE.reader_inputs((("pkg", "reader"),), root)
            self.assertEqual(refs, {})
            self.assertEqual(unknown, [])

    def test_embedded_repository_root_keeps_widening(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "pkg/src/lib.rs"
            source.parent.mkdir(parents=True)
            for expression in ('include_str!("../..")', 'include_bytes!("../..")',
                               '#[path = "../.."] mod external;'):
                with self.subTest(expression=expression):
                    source.write_text(expression)
                    GATE.reader_inputs.cache_clear()
                    refs, unknown = GATE.reader_inputs((("pkg", "reader"),), root)
                    self.assertNotIn(".", refs)
                    self.assertTrue(unknown)

    def test_template_directory_resolving_to_root_keeps_widening(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "pkg/src/lib.rs"
            source.parent.mkdir(parents=True)
            (root / "configs").mkdir()
            source.write_text('"configs/../{name}.md";')
            refs, unknown = GATE.reader_inputs((("pkg", "reader"),), root)
            self.assertNotIn(".", refs)
            self.assertTrue(unknown)

    def test_prose_and_non_path_templates_are_not_readers(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "pkg/src/lib.rs"
            source.parent.mkdir(parents=True)
            source.write_text('''"unknown command /{other} - try /help";
"docs/{name}.md: {error}"; "Read/docs/{name}!";
"https://{address}/docs/read.md"; "data:image/png;base64,{image}";
r#"{"path":"docs/{name}.md"}"#;
''')
            refs, unknown = GATE.reader_inputs((("pkg", "reader"),), root)
            self.assertEqual(refs, {})
            self.assertEqual(unknown, [])

    def test_templates_record_literal_directories_not_runtime_paths(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "pkg/src/lib.rs"
            source.parent.mkdir(parents=True)
            (root / "configs").mkdir()
            (root / "shared/data").mkdir(parents=True)
            source.write_text('''"configs/{preset}"; "shared/data/{name}.json";
"../../shared/data/{name}.json"; "docs/deleted/{name}.md";
"agents/{name}.toml"; "refs/heads/plak/{id}";
''')
            refs, unknown = GATE.reader_inputs((("pkg", "reader"),), root)
            self.assertEqual(refs, {"configs": {"reader"}, "shared/data": {"reader"},
                                    "docs/deleted": {"reader"}})
            self.assertEqual(unknown, [])
            with patch.object(GATE, "reader_inputs", return_value=(refs, unknown)), \
                    patch.object(GATE, "artifact_dependencies", return_value={}):
                plan = GATE.make_plan(["configs/preset.toml"], {"pkg": "reader"})
            self.assertFalse(plan.workspace)
            self.assertEqual(plan.packages, {"reader"})

    def test_current_reader_residue_and_recursive_subagents_scope(self):
        dirs = package_dirs()
        _, unknown = GATE.reader_inputs(tuple(sorted(dirs.items())))
        self.assertEqual(unknown, [])
        plan = GATE.make_plan([
            "crates/orchestration/recursive-subagents/src/tree/lifecycle.rs"], dirs)
        self.assertFalse(plan.workspace, plan.as_dict())
        self.assertEqual(plan.groups, ["static", "builds", "tests-parallel",
                                       "tests-serial", "contracts"])
        self.assertEqual(plan.packages, {"plakous-recursive-subagents"})
        self.assertEqual(plan.scope, "(rdeps(=plakous-recursive-subagents))")

    def test_current_core_and_doc_only_plans_are_scoped(self):
        dirs = package_dirs()
        core = GATE.make_plan(["crates/core/model/src/content.rs"], dirs)
        self.assertFalse(core.workspace)
        self.assertEqual(core.groups, ["static", "builds", "rustdoc", "tests-parallel",
                                       "tests-serial", "contracts"])
        self.assertEqual(core.scope, "(rdeps(=plakous-model))")
        docs = GATE.make_plan(["docs/user-guide/cli.md"], dirs)
        self.assertFalse(docs.workspace)
        self.assertEqual(docs.groups, ["static"])
        self.assertEqual(docs.packages, set())
        self.assertFalse(any(docs.checker_reasons.values()))

    def test_non_reader_inventory_matches_live_source_literals(self):
        for (file, value), rationale in GATE.NON_READER_TEMPLATES.items():
            with self.subTest(file=file, value=value):
                source = (ROOT / file).read_text()
                literals = [token.group() for token in GATE.RUST_TOKENS.finditer(source)
                            if token.group().startswith(('"', 'r'))]
                values = [literal[literal.index('"') + 1:literal.rindex('"')]
                          for literal in literals]
                self.assertIn(value, values, "stale non-reader inventory entry")
                self.assertTrue(rationale)

    def test_non_reader_exemptions_are_exact_and_never_apply_to_embedded_inputs(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "pkg/src/lib.rs"
            source.parent.mkdir(parents=True)
            value = "{root}/missing/shared.txt"
            inventory = {("pkg/src/lib.rs", value): "runtime path"}
            with patch.object(GATE, "NON_READER_TEMPLATES", inventory):
                source.write_text(f'"{value}";')
                refs, unknown = GATE.reader_inputs((("pkg", "reader"),), root)
                self.assertEqual((refs, unknown), ({}, []))
                for expression in (f'"{value}.other";', f'include_str!("{value}")'):
                    with self.subTest(expression=expression):
                        source.write_text(expression)
                        GATE.reader_inputs.cache_clear()
                        self.assertTrue(GATE.reader_inputs((("pkg", "reader"),), root)[1])
                source.write_text("")
                (source.parent / "other.rs").write_text(f'"{value}";')
                GATE.reader_inputs.cache_clear()
                self.assertTrue(GATE.reader_inputs((("pkg", "reader"),), root)[1])

    def test_adversarial_inputs(self):
        dirs = package_dirs()
        rows = (
            ("crates/common/auth/src/secret.rs", {"contracts", "builds"}, {"plakous-auth"}, True),
            ("crates/runtimes/py-worker/src/main.rs", {"contracts", "builds", "tests-serial"},
             {"plakous-py-worker", "plakous-py-eval", "plakous-hosted", "plak"}, True),
            ("crates/runtimes/js-runtime/src/lib.rs", {"contracts", "builds"},
             {"plakous-js-runtime", "plakous-js-eval", "plak"}, True),
            ("crates/core/model/tests/fixtures/canonical_item_v1.json", {"tests-parallel", "tests-serial"},
             {"plakous-model"}, False),
            ("tests/fixtures/model-consumer/notes.txt", {"contracts", "rustdoc"}, set(), True),
            ("tests/fixtures/model-consumer/.cargo/config.toml", {"contracts", "rustdoc"}, set(), True),
            ("configs/providers/grok/agents/root-agent.toml", {"tests-parallel", "tests-serial"}, {"plak"}, False),
            ("docs/spec/05-testing-and-evidence.md", {"tests-parallel", "tests-serial"}, {"plakous-tools"}, False),
            ("bin/plak/src/cli.rs", {"builds", "contracts"}, {"plak"}, True),
            ("crates/harness/prompts/pins.sha256", {"builds"}, {"plakous-prompts"}, False),
        )
        for path, groups, packages, checker in rows:
            with self.subTest(path=path):
                plan = GATE.make_plan([path], dirs)
                self.assertTrue(groups <= set(plan.groups), plan.as_dict())
                self.assertTrue(plan.workspace or packages <= plan.packages, plan.as_dict())
                self.assertEqual(bool(plan.checker_reasons["workspace"]), checker or plan.workspace)

    def test_artifact_consumer_completeness(self):
        dirs = package_dirs()
        metadata = json.loads(subprocess.check_output(
            ["cargo", "metadata", "--no-deps", "--locked", "--format-version", "1"], cwd=ROOT, text=True))
        dependencies = {p["name"]: {d["name"] for d in p["dependencies"]}
                        for p in metadata["packages"]}
        for artifact, markers in (
                ("crates/runtimes/py-worker/src/main.rs", ("PLAK_MONTY_WORKER", "python_bundle::locate")),
                ("crates/runtimes/js-runtime/src/lib.rs", ("PLAK_JS_HOST", "js_host::locate"))):
            artifact_owner = GATE.owner_of(artifact, dirs)
            closure = {artifact_owner}
            while True:
                widened = closure | {dep for name in closure for dep in dependencies.get(name, ())
                                     if dep in dependencies}
                if widened == closure:
                    break
                closure = widened
            files = GATE.reader_source_files(ROOT)
            consumers = {GATE.owner_of(file, dirs) for file in files
                         if GATE.is_reader_source(file, dirs)
                         and any(marker in (ROOT / file).read_text() for marker in markers)}
            for dependency in closure:
                directory = next(directory for directory, name in dirs.items() if name == dependency)
                with patch.object(GATE, "reader_inputs", return_value=({}, [])):
                    plan = GATE.make_plan([f"{directory}/src/lib.rs"], dirs)
                self.assertFalse(plan.workspace)
                covered = set(plan.packages)
                while True:
                    widened = covered | {name for name, deps in dependencies.items() if deps & covered}
                    if widened == covered:
                        break
                    covered = widened
                self.assertTrue(plan.workspace or consumers <= covered,
                                (dependency, consumers - covered))

    def test_reader_completeness(self):
        dirs = package_dirs()
        files = GATE.reader_source_files(ROOT)
        for file in files:
            owner = GATE.owner_of(file, dirs)
            if not GATE.is_reader_source(file, dirs):
                continue
            # Independent inventory: quoted references, not prose comments.
            for target in re.findall(r'"(?:\.\./)*((?:docs|configs)/[^"{}\n ]+)"', (ROOT / file).read_text()):
                if (ROOT / target).exists():
                    plan = GATE.make_plan([target], dirs)
                    self.assertTrue(plan.workspace or owner in plan.packages, (file, target))

    def test_reader_discovery_and_uncertainty(self):
        with tempfile.TemporaryDirectory() as temporary, patch.dict(
                "os.environ", {"PATH": "/usr/bin:/bin"}):
            root = Path(temporary)
            subprocess.run(["git", "init", "-q"], cwd=root, check=True)
            source = root / "pkg/src/lib.rs"
            source.parent.mkdir(parents=True)
            (root / "docs").mkdir()
            (root / "docs/read.md").write_text("read")
            (root / "shared").mkdir()
            (root / "shared/data.json").write_text("{}")
            source.write_text('''// "docs/not-a-reader.md"
const A: &str = include_str!("../../shared/data.json");
const B: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../docs/read.md"));
const C: &str = "../docs/deleted.md";
const D: &str = r#"../docs/read.md"#;
const E: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/generated"));
#[path = "../../shared/module.rs"] mod external;
''')
            (root / "shared/module.rs").write_text("")
            subprocess.run(["git", "add", "."], cwd=root, check=True)
            dirs = (("pkg", "reader"),)
            refs, unknown = GATE.reader_inputs(dirs, root)
            self.assertFalse(unknown, unknown)
            for target in ("shared/data.json", "shared/module.rs", "docs/read.md", "docs/deleted.md"):
                self.assertEqual(refs[target], {"reader"})
            self.assertNotIn("docs/not-a-reader.md", refs)
            source.write_text('const A: &str = include_str!(target!());')
            GATE.reader_inputs.cache_clear()
            self.assertTrue(GATE.reader_inputs(dirs, root)[1])
        with patch.object(GATE, "reader_inputs", return_value=({}, ["dynamic input"])):
            plan = GATE.make_plan(["docs/read.md"], DIRS)
            self.assertTrue(plan.workspace)
            self.assertTrue(set(GATE.CHECK_GROUPS) <= set(plan.groups))
            self.assertTrue(all(plan.checker_reasons.values()))

    def test_default_base_and_empty_warning(self):
        with tempfile.TemporaryDirectory() as temporary, patch.dict(
                "os.environ", {"PATH": "/usr/bin:/bin"}):
            root = Path(temporary)
            def git(*args):
                return subprocess.check_output(["git", *args], cwd=root, text=True).strip()
            git("init", "-qb", "main")
            git("config", "user.name", "Test")
            git("config", "user.email", "test@example.invalid")
            (root / "a").write_text("a")
            git("add", ".")
            git("commit", "-qm", "checked base")
            base = git("rev-parse", "HEAD")
            git("commit", "--allow-empty", "-qm", "next")
            self.assertEqual(GATE.default_base(root), base)
            git("checkout", "-qb", "topic")
            with self.assertRaisesRegex(ValueError, "explicit BASE"):
                GATE.default_base(root)
            git("commit", "--allow-empty", "-qm", "topic change")
            self.assertEqual(GATE.default_base(root), git("rev-parse", "main"))
            git("checkout", "--detach", "-q", base)
            with self.assertRaisesRegex(ValueError, "explicit BASE"):
                GATE.default_base(root)
            git("branch", "-D", "main")
            with self.assertRaisesRegex(ValueError, "explicit BASE"):
                GATE.default_base(root)
        stdout, stderr = io.StringIO(), io.StringIO()
        with patch.object(sys, "argv", ["gate_plan.py", "HEAD", "--shell"]), \
                patch.object(GATE, "changed_paths", return_value=[]), \
                patch.object(GATE, "package_dirs", return_value={}), \
                contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
            self.assertEqual(GATE.main(), 0)
        self.assertIn("WARNING: empty diff", stderr.getvalue())
        self.assertIn("changed plan against HEAD", stderr.getvalue())
        self.assertEqual(stdout.getvalue(), "static\n\nfalse\nfalse\n")
        stderr = io.StringIO()
        with patch.object(sys, "argv", ["gate_plan.py"]), \
                patch.object(GATE, "default_base", side_effect=ValueError("requires an explicit BASE")), \
                contextlib.redirect_stderr(stderr):
            self.assertNotEqual(GATE.main(), 0)
        self.assertIn("explicit BASE", stderr.getvalue())
        self.assertNotIn("Traceback", stderr.getvalue())

    @patch.object(GATE, "reader_inputs", return_value=(READERS, []))
    def test_rule_table(self, _readers):
        rows = (
            ("Cargo.toml", "cargo", "builds", True),
            ("Cargo.lock", "cargo", "tests-serial", True),
            ("crates/common/http/Cargo.toml", "cargo", "builds", True),
            (".cargo/config.toml", "cargo", "builds", True),
            ("rust-toolchain.toml", "cargo", "builds", True),
            (".config/nextest.toml", "cargo", "tests-parallel", True),
            ("deny.toml", "cargo", "builds", True),
            ("crates/common/http/src/lib.rs", "rust", "tests-parallel", False),
            ("crates/common/http/tests/data.json", "package-input", "tests-parallel", False),
            ("crates/plakous/src/lib.rs", "api", "contracts", False),
            ("crates/core/model/src/lib.rs", "api", "rustdoc", False),
            ("tests/fixtures/model-consumer/src/main.rs", "api", "contracts", False),
            ("tools/checks/check_public_compile.py", "api", "contracts", True),
            ("scripts/dev-check.sh", "gate", "tooling", True),
            ("tools/checks/gate_plan.py", "gate", "tooling", True),
            ("tests/tooling/test_gate_plan.py", "tooling", "tooling", False),
            ("Justfile", "gate", "tooling", True),
            ("docs/spec/07-config-reference.md", "readers", "tests-parallel", False),
            ("configs/reference/config.toml", "readers", "tests-parallel", False),
            ("crates/harness/prompts/pins.sha256", "prompts", "tests-serial", False),
            ("bin/plak/src/lib.rs", "durability", "tests-durability", False),
            ("unrecognised.txt", "unknown", "tests-serial", True),
        )
        self.assertEqual({row[1] for row in rows}, set(GATE.RULE_TABLE))
        for path, rule, group, workspace in rows:
            with self.subTest(path=path):
                plan = GATE.make_plan([path], DIRS)
                self.assertIn("static", plan.groups)
                self.assertIn(group, plan.groups)
                self.assertEqual(plan.workspace, workspace)
                self.assertTrue(any(reason.startswith(rule + ":") and path in reason
                                    for reason in plan.reasons[group]))

    @patch.object(GATE, "reader_inputs", return_value=(READERS, []))
    def test_unknown_paths_widen_to_whole_check(self, _readers):
        plan = GATE.make_plan(["README.md", "crates/common/http/tests/data.json"], DIRS)
        self.assertEqual(plan.groups, list(GATE.CHECK_GROUPS))
        self.assertTrue(plan.workspace)
        self.assertEqual(plan.scope, "")
        self.assertIn("README.md", str(plan.reasons))

    def test_lockfile_selects_all_packages(self):
        plan = GATE.make_plan(["Cargo.lock", "bin/plak/src/lib.rs"], DIRS)
        self.assertTrue(plan.workspace)
        self.assertEqual(plan.scope, "")
        self.assertIn("builds", plan.groups)

    @patch.object(GATE, "reader_inputs", return_value=(READERS, []))
    def test_doc_change_selects_doc_and_config_readers(self, _readers):
        plan = GATE.make_plan(["docs/user-guide/configuration.md"], DIRS)
        self.assertFalse(plan.workspace)
        self.assertEqual(plan.groups, ["static"])
        self.assertEqual(plan.scope, "")
        self.assertNotIn("tests-serial", plan.groups)

    @patch.object(GATE, "reader_inputs", return_value=(READERS, []))
    def test_test_only_package_selects_rdeps_in_both_lanes(self, _readers):
        plan = GATE.make_plan(["crates/common/http/tests/regression.rs"], DIRS)
        self.assertEqual(plan.groups, ["static", "builds", "tests-parallel", "tests-serial", "contracts"])
        self.assertIn("plakous-http", plan.packages)
        self.assertIn("rdeps(=plakous-http)", plan.scope)

    @patch.object(GATE, "reader_inputs", return_value=(READERS, []))
    def test_mixed_changes_union_rules_and_filters(self, _readers):
        plan = GATE.make_plan(["crates/common/http/src/lib.rs", "docs/spec/07-config-reference.md",
                               "Justfile", "crates/plakous/src/lib.rs"], DIRS)
        self.assertEqual(plan.groups, ["static", "builds", "rustdoc", "tests-parallel",
                                       "tests-serial", "tests-durability", "contracts", "tooling"])
        self.assertTrue({"plakous", "plakous-http", "plak"} <= plan.packages)
        self.assertTrue(plan.workspace)
        self.assertEqual(plan.scope, "")

    def test_empty_diff_still_checks_static(self):
        self.assertEqual(GATE.make_plan([], DIRS).groups, ["static"])

    @patch.object(GATE, "reader_inputs", return_value=(READERS, []))
    def test_checker_inputs(self, _readers):
        rows = (
            (["docs/a.md"], False, False),
            (["configs/reference/config.toml"], False, False),
            (["tools/checks/gate_plan.py", "Justfile"], True, True),
            (["crates/common/http/tests/regression.rs"], True, True),
            (["crates/common/http/Cargo.toml"], True, True),
            (["tests/fixtures/model-consumer/Cargo.toml"], True, True),
            (["tests/fixtures/model-consumer/Cargo.lock"], True, True),
            (["tests/fixtures/model-consumer/notes.txt"], True, False),
            (["tests/fixtures/model-consumer/.cargo/config.toml"], True, False),
            (["Cargo.lock"], True, True),
            ([".config/nextest.toml"], True, True),
            ([".cargo/config.toml"], False, True),
            (["rust-toolchain.toml"], False, True),
            (["deny.toml"], False, True),
            (["tools/checks/check_workspace.py"], True, True),
            (["tools/checks/check_features.py"], True, True),
            (["tools/checks/plakous_policy.py"], True, True),
            (["unknown-path"], True, True),
            (["docs/a.md", "crates/common/http/src/lib.rs"], True, True),
        )
        for paths, workspace, features in rows:
            with self.subTest(paths=paths):
                plan = GATE.make_plan(paths, DIRS)
                self.assertEqual(bool(plan.checker_reasons["workspace"]), workspace)
                self.assertEqual(bool(plan.checker_reasons["features"]), features)

    def test_replay_real_diffs(self):
        dirs = package_dirs()
        rows = (
            ("facade-diff", ["static", "builds", "rustdoc", "tests-parallel",
                          "tests-serial", "tests-durability", "contracts", "tooling"]),
            ("reader-diff", ["static", "builds", "tests-parallel", "tests-serial",
                          "tests-durability", "contracts"]),
        )
        for head, groups in rows:
            with self.subTest(head=head):
                paths = HISTORICAL_DIFFS[head]
                plan = GATE.make_plan(paths, dirs)
                self.assertEqual(plan.groups, groups)
                if head == "facade-diff":
                    self.assertTrue(plan.workspace)
                    self.assertIn("crates/plakous/src/lib.rs", str(plan.reasons))
                else:
                    self.assertFalse(plan.workspace)
                    self.assertTrue(plan.readers)
                    self.assertIn("plakous-recursive-subagents", plan.packages)
        # Replay a real test-only file from the first diff, without its other paths.
        plan = GATE.make_plan(["bin/plak/tests/telemetry_contract.rs"], dirs)
        self.assertEqual(plan.groups, ["static", "builds", "tests-parallel", "tests-serial",
                                       "tests-durability", "contracts"])
        self.assertFalse(plan.workspace)
        self.assertEqual(plan.scope, "(rdeps(=plak))")

    def test_diff_source_includes_staged_unstaged_untracked_and_deleted_paths(self):
        # Isolate PATH from product git wrappers and developer aliases.
        with tempfile.TemporaryDirectory() as temporary, patch.dict(
                "os.environ", {"PATH": "/usr/bin:/bin"}):
            root = Path(temporary)
            def git(*args):
                return subprocess.check_output(["git", *args], cwd=root, text=True)
            git("init", "-q")
            git("config", "user.name", "Test")
            git("config", "user.email", "test@example.invalid")
            for name in ("staged", "unstaged", "deleted", "renamed"):
                (root / name).write_text("initial-" + name)
            (root / ".gitignore").write_text("ignored\n")
            git("add", ".")
            git("commit", "-qm", "initial")
            base = git("rev-parse", "HEAD").strip()
            (root / "staged").write_text("changed")
            git("add", "staged")
            (root / "unstaged").write_text("changed")
            (root / "deleted").unlink()
            git("mv", "renamed", "moved")
            (root / "untracked\nname").write_text("new")
            (root / "untracked.rs").write_text("// new Rust source\n")
            (root / "ignored").write_text("ignored")
            self.assertEqual(changed_paths(root, base),
                             ["deleted", "moved", "renamed", "staged", "unstaged", "untracked\nname", "untracked.rs"])
            for invalid in ("missing-ref", "--quiet"):
                with self.subTest(base=invalid), self.assertRaises(subprocess.CalledProcessError):
                    changed_paths(root, invalid)

    @patch.object(GATE, "reader_inputs", return_value=(READERS, []))
    def test_durability_follows_owning_packages_only(self, _readers):
        dirs = package_dirs()
        owners = (
            ("bin/plak/src/product_crash_tests/actual_host.rs", "plak"),
            ("crates/plakous/src/sessions/tests.rs", "plakous"),
            ("crates/store/src/sqlite.rs", "plakous-store"),
            ("tests/harnesses/offline-crash/tests/crash_matrix.rs",
             "plakous-offline-crash-harness"),
        )
        for path, package in owners:
            with self.subTest(path=path):
                plan = GATE.make_plan([path], dirs)
                self.assertIn("tests-durability", plan.groups)
                self.assertLess(plan.groups.index("tests-serial"), plan.groups.index("tests-durability"))
                self.assertIn(f"rdeps(={package})", plan.scope)
                self.assertTrue(any(reason.startswith("durability:") and path in reason
                                    for reason in plan.reasons["tests-durability"]))
        plan = GATE.make_plan(["crates/common/http/src/lib.rs"], dirs)
        self.assertNotIn("tests-durability", plan.groups)
        plan = GATE.make_plan(["crates/runtimes/py-worker/src/main.rs"], dirs)
        self.assertNotIn("tests-durability", plan.groups)
        self.assertIn("plak", plan.packages)
        plan = GATE.make_plan(["docs/spec/07-config-reference.md"], DIRS)
        self.assertNotIn("tests-durability", plan.groups)
        self.assertIn("plak", plan.packages)
        plan = GATE.make_plan([".config/nextest.toml"], DIRS)
        self.assertNotIn("tests-durability", plan.groups)
        self.assertTrue(set(GATE.CHECK_GROUPS) <= set(plan.groups))
        self.assertNotIn("tests-durability", GATE.CHECK_GROUPS)
        self.assertIn("tests-durability", GATE.PLATFORM_GROUPS)
        self.assertIn("tests-durability", GATE.GROUPS)



if __name__ == "__main__":
    unittest.main()
