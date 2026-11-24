"""Production sweep evidence and checkout custody against temporary Git repositories."""

from contextlib import ExitStack
import gzip
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

from tests.tooling.test_crash_sweep import ROOT, SWEEP

EVIDENCE = SWEEP.evidence
SWEEP_FILES = (
    SWEEP.DRIVER,
    Path(SWEEP.shards.__file__),
    SWEEP.shards.INVENTORY,
    Path(EVIDENCE.__file__),
)
FORMAT_FILES = (
    "bin/plakd/src/carry/document.rs",
    "bin/plakd/src/home/state.rs",
    "bin/plakd/src/reload_request.rs",
    "bin/plakd/src/store/mod.rs",
    "bin/plakd/src/store/schema.rs",
    "bin/plakd/src/telemetry_store.rs",
    "crates/common/auth/src/api_key.rs",
    "crates/common/auth/src/private_store.rs",
    "crates/common/gateway-protocol/src/lib.rs",
    "crates/common/runner-protocol/src/retention.rs",
    "crates/core/agent/src/checkpoint_bundle.rs",
    "crates/core/agent/src/manifest.rs",
    "crates/core/model/src/history.rs",
    "crates/core/tools/src/policy.rs",
    "crates/orchestration/recursive-subagents-store/src/database.rs",
    "crates/store/src/sqlite.rs",
    "crates/store/src/sqlite/schema.rs",
)
TARGET = "x86_64-unknown-linux-musl"
MAIN = "refs/remotes/origin/main"


class GitFixture(unittest.TestCase):
    def setUp(self):
        self.root = Path(self.enterContext(tempfile.TemporaryDirectory())).resolve()
        self.git("init", "-q")
        self.git("config", "user.name", "Sweep Fixture")
        self.git("config", "user.email", "fixture@example.invalid")
        self.git("config", "commit.gpgsign", "false")
        for original in (*SWEEP_FILES, *(ROOT / path for path in FORMAT_FILES)):
            path = self.root / original.relative_to(ROOT)
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(original.read_bytes())
        self.base = self.commit()
        self.git("update-ref", MAIN, self.base)
        with ExitStack() as mocks:
            mocks.enter_context(patch.object(SWEEP, "ROOT", self.root))
            mocks.enter_context(patch.object(SWEEP, "DRIVER", self.path(SWEEP_FILES[0])))
            mocks.enter_context(
                patch.object(SWEEP.shards, "__file__", str(self.path(SWEEP_FILES[1])))
            )
            mocks.enter_context(patch.object(SWEEP.shards, "INVENTORY", self.path(SWEEP_FILES[2])))
            mocks.enter_context(patch.object(EVIDENCE, "__file__", str(self.path(SWEEP_FILES[3]))))
            self.enterContext(mocks.pop_all())

    def path(self, path):
        return self.root / path.relative_to(ROOT)

    def git(self, *args):
        return subprocess.check_output(["git", *args], cwd=self.root)

    def commit(self):
        self.git("add", ".")
        self.git("commit", "-qm", "Fixture snapshot")
        return self.git("rev-parse", "HEAD").decode().strip()


class CheckoutTests(GitFixture):
    def test_dirty_tracked_checkout(self):
        self.path(SWEEP_FILES[0]).write_text("changed")
        with self.assertRaisesRegex(ValueError, "requires a clean checkout"):
            SWEEP.clean_checkout()

    def test_untracked_file(self):
        (self.root / "untracked").touch()
        with self.assertRaisesRegex(ValueError, "including untracked files"):
            SWEEP.clean_checkout()

    def test_driver_differing_from_head_even_when_git_status_is_clean(self):
        self.git("update-index", "--assume-unchanged", str(SWEEP_FILES[0].relative_to(ROOT)))
        self.path(SWEEP_FILES[0]).write_text("changed")
        self.assertEqual(self.git("status", "--porcelain"), b"")
        with self.assertRaisesRegex(ValueError, "does not match"):
            SWEEP.clean_checkout()

    def test_revision_change_between_preflight_and_publication(self):
        before = SWEEP.clean_checkout()
        (self.root / "other").touch()
        self.commit()
        with self.assertRaisesRegex(ValueError, "changed while the strict sweep was running"):
            SWEEP.require_unchanged(before)

    def test_merge_compares_each_file_to_expected_commit(self):
        for original in SWEEP_FILES:
            with self.subTest(file=original.name):
                path = self.path(original)
                path.write_bytes(path.read_bytes() + b"\n")
                self.commit()
                SWEEP.clean_checkout()
                with self.assertRaisesRegex(ValueError, original.name):
                    SWEEP.verify_sweep_files(self.base)
                self.git("checkout", "--detach", "-q", self.base)


class EvidenceTests(GitFixture):
    def setUp(self):
        super().setUp()
        self.receipts = Path(self.enterContext(tempfile.TemporaryDirectory()))
        self.receipt = self.receipts / "passing.json"
        self.stamp = EVIDENCE.commit_time(self.root, self.base)
        self.now = self.stamp + EVIDENCE.MAX_AGE
        (self.root / "other").touch()
        self.candidate = self.commit()

    def write(self, commit=None, **changes):
        commit = self.base if commit is None else commit
        fields = dict(
            receipt_version=SWEEP.shards.RECEIPT_VERSION,
            kind="full",
            passed_at=self.now - 1,
            commit=commit,
            tree=EVIDENCE.tree_id(self.root, commit),
            target=TARGET,
            formats=EVIDENCE.formats(self.root, commit),
            **SWEEP.shards.expected_summary(lambda path: self.git("show", f"{commit}:{path}")),
        )
        fields.update(changes)
        self.receipt.write_text(json.dumps(fields))

    def check(self):
        return EVIDENCE.check(self.root, self.candidate, self.receipts, MAIN, now=self.now)[0]

    def assert_refused(self):
        with self.assertRaisesRegex(ValueError, "no passing strict sweep evidence"):
            self.check()

    def test_fresh_main_ancestor_and_exact_candidate(self):
        self.write(passed_at=self.stamp)
        self.assertEqual(self.check(), self.receipt)
        self.write(self.candidate, kind="merged", passed_at=self.stamp - 1)
        self.assertEqual(self.check(), self.receipt)

    def test_stale_main_ancestor(self):
        self.write(passed_at=self.stamp - 1)
        self.assert_refused()

    def test_fresh_coverage_cannot_renew_an_old_main_commit(self):
        self.write()
        self.now += 1
        self.assert_refused()

    def test_completion_cannot_precede_ancestor_commit(self):
        self.write(passed_at=self.stamp - 1)
        self.now -= 1
        self.assert_refused()

    def test_missing_main_ref(self):
        self.write()
        self.git("update-ref", "-d", MAIN)
        with self.assertRaisesRegex(ValueError, "main ref .* is missing; fetch origin main first"):
            self.check()

    def test_force_moved_main_ref(self):
        self.write()
        self.assertEqual(self.check(), self.receipt)
        self.git("checkout", "-q", "--orphan", "replacement")
        self.git("update-ref", MAIN, self.commit())
        self.assert_refused()

    def test_main_commit_that_is_not_an_ancestor(self):
        self.git("checkout", "--detach", "-q", self.base)
        (self.root / "sibling").touch()
        sibling = self.commit()
        self.git("update-ref", MAIN, sibling)
        self.write(sibling)
        self.assert_refused()

    def test_ddl_change_without_revision(self):
        self.write()
        before = EVIDENCE.formats(self.root, self.base)
        for key, name in (
            ("product", "bin/plakd/src/store/schema.rs"),
            ("tree", "crates/orchestration/recursive-subagents-store/src/database.rs"),
            ("runtime", "crates/store/src/sqlite/schema.rs"),
        ):
            with self.subTest(schema=key):
                self.git("checkout", "-q", "--detach", self.base)
                path = self.root / name
                path.write_text(
                    path.read_text().replace("CREATE TABLE ", "CREATE TABLE IF NOT EXISTS ", 1)
                )
                self.candidate = self.commit()
                after = EVIDENCE.formats(self.root, self.candidate)
                self.assertEqual(after[f"{key}_revision"], before[f"{key}_revision"])
                self.assertNotEqual(after[f"{key}_schema_sha256"], before[f"{key}_schema_sha256"])
                self.assert_refused()

    def test_ancestor_not_on_main(self):
        self.write(self.candidate)
        (self.root / "another").touch()
        self.candidate = self.commit()
        self.assert_refused()

    def test_format_change(self):
        self.write()
        path = self.root / "crates/common/runner-protocol/src/retention.rs"
        old = EVIDENCE.formats(self.root, self.base)["runner_retention_version"]
        path.write_text(
            path.read_text().replace(
                f"const VERSION: u32 = {old};", f"const VERSION: u32 = {old + 1};"
            )
        )
        self.candidate = self.commit()
        self.assert_refused()

    def test_additive_product_revision_is_detected_with_the_same_state_key(self):
        self.write()
        path = self.root / "bin/plakd/src/store/schema.rs"
        path.write_text(
            path.read_text().replace(
                "revisions: &[],",
                'revisions: &[\n        Revision {\n            sql: "CREATE INDEX ...",\n        },\n    ],',
            )
        )
        self.candidate = self.commit()
        before = EVIDENCE.formats(self.root, self.base)
        after = EVIDENCE.formats(self.root, self.candidate)
        self.assertEqual(after["state_key"], before["state_key"])
        self.assertEqual(after["product_revision"], before["product_revision"] + 1)
        self.assert_refused()

    def test_named_revision_binds_the_referenced_sql(self):
        path = self.root / "bin/plakd/src/store/schema.rs"
        source = path.read_text()
        path.write_text(
            'const ADDED_INDEX: &str = "CREATE INDEX added ON sessions(id)";\n'
            "const ADD_INDEX: Revision = Revision {\n"
            "    sql: ADDED_INDEX,\n"
            '    objects: &[("added", ADDED_INDEX)],\n'
            "};\n" + source.replace("revisions: &[],", "revisions: &[ADD_INDEX],")
        )
        revision = self.commit()
        before = EVIDENCE.formats(self.root, self.base)
        named = EVIDENCE.formats(self.root, revision)
        self.assertEqual(named["product_revision"], before["product_revision"] + 1)
        self.assertEqual(named["state_key"], before["state_key"])
        path.write_text(path.read_text().replace("CREATE INDEX added", "CREATE UNIQUE INDEX added"))
        changed = EVIDENCE.formats(self.root, self.commit())
        self.assertEqual(changed["product_revision"], named["product_revision"])
        self.assertNotEqual(changed["product_schema_sha256"], named["product_schema_sha256"])

    def test_missing_corrupt_and_nonqualifying_receipts(self):
        self.assert_refused()
        for content in (b"{\n", b"null", b"\xff"):
            self.receipt.write_bytes(content)
            self.assert_refused()
        for change in (
            {"kind": "shard"},
            {"tree": "0" * 40},
            {"target": "wrong-target"},
            {"passed_at": self.now + 1},
            {"wire_groups": 0},
            {"wire_groups": 31},
            {"inventory": "a" * 64},
            {"runners": ["not_a_runner"]},
        ):
            self.write(**change)
            self.assert_refused()

    def test_ancestor_coverage_uses_its_own_inventory_and_selectors(self):
        self.write()
        path = self.path(SWEEP_FILES[2])
        rows = json.loads(path.read_text())
        rows["indexed"].pop(next(iter(rows["indexed"])))
        path.write_text(json.dumps(rows))
        selectors = self.path(SWEEP_FILES[1])
        for line in selectors.read_text().splitlines():
            if '"runner_hold_poll":' in line:
                selectors.write_text(selectors.read_text().replace(line, ""))
                break
        self.candidate = self.commit()
        self.assertEqual(self.check(), self.receipt)


class ReceiptWriteTests(unittest.TestCase):
    def test_full_and_shard_receipts_are_readable_and_never_overwritten(self):
        with tempfile.TemporaryDirectory() as scratch:
            directory = Path(scratch)
            for name, compressed in (("full.json", False), ("shard.json.gz", True)):
                with self.subTest(receipt=name):
                    path = directory / name
                    EVIDENCE.write_receipt(path, {"passed_at": 100}, compressed=compressed)
                    with self.assertRaises(FileExistsError):
                        EVIDENCE.write_receipt(path, {"passed_at": 200}, compressed=compressed)
                    data = gzip.decompress(path.read_bytes()) if compressed else path.read_bytes()
                    self.assertEqual(json.loads(data), {"passed_at": 100})
            self.assertEqual(
                sorted(path.name for path in directory.iterdir()), ["full.json", "shard.json.gz"]
            )
