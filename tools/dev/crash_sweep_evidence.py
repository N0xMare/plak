"""Read stored formats and check completed strict sweep evidence."""

from functools import cache
import gzip
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import time

import crash_sweep_shards as shards


SHIPPED_TARGET = "x86_64-unknown-linux-musl"
MAX_AGE = 24 * 60 * 60


def git(root, *args):
    return subprocess.check_output(["git", *args], cwd=root)


def revision(root, ref):
    return (
        git(root, "rev-parse", "--verify", "--end-of-options", f"{ref}^{{commit}}").decode().strip()
    )


def tree_id(root, commit):
    return git(root, "rev-parse", f"{commit}^{{tree}}").decode().strip()


def commit_time(root, commit):
    return int(git(root, "show", "-s", "--format=%ct", commit))


def formats(root, commit):
    @cache
    def source(path):
        return git(root, "show", f"{commit}:{path}").decode()

    def one(text, pattern, description):
        matches = re.findall(pattern, text, re.M)
        if len(matches) != 1:
            raise ValueError(f"cannot read {description} at {commit}")
        return matches[0]

    def constant(path, name):
        return int(one(source(path), rf"^(?:pub(?:\(crate\))? )?const {name}: \w+ = (\d+);$", name))

    def sqlite_identity(path, generation):
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
        # Schema::version() adds one for each additive revision, inline or named.
        without_comments = re.sub(r"(?m)^ *//[^\n]*", "", steps)
        remainder, count = re.subn(r"(?ms)^( *)Revision \{.*?^\1\},?[ \t]*$", "", without_comments)
        if not re.fullmatch(r"\s*(?:[A-Z][A-Z_0-9]*\s*,\s*)*(?:[A-Z][A-Z_0-9]*\s*)?", remainder):
            raise ValueError(f"cannot count {path} revisions at {commit}")
        names = re.findall(r"[A-Z][A-Z_0-9]*", remainder)
        definitions = [
            one(
                text,
                rf"(?s)^(?:pub(?:\(\w+\))? )?const {name}: Revision = Revision \{{(.*?)^\}};",
                f"{path} revision {name}",
            )
            for name in names
        ]
        one(schema, r"^    objects: SCHEMA_OBJECTS,$", f"{path} schema object reference")
        objects = one(text, r"(?s)const SCHEMA_OBJECTS: &\[\(&str, &str\)\] = &\[(.*?)\n\];", path)
        references = set(re.findall(r"\b[A-Z][A-Z_0-9]*\b", "\n".join((objects, steps, *definitions))))
        sql = [
            declaration
            for declaration, name in re.findall(
                r'^((?:pub(?:\(\w+\))? )?const ([A-Z][A-Z_0-9]*): &str = "[^\n]*";)$', text, re.M
            )
            if name in references
        ]
        return generation + count + len(names), hashlib.sha256(
            "\0".join((schema, objects, steps, *definitions, *sql)).encode()
        ).hexdigest()

    runtime = constant("crates/store/src/sqlite.rs", "SQLITE_SCHEMA_VERSION")
    epoch = constant("crates/core/agent/src/checkpoint_bundle.rs", "RUNTIME_FORMAT_EPOCH")
    product = constant("bin/plakd/src/store/mod.rs", "GENERATION")
    tree_store = "crates/orchestration/recursive-subagents-store/src/database.rs"
    tree = constant(tree_store, "GENERATION")
    product_revision, product_ddl = sqlite_identity("bin/plakd/src/store/schema.rs", product)
    tree_revision, tree_ddl = sqlite_identity(tree_store, tree)
    runtime_ddl = one(
        source("crates/store/src/sqlite/schema.rs"),
        r'(?s)const SCHEMA: &str = r"(.*?)\n";',
        "runtime schema",
    )
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
        "product_revision": product_revision,
        "tree_revision": tree_revision,
        "runtime_schema_sha256": hashlib.sha256(runtime_ddl.encode()).hexdigest(),
        "product_schema_sha256": product_ddl,
        "tree_schema_sha256": tree_ddl,
        "context_codec_revision": constant(
            "crates/core/agent/src/manifest.rs", "RUNTIME_CONTEXT_STATE_CODEC_REVISION"
        ),
        "export_version": constant("bin/plakd/src/carry/document.rs", "VERSION"),
        "host_upgrade_receipt_version": constant(
            "bin/plakd/src/carry/upgrade/receipt.rs", "VERSION"
        ),
        "telemetry_schema_version": constant(
            "bin/plakd/src/telemetry_store.rs", "TELEMETRY_SCHEMA_VERSION"
        ),
        "auth_private_store_version": constant(
            "crates/common/auth/src/private_store.rs", "FORMAT_VERSION"
        ),
        "api_key_binding_version": constant(
            "crates/common/auth/src/api_key.rs", "KEY_BINDING_VERSION"
        ),
        "reload_request_version": constant("bin/plakd/src/reload_request.rs", "VERSION"),
        "canonical_item_prefix_digest_revision": constant(
            "crates/core/model/src/history.rs", "CANONICAL_ITEM_PREFIX_DIGEST_REVISION"
        ),
        "effect_authorization_binding_version": constant(
            "crates/core/tools/src/policy.rs", "EFFECT_AUTHORIZATION_BINDING_VERSION"
        ),
        "runner_retention_version": constant(
            "crates/common/runner-protocol/src/retention.rs", "VERSION"
        ),
        "gateway_version": constant(
            "crates/common/gateway-protocol/src/lib.rs", "PROTOCOL_VERSION"
        ),
    }


def write_receipt(path, receipt, *, compressed=False):
    path.parent.mkdir(parents=True, exist_ok=True)
    stream = tempfile.NamedTemporaryFile(dir=path.parent, delete=False, mode="w", encoding="utf-8")
    pending = Path(stream.name)
    try:
        with stream:
            if compressed:
                with gzip.open(pending, "wt", encoding="utf-8") as gz:
                    json.dump(receipt, gz, separators=(",", ":"))
            else:
                json.dump(receipt, stream, indent=2)
                stream.write("\n")
                stream.flush()
            os.fsync(stream.fileno())
        path.hardlink_to(pending)
    finally:
        pending.unlink(missing_ok=True)
    directory = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(directory)
    finally:
        os.close(directory)


def check(root, candidate, directory, main_ref, *, now=None):
    now = int(time.time()) if now is None else now
    candidate = revision(root, candidate)
    expected_formats = formats(root, candidate)
    try:
        main_commit = revision(root, main_ref)
    except subprocess.CalledProcessError as error:
        raise ValueError(f"main ref {main_ref} is missing; fetch origin main first") from error
    main_history = None
    for path in sorted(directory.glob("*.json")):
        try:
            receipt = json.loads(path.read_text(encoding="utf-8"))
            if (
                type(receipt["receipt_version"]) is not int
                or receipt["receipt_version"] != shards.RECEIPT_VERSION
                or receipt["kind"] not in ("full", "merged")
                or receipt["target"] != SHIPPED_TARGET
                or receipt["formats"] != expected_formats
                or any(
                    type(receipt["formats"][k]) is not type(v) for k, v in expected_formats.items()
                )
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
                if main_history is None:
                    main_history = set(
                        git(root, "rev-list", "--first-parent", main_commit).decode().splitlines()
                    )
                if (
                    commit not in main_history
                    or subprocess.run(
                        ["git", "merge-base", "--is-ancestor", commit, candidate],
                        cwd=root,
                        check=False,
                    ).returncode
                ):
                    continue
                stamped = commit_time(root, commit)
                if (
                    stamped > completed
                    or now - stamped > MAX_AGE
                    or formats(root, commit) != expected_formats
                ):
                    continue
            if receipt["tree"] != tree_id(root, commit):
                continue
            coverage = shards.expected_summary(lambda path: git(root, "show", f"{commit}:{path}"))
            if all(receipt[k] == v and type(receipt[k]) is type(v) for k, v in coverage.items()):
                return path, receipt, main_commit
        except (OSError, ValueError, KeyError, TypeError, SyntaxError, subprocess.SubprocessError):
            continue
    raise ValueError(
        f"no passing strict sweep evidence for {candidate} (main {main_ref} at {main_commit})"
    )
