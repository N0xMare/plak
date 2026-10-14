import importlib.util
import re
import sys
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "tools/checks"))
SPEC = importlib.util.spec_from_file_location("check_prod_loc", ROOT / "tools/checks/check_prod_loc.py")
assert SPEC and SPEC.loader
LOC = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = LOC
SPEC.loader.exec_module(LOC)


class ResponsibilityReportTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.workspace = LOC.load_workspace(ROOT)

    def package(self, name: str):  # type: ignore[no-untyped-def]
        return self.workspace.packages[name]

    def test_unregistered_external_test_module_is_counted_fail_closed(self):
        with tempfile.TemporaryDirectory() as directory:
            crate = Path(directory)
            src = crate / "src"
            src.mkdir()
            (src / "lib.rs").write_text(
                "mod production;\n#[cfg(test)]\nmod test_support;\n",
                encoding="utf-8",
            )
            (src / "production.rs").write_text("pub fn live() {}\n", encoding="utf-8")
            (src / "test_support.rs").write_text(
                "fn only_for_tests() {}\n", encoding="utf-8"
            )

            total, files = LOC.crate_prod_loc(crate)

            self.assertEqual(total, 5)
            self.assertEqual(files, 3)

    def test_repository_tests_package_is_not_mistaken_for_src_test_module(self):
        self.assertFalse(
            LOC.is_excluded_src_file(Path("/repo/tests/harnesses/offline-crash/src/lib.rs"))
        )
        self.assertTrue(
            LOC.is_excluded_src_file(
                Path("/repo/tests/harnesses/offline-crash/src/tests/fixture.rs")
            )
        )

    def test_every_production_source_is_classified_once_and_reconciled(self):
        report = LOC.report(self.workspace)
        responsibilities = report["responsibility_report"]
        sources = responsibilities["sources"]
        paths = [source["path"] for source in sources]
        self.assertEqual(len(paths), len(set(paths)))
        self.assertEqual(set(responsibilities["totals"]), set(LOC.RESPONSIBILITIES))
        scopes = report["scopes"]
        self.assertEqual(
            responsibilities["scope_totals"],
            {scope: scopes[scope] for scope in LOC.PRODUCTION_SCOPES},
        )
        self.assertEqual(
            sum(responsibilities["totals"].values()),
            sum(scopes[scope] for scope in LOC.PRODUCTION_SCOPES),
        )
        self.assertEqual(report["library"], scopes["l0"] + scopes["below_facade"])
        self.assertNotIn("crates/plakous/src/test_model.rs", set(paths))
        self.assertNotIn("bin/plak/src/test_support.rs", set(paths))

    def test_registered_external_test_source_is_exactly_cfg_test_only(self):
        self.assertEqual(
            LOC.TEST_ONLY_EXTERNAL_SOURCES,
            {
                ("plak", "src/product_crash_tests/actual_host.rs"),
                ("plak", "src/product_crash_tests/fixture.rs"),
                ("plak", "src/product_crash_tests/mod.rs"),
                ("plak", "src/product_crash_tests/product_release.rs"),
                ("plak", "src/product_crash_tests/protocol.rs"),
                ("plak", "src/test_support.rs"),
                ("plakous", "src/test_model.rs"),
            },
        )
        facade = self.package("plakous")
        declaration = (ROOT / facade.relative("src/lib.rs")).read_text(encoding="utf-8")
        self.assertEqual(declaration.count("mod test_model;"), 1)
        self.assertRegex(declaration, r"#\[cfg\(test\)\]\s*mod test_model;")
        plak = (ROOT / self.package("plak").relative("src/lib.rs")).read_text(encoding="utf-8")
        self.assertRegex(plak, r"#\[cfg\(test\)\]\s*mod product_crash_tests;")
        self.assertRegex(plak, r"#\[cfg\(test\)\]\s*mod test_support;")
        for name, relative in LOC.TEST_ONLY_EXTERNAL_SOURCES:
            self.assertTrue((ROOT / self.package(name).relative(relative)).is_file(), relative)

    def test_narrow_ownership_rules_precede_broad_crate_rules(self):
        cases = {
            ("plakous-agent", "src/runtime/driver/mod.rs"): "runtime",
            ("plakous-agent", "src/event/projection.rs"): "event/presentation",
            ("plakous-js-eval", "src/lib.rs"): "runtime",
            ("plakous-code-mode", "src/lib.rs"): "runtime",
            ("plakous-agent", "src/manifest.rs"): "checkpoint/rehydration",
            ("plakous-agent", "src/session.rs"): "session/facade",
            ("plakous-store", "src/sqlite.rs"): "store",
            ("plakous-responses", "src/stream/kernel.rs"): "family kernels",
            ("plakous-openai", "src/model.rs"): "provider glue",
            ("plakous-openai", "src/native_auth.rs"): "auth",
            ("plakous-anthropic", "src/native_auth.rs"): "auth",
            ("plakous-xai", "src/native_auth.rs"): "auth",
            ("plak", "src/lib.rs"): "product",
            ("plakous-hosted", "src/runtime.rs"): "product",
            ("plakous-js-eval", "src/bin/plakous-js-host.rs"): "runtime",
            ("plakous-host-protocol", "src/lib.rs"): "product",
        }
        for (name, relative), expected in cases.items():
            with self.subTest(path=f"{name}:{relative}"):
                self.assertEqual(LOC.responsibility_for(self.package(name), relative), expected)

    def test_rules_name_existing_packages_and_sources(self):
        for _responsibility, sources in LOC.RESPONSIBILITY_RULES:
            for name, prefix in sources:
                with self.subTest(rule=f"{name}:{prefix}"):
                    package = self.package(name)
                    self.assertNotIn(package.layer, LOC.LAYER_RESPONSIBILITIES)
                    self.assertTrue((ROOT / package.relative(prefix)).exists())

    def test_every_workspace_package_is_metered(self):
        """No workspace package may sit outside every reported scope."""
        totals, by_package = LOC.scope_totals(self.workspace)
        metered = {directory for scope in by_package.values() for directory in scope}
        self.assertEqual(
            metered, {package.directory for package in self.workspace.packages.values()}
        )
        self.assertEqual(set(totals), set(LOC.SCOPES))

    def test_whole_file_cfg_test_bodies_are_excluded_and_really_test_only(self):
        matched = []
        for package in self.workspace.packages.values():
            if LOC.scope_of(package) not in LOC.PRODUCTION_SCOPES:
                continue
            src = ROOT / package.directory / "src"
            if not src.is_dir():
                continue
            for path in sorted(src.rglob("*.rs")):
                if not LOC.is_test_only_file_name(path.name):
                    continue
                matched.append(path.relative_to(ROOT).as_posix())
                self.assertTrue(LOC.is_excluded_src_file(path), path)
                text = path.read_text(encoding="utf-8")
                if path.name.startswith("rendered_"):
                    # Whole body is one `#[cfg(test)]` item.
                    self.assertIn("#[cfg(test)]", text, path)
                    self.assertNotIn("\npub fn ", text, path)
                else:
                    # `tests.rs` must be declared by `#[cfg(test)] mod tests;`
                    # or `#[cfg(all(test, unix))] mod tests;` (unix-only bodies).
                    parent = path.parent
                    # A crate root declares `mod tests;` too (e.g. a crate
                    # whose whole body is one module: plakous-py-eval).
                    declarers = [
                        parent / "mod.rs",
                        parent / "lib.rs",
                        parent.with_suffix(".rs"),
                    ]
                    declared = any(
                        candidate.is_file()
                        and re.search(
                            r"#\[cfg\((?:test|all\(test,\s*unix\))\)\]\s*(?:#\[[^\]]*\]\s*)*(?:pub(?:\([^)]*\))?\s+)?mod tests;",
                            candidate.read_text(encoding="utf-8"),
                        )
                        for candidate in declarers
                    )
                    self.assertTrue(declared, f"{path} is not a #[cfg(test)] mod tests; target")
        # Fail closed if the conventions stop matching anything at all.
        self.assertGreaterEqual(len(matched), 4, matched)

    def test_unknown_source_fails_closed(self):
        with self.assertRaisesRegex(ValueError, "unclassified production source"):
            LOC.responsibility_for(self.package("plakous-model"), "benches/new-peer-stack.rs")
        peer = LOC.Package(
            name="plakous-new-peer",
            manifest_path=ROOT / "crates/plakous/new-peer/Cargo.toml",
            directory="crates/plakous/new-peer",
            layer="protocol",
            package_class="published-internal",
            neutral=False,
            manifest={},
            metadata={},
        )
        with self.assertRaisesRegex(ValueError, "unclassified production source"):
            LOC.responsibility_for(peer, "src/lib.rs")


if __name__ == "__main__":
    unittest.main()
