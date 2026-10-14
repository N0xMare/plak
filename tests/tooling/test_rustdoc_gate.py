"""The gate runs rustdoc, with rustdoc's own lints denied.

`cargo clippy` does not build documentation, so broken intra-doc links, links
from public documentation into private items, and bare URLs are invisible to
every other layer of `scripts/dev-check.sh`. This file pins the one invocation
that sees them:

1. the script has a `== rustdoc ==` step that runs `cargo doc` over the whole
   workspace, all features, `--locked`, and `--no-deps` (dependency docs are
   not ours to keep warning-free);
2. that invocation carries `RUSTDOCFLAGS="-D warnings"`, which is what turns
   rustdoc's warn-by-default lints into gate failures;
3. the step sits after clippy and before nextest, so it fails fast; and
4. it belongs to the `--full` tier (`just full`), not the per-commit tier.
"""

import re
import sys
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
DEV_CHECK = ROOT / "scripts/dev-check.sh"

RUSTDOC_STEP = re.compile(
    r'^\s*RUSTDOCFLAGS="-D warnings" (cargo doc\b[^\n]*)$', re.MULTILINE
)
sys.path.insert(0, str(ROOT / 'tools/checks'))
from gate_plan import GROUPS


class RustdocGate(unittest.TestCase):
    """`scripts/dev-check.sh` keeps the rustdoc layer."""

    def setUp(self) -> None:
        self.script = DEV_CHECK.read_text(encoding="utf-8")

    def test_rustdoc_invocation_denies_warnings(self) -> None:
        """One `cargo doc` line, prefixed with `RUSTDOCFLAGS="-D warnings"`."""
        matches = RUSTDOC_STEP.findall(self.script)
        self.assertEqual(
            len(matches), 1, f"expected one denied-warnings cargo doc line, got {matches}"
        )
        invocation = matches[0]
        for flag in ("--workspace", "--no-deps", "--all-features", "--locked"):
            self.assertIn(flag, invocation, f"{flag} missing from: {invocation}")
        self.assertEqual(
            self.script.count("cargo doc"),
            1,
            "a second cargo doc invocation could run without -D warnings",
        )

    def test_step_is_announced(self) -> None:
        """The step prints the `== rustdoc ==` banner the other layers use."""
        self.assertIn('echo "== rustdoc =="', self.script)

    def test_runs_after_clippy_and_before_the_tests(self) -> None:
        """Ordering: clippy, then rustdoc, then nextest."""
        clippy = self.script.index("cargo clippy")
        rustdoc = self.script.index("cargo doc")
        nextest = self.script.index("cargo nextest run")
        self.assertLess(clippy, rustdoc, "rustdoc must run after clippy")
        self.assertLess(rustdoc, nextest, "rustdoc must fail fast, before the tests")

    def test_rustdoc_runs_in_the_full_tier(self) -> None:
        """The shared group inventory selects rustdoc only in full/explicit plans."""
        self.assertIn('  --full) tier=full; selection="$(IFS=,; echo "${all_groups[*]}")"', self.script)
        self.assertIn('    rustdoc)', self.script)
        self.assertIn('rustdoc', GROUPS)
        self.assertLess(GROUPS.index('static'), GROUPS.index('rustdoc'))
        self.assertLess(GROUPS.index('rustdoc'), GROUPS.index('tests-parallel'))



if __name__ == "__main__":
    unittest.main()
