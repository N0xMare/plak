"""The nextest lane profiles partition the offline suite.

`.config/nextest.toml` names the serial (wall-clock budget) and durability
members once each, as those profiles' `default-filter`, and the `offline`
(parallel) profile excludes exactly their union. A predicate that drifts out
of the exclusion would run a budget test under parallel load; one that only
the exclusion names would never run at all.
"""

from __future__ import annotations

import re
import sys
import tomllib
import unittest
from pathlib import Path
import tempfile

ROOT = Path(__file__).resolve().parents[2]
CONFIG = ROOT / ".config/nextest.toml"
SOURCE_ROOTS = ("crates", "bin", "tests")
PREDICATE = re.compile(r"test\(([^)]+)\)")
sys.path.insert(0, str(ROOT / "tools/gate"))
from check_workspace import _inline_modules, all_rust_tokens  # noqa: E402


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
        return name[:-2] in modules
    if "::" in name:
        return name in qualified
    return name in functions or name in modules


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


def exact_inventory(root: Path) -> set[str]:
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

    root_manifest = root / "Cargo.toml"
    workspace = tomllib.loads(root_manifest.read_text()).get("workspace", {})
    members = workspace.get("members", [])
    manifests = (
        {path / "Cargo.toml" for member in members for path in root.glob(member)}
        if members
        else {root_manifest}
    )
    for manifest in manifests:
        data = tomllib.loads(manifest.read_text())
        if "package" not in data:
            continue
        package = manifest.parent
        entries = {package / "src/lib.rs", package / "src/main.rs"}
        for kind in ("lib", "bin", "test"):
            items = data.get(kind, [])
            if isinstance(items, dict):
                items = [items]
            entries.update(package / item["path"] for item in items if "path" in item)
        for directory in (package / "tests", package / "src/bin"):
            entries.update(directory.glob("*.rs"))
            entries.update(directory.glob("*/main.rs"))
        for entry in entries:
            visit(entry, ())
    return found


class NextestLaneTests(unittest.TestCase):
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

    def test_offline_excludes_exactly_the_serial_and_durability_lanes(self) -> None:
        serial, durability = predicates("serial"), predicates("durability")
        self.assertTrue(serial and durability)
        self.assertEqual(set(serial) & set(durability), set())
        for lane in (serial, durability):
            self.assertEqual(len(lane), len(set(lane)), "duplicate lane predicate")
        offline = profiles()["offline"]["default-filter"]
        self.assertRegex(offline, r"^\s*not \(")
        self.assertEqual(predicates("offline"), serial + durability)
        for name in ("serial", "durability"):
            body = profiles()[name]["default-filter"]
            self.assertEqual(PREDICATE.sub("", body).replace("|", "").split(), [], name)

    def test_lane_profiles_are_single_threaded_with_the_budget_timeout(self) -> None:
        for name in ("serial", "durability"):
            profile = profiles()[name]
            self.assertEqual(profile["test-threads"], 1, name)
            self.assertEqual(profile["slow-timeout"], {"period": "240s", "terminate-after": 2})
        self.assertNotIn("retries", CONFIG.read_text())

    def test_every_lane_predicate_names_a_test_or_module_in_the_tree(self) -> None:
        names = predicates("serial") + predicates("durability")
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
