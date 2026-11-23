"""Read stored formats and check completed strict sweep evidence."""

import json
from pathlib import Path
import re
import subprocess
import tempfile
import time


def git(root, *args):
    return subprocess.check_output(["git", *args], cwd=root)


def revision(root, ref):
    return (
        git(root, "rev-parse", "--verify", "--end-of-options", f"{ref}^{{commit}}").decode().strip()
    )


def tree_id(root, commit):
    return git(root, "rev-parse", f"{commit}^{{tree}}").decode().strip()


def formats(root, commit):
    def source(path):
        return git(root, "show", f"{commit}:{path}").decode()

    def one(text, pattern, description):
        matches = re.findall(pattern, text, re.M)
        if len(matches) != 1:
            raise ValueError(f"cannot read {description} at {commit}")
        return matches[0]

    def constant(path, name):
        return int(one(source(path), rf"^pub(?:\(crate\))? const {name}: \w+ = (\d+);$", name))

    def sqlite_revision(path, generation):
        text = source(path)
        schema = one(text, r"(?s)const SCHEMA: Schema = Schema \{(.*?)\n\};", path)
        one(schema, r"^    generation: GENERATION,$", f"{path} generation")
        revisions = one(schema, r"(?s)^    revisions: (.*),\s*\Z", f"{path} revisions")
        if revisions.startswith("&["):
            steps = one(revisions, r"(?s)^&\[(.*)\]$", f"{path} inline revisions")
        elif re.fullmatch(r"\w+", revisions):
            steps = one(
                text,
                rf"(?s)^const {revisions}: &\[Revision\] = &\[(.*?)\];",
                f"{path} revision list",
            )
        else:
            raise ValueError(f"cannot read {path} revisions at {commit}")
        # Schema::version() adds one for each additive revision.
        steps = re.sub(r"(?m)^ *//[^\n]*", "", steps)
        remainder, count = re.subn(r"(?ms)^( *)Revision \{.*?^\1\},?[ \t]*$", "", steps)
        if remainder.strip():
            raise ValueError(f"cannot count {path} revisions at {commit}")
        return generation + count

    runtime = constant("crates/store/src/sqlite.rs", "SQLITE_SCHEMA_VERSION")
    epoch = constant("crates/core/agent/src/checkpoint_bundle.rs", "RUNTIME_FORMAT_EPOCH")
    product = constant("bin/plakd/src/store/mod.rs", "GENERATION")
    tree_store = "crates/orchestration/recursive-subagents-store/src/database.rs"
    tree = constant(tree_store, "GENERATION")
    state_key = one(
        source("bin/plakd/src/home/state.rs"),
        r'(?m)^        "([^"\n]+)",\n'
        r"        plakous::sessions::RUNTIME_STORE_SCHEMA_VERSION,\n"
        r"        plakous::sessions::RUNTIME_FORMAT_EPOCH,\n"
        r"        crate::store::GENERATION,\n"
        r"        plakous_recursive_subagents_store::GENERATION,",
        "state key format",
    ).format(runtime, epoch, product, tree)
    return {
        "state_key": state_key,
        "runtime_revision": runtime,
        "product_revision": sqlite_revision("bin/plakd/src/store/schema.rs", product),
        "tree_revision": sqlite_revision(tree_store, tree),
        "export_version": constant("bin/plakd/src/carry/document.rs", "VERSION"),
        "runner_retention_version": constant(
            "crates/common/runner-protocol/src/retention.rs", "VERSION"
        ),
        "gateway_version": constant(
            "crates/common/gateway-protocol/src/lib.rs", "PROTOCOL_VERSION"
        ),
    }


def passed(identity, kind, counts):
    return dict(identity, receipt_version=1, kind=kind, passed_at=int(time.time()), **counts)


COUNT_KEYS = (
    "indexed_cuts",
    "indexed_shapes",
    "runner_cases",
    "wire_groups",
    "wire_invariant_rows",
)
MAX_AGE = 24 * 60 * 60


def write_receipt(path, receipt):
    path.parent.mkdir(parents=True, exist_ok=True)
    stream = tempfile.NamedTemporaryFile(dir=path.parent, delete=False, mode="w", encoding="utf-8")
    pending = Path(stream.name)
    try:
        with stream:
            json.dump(receipt, stream, indent=2)
            stream.write("\n")
        path.hardlink_to(pending)
    finally:
        pending.unlink(missing_ok=True)


def check(root, candidate, directory, target, *, now=None):
    now = int(time.time()) if now is None else now
    candidate = revision(root, candidate)
    expected_formats = formats(root, candidate)
    main = None
    for path in sorted(directory.glob("*.json")):
        try:
            receipt = json.loads(path.read_text(encoding="utf-8"))
            if (
                receipt["receipt_version"] != 1
                or receipt["kind"] not in ("full", "merged")
                or receipt["target"] != target
                or receipt["formats"] != expected_formats
                or any(
                    type(receipt["formats"][k]) is not type(v) for k, v in expected_formats.items()
                )
                or not all(type(receipt[key]) is int and receipt[key] > 0 for key in COUNT_KEYS)
                or not re.fullmatch(r"[0-9a-f]{64}", receipt["inventory"])
            ):
                continue
            commit, completed = receipt["commit"], receipt["passed_at"]
            if (
                not re.fullmatch(r"[0-9a-f]{40}", commit)
                or type(completed) is not int
                or not 0 < completed <= now
            ):
                continue
            if commit != candidate:
                if now - completed > MAX_AGE:
                    continue
                if main is None:
                    listing = subprocess.run(
                        ["git", "rev-list", "--first-parent", "refs/remotes/origin/main"],
                        cwd=root,
                        capture_output=True,
                        check=False,
                    )
                    main = (
                        set(listing.stdout.decode().splitlines())
                        if not listing.returncode
                        else set()
                    )
                if (
                    commit not in main
                    or subprocess.run(
                        ["git", "merge-base", "--is-ancestor", commit, candidate],
                        cwd=root,
                        check=False,
                    ).returncode
                ):
                    continue
            if receipt["tree"] == tree_id(root, commit) and (
                commit == candidate or formats(root, commit) == expected_formats
            ):
                return path, receipt
        except (OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError):
            continue
    raise ValueError(f"no passing strict sweep evidence for {candidate}")
