"""Production sweep evidence and checkout custody against temporary Git repositories."""

from contextlib import ExitStack
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
    "bin/plakd/src/store/mod.rs",
    "bin/plakd/src/store/schema.rs",
    "crates/common/gateway-protocol/src/lib.rs",
    "crates/common/runner-protocol/src/retention.rs",
    "crates/core/agent/src/checkpoint_bundle.rs",
    "crates/orchestration/recursive-subagents-store/src/database.rs",
    "crates/store/src/sqlite.rs",
)
NOW = 1_800_000_000
TARGET = "x86_64-unknown-linux-musl"


class GitFixture(unittest.TestCase):
    def setUp(self):
        self.root = Path(self.enterContext(tempfile.TemporaryDirectory())).resolve()
        self.git("init", "-q")
        # Use the enclosing checkout's identity even inside the isolated tooling gate.
        name, email = (
            EVIDENCE.git(ROOT, "log", "-1", "--format=%an%x00%ae").decode().strip().split("\0")
        )
        self.git("config", "user.name", name)
        self.git("config", "user.email", email)
        self.git("config", "commit.gpgsign", "false")
        for original in (*SWEEP_FILES, *(ROOT / path for path in FORMAT_FILES)):
            path = self.root / original.relative_to(ROOT)
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(original.read_bytes())
        self.base = self.commit()
        self.git("update-ref", "refs/remotes/origin/main", self.base)
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
        (self.root / "other").touch()
        self.candidate = self.commit()

    def write(self, commit=None, **changes):
        commit = self.base if commit is None else commit
        fields = dict(
            receipt_version=1,
            kind="full",
            passed_at=NOW - 1,
            commit=commit,
            tree=EVIDENCE.tree_id(self.root, commit),
            target=TARGET,
            inventory="a" * 64,
            formats=EVIDENCE.formats(self.root, commit),
            **dict.fromkeys(EVIDENCE.COUNT_KEYS, 1),
        )
        fields.update(changes)
        self.receipt.write_text(json.dumps(fields))

    def check(self):
        return EVIDENCE.check(self.root, self.candidate, self.receipts, TARGET, now=NOW)[0]

    def assert_refused(self):
        with self.assertRaisesRegex(ValueError, "no passing strict sweep evidence"):
            self.check()

    def test_fresh_main_ancestor_and_exact_candidate(self):
        self.write(passed_at=NOW - EVIDENCE.MAX_AGE)
        self.assertEqual(self.check(), self.receipt)
        self.write(self.candidate, kind="merged", passed_at=NOW - EVIDENCE.MAX_AGE - 1)
        self.assertEqual(self.check(), self.receipt)

    def test_stale_main_ancestor(self):
        self.write(passed_at=NOW - EVIDENCE.MAX_AGE - 1)
        self.assert_refused()

    def test_main_commit_that_is_not_an_ancestor(self):
        self.git("checkout", "--detach", "-q", self.base)
        (self.root / "sibling").touch()
        sibling = self.commit()
        self.git("update-ref", "refs/remotes/origin/main", sibling)
        self.write(sibling)
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

    def test_missing_corrupt_and_nonqualifying_receipts(self):
        self.assert_refused()
        for content in (b"{\n", b"null", b"\xff"):
            self.receipt.write_bytes(content)
            self.assert_refused()
        for change in (
            {"kind": "shard"},
            {"tree": "0" * 40},
            {"target": "wrong-target"},
            {"passed_at": NOW + 1},
            {"wire_groups": 0},
        ):
            self.write(**change)
            self.assert_refused()
