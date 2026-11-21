"""The nextest lane profiles partition the offline suite.

`.config/nextest.toml` defines the serial and core durability filters.
The install families live in the install catalog. The generated offline
filter excludes exactly their union.
"""

from __future__ import annotations

from functools import cache
import json
import re
import subprocess
import sys
import tomllib
import unittest
from collections.abc import Callable
from pathlib import Path
import tempfile

ROOT = Path(__file__).resolve().parents[2]
CONFIG = ROOT / ".config/nextest.toml"
SOURCE_ROOTS = ("crates", "bin", "tests")
PREDICATE = re.compile(r"test\(([^)]+)\)")
sys.path.insert(0, str(ROOT / "tools/gate"))
from check_workspace import _inline_modules, all_rust_tokens  # noqa: E402
from changed_packages import INSTALL_FAMILY_PATHS  # noqa: E402
from nextest_layout import SPECIAL_LANES, install_catalog, sync_membership  # noqa: E402
import vcs_listing  # noqa: E402


@cache
def profiles() -> dict:
    with CONFIG.open("rb") as handle:
        return tomllib.load(handle)["profile"]


def predicates(profile: str) -> list[str]:
    return PREDICATE.findall(profiles()[profile]["default-filter"])


def module_overmatches(names: list[str], modules: set[str]) -> list[tuple[str, str]]:
    """Substring module predicates must not capture a suffix of another module."""
    return sorted(
        (name, module)
        for name in names
        if re.fullmatch(r"[a-z0-9_]+::", name)
        for module in modules
        if module.endswith("_" + name[:-2])
    )


def predicate_is_defined(
    name: str, functions: set[str], modules: set[str], qualified: set[str], exact: set[str]
) -> bool:
    if name.startswith("="):
        return name[1:] in exact
    if name.endswith("::"):
        return name[:-2] in modules or any(path.startswith(name) for path in exact)
    if "::" in name:
        return name in qualified
    return name in functions or name in modules or any(test_match(name, path) for path in exact)


def exact_test_paths(path: Path, text: str, prefix: tuple[str, ...] | None = None) -> set[str]:
    parts = path.with_suffix("").parts
    if prefix is not None:
        pass
    elif "src" in parts:
        prefix = parts[parts.index("src") + 1 :]
        if prefix[-1] in {"lib", "main", "mod"}:
            prefix = prefix[:-1]
    elif "tests" in parts:
        # Each tests/<name>.rs or tests/<name>/main.rs is a separate binary.
        prefix = parts[parts.index("tests") + 2 :]
        if prefix and prefix[-1] in {"main", "mod"}:
            prefix = prefix[:-1]
    else:
        return set()
    tokens = all_rust_tokens(text)
    inline = _inline_modules(tokens)
    found = set()
    for i, token in enumerate(tokens[:-1]):
        if token.value != "fn":
            continue
        start = i - 1
        while start >= 0 and tokens[start].value not in {"{", "}", ";"}:
            start -= 1
        attributes = " ".join(t.value for t in tokens[start + 1 : i])
        if re.search(r"# \[ (?:[a-z_]+ :: )*(?:test|rstest|test_case)(?: \]| \()", attributes):
            found.add("::".join((*prefix, *inline[i], tokens[i + 1].value)))
    return found


def target_tests(entry: Path) -> set[str]:
    found: set[str] = set()
    visited: set[tuple[Path, tuple[str, ...]]] = set()

    def visit(path: Path, prefix: tuple[str, ...]) -> None:
        path = path.resolve()
        if not path.is_file() or (path, prefix) in visited:
            return
        visited.add((path, prefix))
        text = path.read_text()
        found.update(exact_test_paths(path, text, prefix))
        tokens = all_rust_tokens(text)
        inline = _inline_modules(tokens)
        for i, token in enumerate(tokens[:-2]):
            if token.value != "mod" or tokens[i + 2].value != ";":
                continue
            name = tokens[i + 1].value
            start = i - 1
            while start >= 0 and tokens[start].value not in {"{", "}", ";"}:
                start -= 1
            attributes = tokens[start + 1 : i]
            explicit = next(
                (
                    attributes[j + 2].value
                    for j in range(len(attributes) - 2)
                    if attributes[j].value == "path"
                    and attributes[j + 1].value == "="
                    and attributes[j + 2].kind == "string"
                ),
                None,
            )
            if explicit:
                child = path.parent.joinpath(*inline[i], explicit)
            else:
                base = path.parent if path.stem in {"lib", "main", "mod"} else path.with_suffix("")
                child = base.joinpath(*inline[i], name).with_suffix(".rs")
                if not child.is_file():
                    child = base.joinpath(*inline[i], name, "mod.rs")
            visit(child, (*prefix, *inline[i], name))

    visit(entry, ())
    return found


@cache
def target_inventory(root: Path) -> dict[tuple[str, str, str], set[str]]:
    metadata = json.loads(
        subprocess.check_output(
            ["cargo", "metadata", "--no-deps", "--format-version", "1"], cwd=root
        )
    )
    return {
        (package["name"], kind, target["name"]): target_tests(Path(target["src_path"]))
        for package in metadata["packages"]
        for target in package["targets"]
        if target["test"]
        for kind in target["kind"]
    }


def exact_inventory(root: Path) -> set[str]:
    return set().union(*target_inventory(root).values())


def test_matcher(selector: str) -> Callable[[str], bool]:
    if selector.startswith("="):
        exact = selector[1:]
        return lambda name: name == exact
    if selector.startswith("/") and selector.endswith("/"):
        search = re.compile(selector[1:-1]).search
        return lambda name: search(name) is not None
    return lambda name: selector in name


def test_match(selector: str, name: str) -> bool:
    return test_matcher(selector)(name)


class NextestLaneTests(unittest.TestCase):
    def test_qualified_module_predicates_require_a_test_below_that_module(self) -> None:
        exact = {"host::runner::foreground::runs"}
        self.assertTrue(
            predicate_is_defined("host::runner::foreground::", set(), set(), set(), exact)
        )
        self.assertFalse(
            predicate_is_defined("host::runner::missing::", set(), set(), set(), exact)
        )
        self.assertFalse(predicate_is_defined("host::runner::fore::", set(), set(), set(), exact))

    def test_exact_paths_follow_binary_roots_and_explicit_module_names(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "src").mkdir()
            (root / "Cargo.toml").write_text('[package]\nname="probe"\nversion="0.0.0"\n')
            (root / "src/lib.rs").write_text('#[path="different.rs"] mod named;')
            (root / "src/different.rs").write_text('mod nested { #[test] fn runs() {} }')
            self.assertEqual(exact_inventory(root), {"named::nested::runs"})

    def test_exact_predicates_require_an_existing_definition(self) -> None:
        functions = {"real_test"}
        modules = {"real_module"}
        qualified = {"real_module::real_test", "real_test"}
        exact = {"outer::real_module::real_test", "root_test"}
        for name in exact:
            self.assertTrue(predicate_is_defined("=" + name, functions, modules, qualified, exact))
        for name in ("real_module", "real_test", "real_module::real_test", "missing_test"):
            self.assertFalse(predicate_is_defined("=" + name, functions, modules, qualified, exact))
        text = "#[test] fn root_test() {} mod nested { #[tokio::test] async fn runs() {} fn helper() {} }"
        self.assertEqual(
            exact_test_paths(Path("bin/p/tests/integration.rs"), text),
            {"root_test", "nested::runs"},
        )
        self.assertEqual(
            exact_test_paths(Path("bin/p/tests/integration/outer.rs"), text),
            {"outer::root_test", "outer::nested::runs"},
        )
        self.assertEqual(
            exact_test_paths(Path("crates/p/src/lib.rs"), text), {"root_test", "nested::runs"}
        )

    def test_module_predicates_cannot_capture_prefixed_modules(self) -> None:
        self.assertEqual(
            module_overmatches(["application::"], {"application", "terminal_application"}),
            [("application::", "terminal_application")],
        )
        modules = set()
        for root in SOURCE_ROOTS:
            for path in (ROOT / root).rglob("*.rs"):
                modules.add(path.stem)
                modules.update(re.findall(r"\bmod\s+([a-z0-9_]+)\s*[;{]", path.read_text()))
        self.assertEqual(
            module_overmatches(predicates("serial") + predicates("durability"), modules), []
        )

    def test_generated_offline_exclusion_partitions_the_suite(self) -> None:
        source, catalog = CONFIG.read_text(), install_catalog(CONFIG)
        self.assertEqual(set(catalog), set(INSTALL_FAMILY_PATHS))
        self.assertEqual(source, sync_membership(source, catalog), "run nextest_layout.py --sync")
        selectors = {name: predicates(name) for name in SPECIAL_LANES}
        offline = profiles()["offline"]["default-filter"]
        self.assertRegex(offline, r"^\s*not \(")
        self.assertEqual(
            predicates("offline"), [term for lane in selectors.values() for term in lane]
        )
        membership: dict[str, str] = {}
        inventory = exact_inventory(ROOT)
        for name, terms in selectors.items():
            self.assertTrue(terms, name)
            self.assertEqual(len(terms), len(set(terms)), f"duplicate {name} predicate")
            body = profiles()[name]["default-filter"]
            self.assertEqual(re.sub(r"[|()]", "", PREDICATE.sub("", body)).split(), [], name)
            matchers = [test_matcher(term) for term in terms]
            matches = {test for test in inventory if any(match(test) for match in matchers)}
            self.assertFalse(
                membership.keys() & matches,
                f"{name} overlaps an earlier lane: {sorted(membership.keys() & matches)}",
            )
            membership.update(dict.fromkeys(matches, name))

    def test_each_install_trigger_matches_a_tracked_path(self) -> None:
        # Source exports contain only staged source files and have no Git index.
        if vcs_listing.kind(ROOT) == "git":
            paths = vcs_listing.run(ROOT, "git", "ls-files", "-z", "--cached").split("\0")
        else:
            paths = vcs_listing.listed_files(ROOT)
        for family, patterns in INSTALL_FAMILY_PATHS.items():
            for pattern in patterns:
                with self.subTest(family=family, pattern=pattern):
                    matches = re.compile(pattern).search
                    self.assertTrue(any(matches(path) for path in paths))

    def test_lane_profiles_keep_reviewed_thread_counts_and_budget_timeout(self) -> None:
        self.assertEqual(profiles()["serial"]["test-threads"], 4)
        self.assertEqual(profiles()["durability"]["test-threads"], 16)
        self.assertEqual(profiles()["install"]["test-threads"], 16)
        for name in SPECIAL_LANES:
            profile = profiles()[name]
            self.assertEqual(profile["slow-timeout"], {"period": "240s", "terminate-after": 2})

        def no_retry_setting(value):
            if isinstance(value, dict):
                self.assertNotIn("retries", value)
                for nested in value.values():
                    no_retry_setting(nested)
            elif isinstance(value, list):
                for nested in value:
                    no_retry_setting(nested)

        no_retry_setting(tomllib.loads(CONFIG.read_text()))

    def test_durability_weights_protect_internal_workers_and_pilot_isolation(self) -> None:
        heavy, first = profiles()["durability"]["overrides"]
        former_serial, isolated, image, ledger, install_first = profiles()["install"]["overrides"]
        self.assertEqual(former_serial["threads-required"], "num-test-threads")
        self.assertEqual(former_serial["priority"], -50)
        self.assertEqual(
            isolated,
            {
                "filter": "test(=crash_sweep::durability_ledger_retirement_closed_cells)",
                "threads-required": "num-test-threads",
                "priority": 100,
            },
        )
        self.assertEqual(heavy["threads-required"], "num-test-threads")
        self.assertEqual(heavy["priority"], -50)
        self.assertEqual(ledger["threads-required"], 4)
        self.assertEqual(ledger["priority"], 95)
        self.assertIn("durability_subset", heavy["filter"])
        self.assertIn("durability_twin_pilots", heavy["filter"])
        self.assertIn("image_residency_crash_cuts", image["filter"])
        self.assertEqual(image["threads-required"], "num-test-threads")
        self.assertEqual(first["priority"], 90)
        self.assertEqual(first["threads-required"], 4)
        self.assertIn("host_crash_cuts_of_the_start_scan_continue_once_or_hold", first["filter"])
        for override in (heavy, first, former_serial, isolated, image, ledger, install_first):
            self.assertLessEqual(-100, override.get("priority", 0))
            self.assertLessEqual(override.get("priority", 0), 100)
        for family, rule in (
            ("wire_host_restart", first),
            ("wire_host_restart_switch", install_first),
        ):
            pattern = rf"::{family}::(root|child)_[0-7]$"
            self.assertIn(f"test(/{pattern}/)", rule["filter"])
            self.assertEqual(rule["priority"], 90)
            self.assertEqual(rule["threads-required"], 4)
            for role in ("root", "child"):
                for group in range(8):
                    self.assertRegex(f"prefix::{family}::{role}_{group}", pattern)
            self.assertNotRegex(f"prefix::{family}", pattern)
            self.assertNotRegex(f"prefix::{family}::root_8", pattern)

    def test_restart_families_cover_every_macro_generated_group(self) -> None:
        directory = ROOT / "bin/plakd/tests/integration/host_skeleton/crash"
        groups = exact_test_paths(
            Path("restart_matrix.rs"), (directory / "restart_matrix.rs").read_text(), ()
        )
        expected = {f"{role}_{group}" for role in ("root", "child") for group in range(8)}
        self.assertEqual({name for name in groups if re.match(r"^(root|child)_", name)}, expected)
        modules = (directory / "wire/cuts.rs").read_text()
        for family in ("wire_host_restart", "wire_host_restart_switch"):
            self.assertRegex(modules, rf"mod {family} \{{[^}}]*restart_matrix::matrix_tests!")
            lane = "install" if family.endswith("_switch") else "durability"
            for group in expected:
                name = f"host_skeleton::crash::wire::cuts::{family}::{group}"
                self.assertTrue(any(test_match(term, name) for term in predicates(lane)), name)

    def test_each_mac_filter_clause_has_a_test_in_its_selected_target(self) -> None:
        inventory = target_inventory(ROOT)
        filters = ROOT / "tools/gate/mac-selection.filter"
        # Inside a clause, only the test regex may contain a union.
        for line in filters.read_text().splitlines():
            if not line or line.startswith("#"):
                continue
            with self.subTest(clause=line):
                clause = line.removeprefix("(").removesuffix(")") if line.startswith("(") else line
                selected = inventory.items()
                pattern = None
                for term in clause.split(" & "):
                    op, _, selector = term.removesuffix(")").partition("(")
                    if op == "test":
                        pattern = selector
                    else:
                        index = {"package": 0, "kind": 1, "binary": 2}[op]
                        selected = [
                            (key, tests)
                            for key, tests in selected
                            if test_match(selector, key[index])
                        ]
                self.assertTrue(
                    any(
                        pattern is None or test_match(pattern, test)
                        for _, tests in selected
                        for test in tests
                    )
                )

    def test_every_lane_predicate_names_a_test_or_module_in_the_tree(self) -> None:
        names = [term for lane in SPECIAL_LANES for term in predicates(lane)]
        functions: set[str] = set()
        modules: set[str] = set()
        qualified: set[str] = set()
        exact = exact_inventory(ROOT)
        for root in SOURCE_ROOTS:
            for path in (ROOT / root).rglob("*.rs"):
                text = path.read_text()
                file_functions = set(re.findall(r"\bfn\s+([a-z0-9_]+)\s*\(", text))
                functions |= file_functions
                modules |= set(
                    re.findall(
                        r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+([a-z0-9_]+)\s*[;{]",
                        text,
                        re.MULTILINE,
                    )
                )
                module_path = (
                    path.parent.parts
                    if path.stem in {"main", "mod"}
                    else path.with_suffix("").parts
                )
                for name in names:
                    name = name.removeprefix("=")
                    if "::" in name and not name.endswith("::"):
                        prefix, function = name.rsplit("::", 1)
                        parts = tuple(prefix.split("::"))
                        if module_path[-len(parts) :] == parts and function in file_functions:
                            qualified.add(name)
        missing = [
            name
            for name in names
            if not predicate_is_defined(name, functions, modules, qualified, exact)
        ]
        self.assertEqual(missing, [], f"lane predicates with no definition under {SOURCE_ROOTS}")


if __name__ == "__main__":
    unittest.main()
