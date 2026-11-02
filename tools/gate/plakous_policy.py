#!/usr/bin/env python3
"""Workspace layering policy shared by the structure checkers.

The policy lives in two places, both exposed by `cargo metadata`:

* every workspace manifest carries `[package.metadata.plakous]` with its
  `layer`, its support `class`, and an optional `neutral = true`; a loaded
  manifest has its `workspace = true` dependencies resolved against the root
  `[workspace.dependencies]`, so checkers read each edge as Cargo does;
* the root `Cargo.toml` carries `[workspace.metadata.plakous]`: the layer list,
  the layer-pair edge matrix, explicit today-only `edge-exceptions`, the
  per-layer `layout` path templates, the legal `classes` per layer, the exact
  `production-dependencies` of every host/bin package, the facade features a
  host/bin package enables, and the `modules` intra-crate rules.

Nothing here reads a hardcoded path: package directories come from the
manifest paths Cargo reports, so a package move is a manifest move plus a
`layout` template change, never a checker edit.
"""

from __future__ import annotations

import fnmatch
import json
import subprocess
import tomllib
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parents[2]

FAMILY_PREFIXES = ("plakous-", "plak-")
PACKAGE_CLASSES = ("supported", "published-internal", "repository-only")

# Layers whose packages sit strictly below the facade and are reachable by a
# consumer only through `plakous` (spec 00 "the crate-set rule").
BELOW_FACADE_LAYERS = ("common", "engine", "protocol", "provider", "store", "harness")
# L0: the four-crate supported core plus the facade that re-exports it.
L0_LAYERS = ("core", "facade")
# Layers above the facade: Plakous host crates and the product binaries.
# Every one carries the host/bin pins (production dependencies, facade features,
# security and session authority, the facade-only symbol rule, module rules).
ABOVE_FACADE_LAYERS = ("host", "bin")


def short_name(name: str) -> str:
    """Return the package's family-stripped short name (`plakous-model` -> `model`)."""
    for prefix in FAMILY_PREFIXES:
        if name.startswith(prefix) and len(name) > len(prefix):
            return name[len(prefix) :]
    return name


@dataclass(frozen=True)
class Package:
    """One workspace member with its `[package.metadata.plakous]` facts."""

    name: str
    manifest_path: Path
    directory: str
    layer: str
    package_class: str
    neutral: bool
    manifest: dict[str, Any]
    metadata: dict[str, Any]

    @property
    def short(self) -> str:
        return short_name(self.name)

    def relative(self, path: Path | str) -> str:
        """Return a repository path relative to this package's directory."""
        return (Path(self.directory) / path).as_posix()

    def production_dependency_names(self) -> set[str]:
        """Direct normal dependencies (optional ones included), by package name."""
        return {
            dependency["name"]
            for dependency in self.metadata.get("dependencies", [])
            if dependency.get("kind") is None
        }

    def dependency_names(self) -> set[str]:
        return {dependency["name"] for dependency in self.metadata.get("dependencies", [])}


@dataclass(frozen=True)
class Policy:
    """`[workspace.metadata.plakous]` as loaded from the root manifest."""

    layers: tuple[str, ...]
    edges: dict[str, tuple[str, ...]]
    edge_exceptions: dict[str, tuple[str, ...]]
    layout: dict[str, tuple[str, ...]]
    classes: dict[str, tuple[str, ...]]
    production_dependencies: dict[str, tuple[str, ...]]
    facade_features: dict[str, tuple[str, ...]]
    harness_pack_independent: tuple[str, ...] = ()
    modules: dict[str, dict[str, dict[str, Any]]] = field(default_factory=dict)

    def allowed_targets(self, layer: str) -> tuple[str, ...]:
        return self.edges.get(layer, ())


@dataclass(frozen=True)
class Workspace:
    """The loaded workspace: policy plus every member package."""

    root: Path
    policy: Policy
    packages: dict[str, Package]
    root_manifest: dict[str, Any]

    def in_layers(self, *layers: str) -> list[Package]:
        return [
            package
            for package in sorted(self.packages.values(), key=lambda item: item.name)
            if package.layer in layers
        ]

    def layer_of(self, name: str) -> str | None:
        package = self.packages.get(name)
        return None if package is None else package.layer


def cargo_metadata(root: Path = ROOT) -> dict[str, Any]:
    """Load locked, offline, dependency-free Cargo metadata for the workspace."""
    output = subprocess.check_output(
        ["cargo", "metadata", "--format-version", "1", "--no-deps", "--locked", "--offline"],
        cwd=root,
        text=True,
    )
    return json.loads(output)


def _string_tuple(value: object, label: str) -> tuple[str, ...]:
    if not isinstance(value, list) or not all(isinstance(item, str) for item in value):
        raise ValueError(f"workspace policy {label} must be a list of strings")
    return tuple(value)


def _string_table(value: object, label: str) -> dict[str, tuple[str, ...]]:
    if not isinstance(value, dict):
        raise ValueError(f"workspace policy {label} must be a table")
    return {key: _string_tuple(item, f"{label}.{key}") for key, item in value.items()}


def load_policy(root_manifest: dict[str, Any]) -> Policy:
    """Parse and shape-check `[workspace.metadata.plakous]`."""
    table = root_manifest.get("workspace", {}).get("metadata", {}).get("plakous")
    if not isinstance(table, dict):
        raise ValueError("root Cargo.toml lacks [workspace.metadata.plakous]")
    layers = _string_tuple(table.get("layers"), "layers")
    edges = _string_table(table.get("edges", {}), "edges")
    layout = _string_table(table.get("layout", {}), "layout")
    classes = _string_table(table.get("classes", {}), "classes")
    for name, keyed in (("edges", edges), ("layout", layout), ("classes", classes)):
        unknown = sorted(set(keyed) - set(layers))
        if unknown:
            raise ValueError(f"workspace policy {name} names unknown layers {unknown!r}")
        missing = sorted(set(layers) - set(keyed))
        if missing:
            raise ValueError(f"workspace policy {name} lacks rows for layers {missing!r}")
    for layer, targets in edges.items():
        for target in targets:
            base = target.split(":", 1)[0]
            if target != "*" and base not in layers:
                raise ValueError(f"workspace policy edges.{layer} names unknown layer {target!r}")
    for layer, legal in classes.items():
        unknown = sorted(set(legal) - set(PACKAGE_CLASSES))
        if unknown:
            raise ValueError(f"workspace policy classes.{layer} names unknown classes {unknown!r}")
    modules_value = table.get("modules", {})
    if not isinstance(modules_value, dict):
        raise ValueError("workspace policy modules must be a table")
    modules: dict[str, dict[str, dict[str, Any]]] = {}
    for package_name, package_modules in modules_value.items():
        if not isinstance(package_modules, dict):
            raise ValueError(f"workspace policy modules.{package_name} must be a table")
        modules[package_name] = {}
        for module_name, rule in package_modules.items():
            if not isinstance(rule, dict):
                raise ValueError(
                    f"workspace policy modules.{package_name}.{module_name} must be a table"
                )
            modules[package_name][module_name] = {
                "forbid-crates": _string_tuple(
                    rule.get("forbid-crates", []),
                    f"modules.{package_name}.{module_name}.forbid-crates",
                ),
                "allow-crates": _string_tuple(
                    rule.get("allow-crates", []),
                    f"modules.{package_name}.{module_name}.allow-crates",
                ),
                "may-use": _string_tuple(
                    rule.get("may-use", []),
                    f"modules.{package_name}.{module_name}.may-use",
                ),
            }
    return Policy(
        layers=layers,
        edges=edges,
        edge_exceptions=_string_table(table.get("edge-exceptions", {}), "edge-exceptions"),
        layout=layout,
        classes=classes,
        production_dependencies=_string_table(
            table.get("production-dependencies", {}), "production-dependencies"
        ),
        facade_features=_string_table(table.get("facade-features", {}), "facade-features"),
        harness_pack_independent=_string_tuple(
            table.get("harness-pack-independent", []), "harness-pack-independent"
        ),
        modules=modules,
    )


def package_metadata(manifest: dict[str, Any], name: str) -> tuple[str, str, bool]:
    """Return (layer, class, neutral) from `[package.metadata.plakous]`."""
    table = manifest.get("package", {}).get("metadata", {}).get("plakous")
    if not isinstance(table, dict):
        raise ValueError(f"{name}: manifest lacks [package.metadata.plakous]")
    layer = table.get("layer")
    package_class = table.get("class")
    neutral = table.get("neutral", False)
    if not isinstance(layer, str):
        raise ValueError(f"{name}: [package.metadata.plakous] layer must be a string")
    if not isinstance(package_class, str):
        raise ValueError(f"{name}: [package.metadata.plakous] class must be a string")
    if not isinstance(neutral, bool):
        raise ValueError(f"{name}: [package.metadata.plakous] neutral must be a boolean")
    unknown = sorted(set(table) - {"layer", "class", "neutral"})
    if unknown:
        raise ValueError(f"{name}: unknown [package.metadata.plakous] keys {unknown!r}")
    return layer, package_class, neutral


DEPENDENCY_SECTIONS = ("dependencies", "dev-dependencies", "build-dependencies")


def inherit_workspace_dependencies(
    manifest: dict[str, Any], workspace_dependencies: dict[str, Any],
) -> dict[str, Any]:
    """Return the manifest with each `workspace = true` dependency resolved.

    Cargo's inheritance: the member's `features` extend the workspace entry's,
    the member's `optional` and `default-features` apply over it.
    """

    def resolve(alias: str, spec: Any) -> Any:
        if not isinstance(spec, dict) or spec.get("workspace") is not True:
            return spec
        shared = workspace_dependencies.get(alias)
        if shared is None:
            raise ValueError(f"{alias}: inherits a dependency [workspace.dependencies] lacks")
        resolved = {"version": shared} if isinstance(shared, str) else dict(shared)
        for key, value in spec.items():
            if key == "features":
                resolved["features"] = [*resolved.get("features", []), *value]
            elif key != "workspace":
                resolved[key] = value
        return resolved

    def resolve_tables(table: dict[str, Any]) -> dict[str, Any]:
        resolved = dict(table)
        for section in DEPENDENCY_SECTIONS:
            if isinstance(table.get(section), dict):
                resolved[section] = {
                    alias: resolve(alias, spec) for alias, spec in table[section].items()
                }
        return resolved

    resolved = resolve_tables(manifest)
    if isinstance(manifest.get("target"), dict):
        resolved["target"] = {
            target: resolve_tables(table) for target, table in manifest["target"].items()
        }
    return resolved


def load_workspace(root: Path = ROOT, graph: dict[str, Any] | None = None) -> Workspace:
    """Load the policy and every workspace member from Cargo metadata."""
    graph = cargo_metadata(root) if graph is None else graph
    with (root / "Cargo.toml").open("rb") as stream:
        root_manifest = tomllib.load(stream)
    policy = load_policy(root_manifest)
    members = set(graph["workspace_members"])
    packages: dict[str, Package] = {}
    for metadata in graph["packages"]:
        if metadata["id"] not in members:
            continue
        manifest_path = Path(metadata["manifest_path"]).resolve()
        with manifest_path.open("rb") as stream:
            manifest = inherit_workspace_dependencies(
                tomllib.load(stream),
                root_manifest.get("workspace", {}).get("dependencies", {}),
            )
        layer, package_class, neutral = package_metadata(manifest, metadata["name"])
        packages[metadata["name"]] = Package(
            name=metadata["name"],
            manifest_path=manifest_path,
            directory=manifest_path.parent.relative_to(root.resolve()).as_posix(),
            layer=layer,
            package_class=package_class,
            neutral=neutral,
            manifest=manifest,
            metadata=metadata,
        )
    return Workspace(root=root, policy=policy, packages=packages, root_manifest=root_manifest)


def expand_layout_template(template: str, package: Package) -> str:
    """Substitute `{short}` and `{name}` into one layout template."""
    return template.replace("{short}", package.short).replace("{name}", package.name)


def layout_matches(policy: Policy, package: Package) -> bool:
    """Return whether the package directory satisfies its layer's layout templates."""
    for template in policy.layout.get(package.layer, ()):
        pattern = expand_layout_template(template, package)
        if pattern.endswith("/**"):
            if package.directory.startswith(pattern[:-2]):
                return True
        elif fnmatch.fnmatchcase(package.directory, pattern):
            return True
    return False


def edge_permitted(policy: Policy, source: Package, target: Package) -> bool:
    """Return whether the layer matrix (or an explicit exception) admits an edge."""
    if target.name in policy.edge_exceptions.get(source.name, ()):
        return True
    for allowed in policy.allowed_targets(source.layer):
        if allowed == "*":
            return True
        layer, _, only_package = allowed.partition(":")
        if layer != target.layer:
            continue
        if not only_package or only_package == target.name:
            return True
    return False
