#!/usr/bin/env python3
"""Soft, key-usage budget stop for serial paid OpenRouter Harbor arms.

The baseline is immutable and shared across every budgeted invocation. Polling and
provider accounting lag mean this is not a hard spending limit.
"""
import argparse
from dataclasses import dataclass
from decimal import Decimal, InvalidOperation
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import signal
import stat
import subprocess
import sys
import time
from urllib.request import HTTPRedirectHandler, Request, build_opener

ENDPOINT = "https://openrouter.ai/api/v1/key"
CEILING = Decimal("10")
STOP_AT = Decimal("8")  # Leave at least $2 of the budget ceiling uncommitted.


@dataclass(frozen=True)
class WatchState:
    raw: Decimal
    highwater: Decimal
    regression_reserve: Decimal
    extended: bool = False
    imported_receipt_sha256: str | None = None


def transition(state, raw):
    raw = amount(raw)
    return WatchState(raw, max(state.highwater, raw),
                      state.regression_reserve + max(state.raw - raw, Decimal("0")),
                      True, state.imported_receipt_sha256)


def check_budget(state, baseline, held, remaining):
    held = amount(held)
    remaining = amount(remaining)
    spent_floor = state.highwater - baseline
    if (spent_floor + state.regression_reserve + held >= STOP_AT
            or remaining - state.regression_reserve - held <= CEILING - STOP_AT):
        raise ValueError("OpenRouter soft stop reached; held costs and $2 reserve must remain")
    return spent_floor


def amount(value):
    if isinstance(value, bool) or not isinstance(value, (str, int, float, Decimal)):
        raise ValueError("unreadable usage meter")
    try:
        number = Decimal(str(value))
    except InvalidOperation as error:
        raise ValueError("unreadable usage meter") from error
    if not number.is_finite() or number < 0 or number > 1_000_000:
        raise ValueError("unreadable usage meter")
    return number


def private_bytes(path, maximum=16_384):
    if not path.is_absolute():
        raise ValueError("key and receipt paths must be absolute")
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    try:
        info = os.fstat(fd)
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
            raise ValueError("key or receipt is not a private regular file")
        if info.st_size > maximum:
            raise ValueError("key or receipt is too large")
        return os.read(fd, maximum + 1)
    finally:
        os.close(fd)


def key_bytes(path):
    key = private_bytes(path).strip()
    if not key or b"\n" in key or b"\r" in key or not key.isascii():
        raise ValueError("unreadable OpenRouter key file")
    return key


class NoRedirect(HTTPRedirectHandler):
    def redirect_request(self, *_args, **_kwargs):
        raise ValueError("OpenRouter usage endpoint redirected")


def usage(key):
    request = Request(ENDPOINT, headers={"Authorization": "Bearer " + key.decode("ascii")})
    try:
        with build_opener(NoRedirect).open(request, timeout=10) as response:
            if response.status != 200:
                raise ValueError("OpenRouter usage meter unavailable")
            payload = response.read(65_537)
        if len(payload) > 65_536:
            raise ValueError("OpenRouter usage meter too large")
        data = json.loads(payload, parse_float=Decimal)["data"]
        return amount(data["usage"]), amount(data["limit_remaining"])
    except (OSError, KeyError, TypeError, json.JSONDecodeError) as error:
        raise ValueError("OpenRouter usage meter unavailable") from error


def source_binding():
    root = Path(__file__).resolve().parents[2]
    head = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip()
    files = (Path(__file__).resolve(), root / "eval/scripts/rung-a.sh")
    digest = hashlib.sha256()
    for file in files:
        digest.update(file.relative_to(root).as_posix().encode())
        digest.update(file.read_bytes())
    return {"source_commit": head, "watch_source_sha256": digest.hexdigest()}


def write_new(path, record):
    path.parent.mkdir(parents=True, exist_ok=True)
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as output:
            json.dump(record, output, sort_keys=True)
            output.write("\n")
            output.flush()
            os.fsync(output.fileno())
    except BaseException:
        path.unlink(missing_ok=True)
        raise


def lock(path):
    fd = os.open(str(path) + ".lock", os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    if not stat.S_ISREG(os.fstat(fd).st_mode):
        os.close(fd)
        raise ValueError("watch lock is not a regular file")
    fcntl.flock(fd, fcntl.LOCK_EX)
    return fd


def load_baseline(path, key):
    raw = private_bytes(path)
    record = json.loads(raw)
    if (record.get("version") != 1 or not isinstance(record.get("source"), dict)
            or record.get("key_sha256") != hashlib.sha256(key).hexdigest()
            or amount(record["ceiling_usd"]) != CEILING
            or amount(record["stop_at_usd"]) != STOP_AT):
        raise ValueError("OpenRouter watch baseline binding changed")
    return amount(record["usage_usd"]), hashlib.sha256(raw).hexdigest()


def _unique_object(pairs):
    record = {}
    for key, value in pairs:
        if key in record:
            raise ValueError("OpenRouter usage journal has duplicate fields")
        record[key] = value
    return record


def load_state(path, baseline):
    journal = Path(str(path) + ".observations.jsonl")
    try:
        content = private_bytes(journal, 1_048_576)
    except FileNotFoundError as error:
        raise ValueError("OpenRouter usage journal is missing") from error
    if content and not content.endswith(b"\n"):
        raise ValueError("OpenRouter usage journal has a partial tail")
    state = WatchState(baseline, baseline, Decimal("0"))
    prefix = b""
    for rows_seen, line in enumerate(content.splitlines(keepends=True)):
        if not line:
            raise ValueError("OpenRouter usage journal has an empty row")
        record = json.loads(line, object_pairs_hook=_unique_object)
        if not isinstance(record, dict):
            raise ValueError("OpenRouter usage journal row is invalid")
        if set(record) == {"usage_usd"} and not state.extended:
            value = amount(record["usage_usd"])
            if value < state.highwater:
                raise ValueError("OpenRouter usage journal regressed")
            state = WatchState(value, value, Decimal("0"))
            prefix += line
            continue
        keys = {"policy_version", "baseline_sha256", "prior_journal_sha256", "prior_rows",
                "usage_usd", "raw_usage_usd", "regression_reserve_usd"}
        imported = "import_receipt_sha256" in record
        import_keys = {"import_receipt_sha256", "import_baseline_sha256",
                       "import_prior_journal_sha256", "import_prior_rows",
                       "import_prior_raw_usd", "import_prior_highwater_usd"}
        if (set(record) != keys | (import_keys if imported else set())
                or record.get("policy_version") != 2):
            raise ValueError("OpenRouter usage journal row is invalid")
        if (record["baseline_sha256"] != hashlib.sha256(private_bytes(path)).hexdigest()
                or record["prior_journal_sha256"] != hashlib.sha256(prefix).hexdigest()
                or record["prior_rows"] != rows_seen):
            raise ValueError("OpenRouter usage journal predecessor binding is invalid")
        expected = transition(state, record["raw_usage_usd"])
        if (amount(record["usage_usd"]) != expected.highwater
                or amount(record["regression_reserve_usd"]) != expected.regression_reserve):
            raise ValueError("OpenRouter usage journal transition is invalid")
        receipt_sha = record.get("import_receipt_sha256")
        if imported:
            if (state.imported_receipt_sha256 is not None
                    or not isinstance(receipt_sha, str)
                    or re.fullmatch(r"[0-9a-f]{64}", receipt_sha) is None
                    or record["import_baseline_sha256"] != hashlib.sha256(
                        private_bytes(path)).hexdigest()
                    or record["import_prior_journal_sha256"] != hashlib.sha256(prefix).hexdigest()
                    or record["import_prior_rows"] != rows_seen
                    or amount(record["import_prior_raw_usd"]) != state.raw
                    or amount(record["import_prior_highwater_usd"]) != state.highwater):
                raise ValueError("OpenRouter usage journal import is invalid")
        state = WatchState(expected.raw, expected.highwater, expected.regression_reserve,
                           True, receipt_sha or state.imported_receipt_sha256)
        prefix += line
    return state


def prior_highwater(path, baseline):
    return load_state(path, baseline).highwater


def append_observation(path, state, import_record=None):
    journal = Path(str(path) + ".observations.jsonl")
    import_keys = {"import_receipt_sha256", "import_baseline_sha256",
                   "import_prior_journal_sha256", "import_prior_rows",
                   "import_prior_raw_usd", "import_prior_highwater_usd"}
    if import_record is not None and set(import_record) != import_keys:
        raise ValueError("OpenRouter usage journal import is invalid")
    prior = load_state(path, amount(json.loads(private_bytes(path))["usage_usd"]))
    expected = transition(prior, state.raw)
    if ((state.raw, state.highwater, state.regression_reserve)
            != (expected.raw, expected.highwater, expected.regression_reserve)
            or (import_record is None and state.imported_receipt_sha256
                != prior.imported_receipt_sha256)
            or (import_record is not None and prior.imported_receipt_sha256 is not None)):
        raise ValueError("OpenRouter usage journal transition is invalid")
    prior_bytes = private_bytes(journal, 1_048_576)
    fd = os.open(journal, os.O_WRONLY | os.O_APPEND | os.O_NOFOLLOW)
    try:
        info = os.fstat(fd)
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
            raise ValueError("OpenRouter usage journal is not private")
        record = {"policy_version": 2,
                  "baseline_sha256": hashlib.sha256(private_bytes(path)).hexdigest(),
                  "prior_journal_sha256": hashlib.sha256(prior_bytes).hexdigest(),
                  "prior_rows": len(prior_bytes.splitlines()),
                  "usage_usd": str(state.highwater), "raw_usage_usd": str(state.raw),
                  "regression_reserve_usd": str(state.regression_reserve)}
        if import_record is not None:
            record.update(import_record)
        line = json.dumps(record, sort_keys=True).encode() + b"\n"
        if os.write(fd, line) != len(line):
            raise OSError("OpenRouter usage journal write incomplete")
        os.fsync(fd)
    finally:
        os.close(fd)


def import_rejected_observation(path, baseline, baseline_sha256, report_path, expected_sha256,
                                expected_prior_sha256, expected_prior_rows):
    state = load_state(path, baseline)
    if state.imported_receipt_sha256 is not None:
        raise ValueError("OpenRouter rejected observation was already imported")
    raw = private_bytes(report_path)
    if (re.fullmatch(r"[0-9a-f]{64}", expected_sha256) is None
            or hashlib.sha256(raw).hexdigest() != expected_sha256):
        raise ValueError("OpenRouter rejected report hash mismatch")
    receipt = json.loads(raw)
    diagnostic = receipt.get("meter_regression")
    if (receipt.get("status") != "incomplete"
            or receipt.get("reason") != "OpenRouter usage meter regressed"
            or receipt.get("baseline_sha256") != baseline_sha256
            or not isinstance(diagnostic, dict)
            or set(diagnostic) != {"observed_usd", "prior_highwater_usd", "baseline_usd"}
            or amount(diagnostic["baseline_usd"]) != baseline
            or amount(diagnostic["prior_highwater_usd"]) != state.highwater
            or amount(receipt["usage_usd"]) != state.highwater
            or state.raw != state.highwater):
        raise ValueError("OpenRouter rejected observation does not bind current journal")
    rejected = amount(diagnostic["observed_usd"])
    if rejected >= state.raw:
        raise ValueError("OpenRouter imported observation is not a regression")
    receipt_sha256 = hashlib.sha256(raw).hexdigest()
    next_state = transition(state, rejected)
    prior_journal = private_bytes(Path(str(path) + ".observations.jsonl"), 1_048_576)
    if (hashlib.sha256(prior_journal).hexdigest() != expected_prior_sha256
            or len(prior_journal.splitlines()) != expected_prior_rows):
        raise ValueError("OpenRouter rejected report predecessor journal changed")
    import_record = {"import_receipt_sha256": receipt_sha256,
                     "import_baseline_sha256": baseline_sha256,
                     "import_prior_journal_sha256": hashlib.sha256(prior_journal).hexdigest(),
                     "import_prior_rows": len(prior_journal.splitlines()),
                     "import_prior_raw_usd": str(state.raw),
                     "import_prior_highwater_usd": str(state.highwater)}
    append_observation(path, next_state, import_record)
    return WatchState(next_state.raw, next_state.highwater, next_state.regression_reserve,
                      True, receipt_sha256)


def observe(key, state):
    raw, remaining = usage(key)
    return transition(state, raw), remaining


def stop_owned(process):
    for sig, timeout in ((signal.SIGINT, 20), (signal.SIGTERM, 10), (signal.SIGKILL, 10)):
        if process.poll() is not None:
            return True
        try:
            os.killpg(process.pid, sig)
        except ProcessLookupError:
            pass
        try:
            process.wait(timeout=timeout)
            return True
        except subprocess.TimeoutExpired:
            pass
    return process.poll() is not None


def docker_ids(*filters):
    result = subprocess.run(["docker", "ps", "-aq", *filters], capture_output=True,
                            text=True, check=True, timeout=15)
    ids = result.stdout.split()
    if any(re.fullmatch(r"[0-9a-f]{12,64}", value) is None for value in ids):
        raise ValueError("Docker returned an invalid container identity")
    return set(ids)


def owned_projects(jobs_dir):
    projects = set()
    trials = [path for path in jobs_dir.glob("*/*") if path.is_dir()]
    for trial in trials:
        config = trial / "config.json"
        if not config.is_file():
            raise ValueError("Harbor trial configuration is missing")
        if config.is_symlink() or config.parent.is_symlink():
            raise ValueError("Harbor trial configuration is not a regular owned file")
        data = json.loads(config.read_bytes())
        name = data.get("trial_name")
        if (not isinstance(name, str) or name != config.parent.name
                or Path(data.get("trials_dir", "")).resolve() != config.parent.parent.resolve()
                or re.fullmatch(r"[A-Za-z0-9_-]{1,120}", name) is None):
            raise ValueError("Harbor trial ownership cannot be verified")
        projects.add(re.sub(r"[^a-z0-9_-]", "-", (name + "__env").lower()))
    return projects


def stop_owned_docker(jobs_dir, previous_ids):
    """Best effort only on new containers in this invocation's trial projects."""
    try:
        projects = owned_projects(jobs_dir)
        if not projects:
            return False
        candidates = set()
        for project in projects:
            candidates |= docker_ids("--filter", "label=com.docker.compose.project=" + project)
        candidates -= previous_ids
        if candidates:
            subprocess.run(["docker", "stop", "--time", "3", *sorted(candidates)],
                           capture_output=True, check=False, timeout=30)
        running = set()
        for project in projects:
            running |= docker_ids("--filter", "status=running", "--filter",
                                  "label=com.docker.compose.project=" + project)
        running &= candidates
        if running:
            subprocess.run(["docker", "kill", *sorted(running)], capture_output=True,
                           check=False, timeout=20)
        return all(not (docker_ids("--filter", "status=running", "--filter",
                                   "label=com.docker.compose.project=" + project) & candidates)
                   for project in projects)
    except (OSError, ValueError, subprocess.SubprocessError):
        return False


def interrupted(_signal, _frame):
    raise KeyboardInterrupt


def report(path, **record):
    if path is None:
        return
    write_new(path, record)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("init", "run", "import"))
    parser.add_argument("--key-file", type=Path, required=True)
    parser.add_argument("--baseline", type=Path, required=True)
    parser.add_argument("--report", type=Path)
    parser.add_argument("--held-usd", type=amount, default=Decimal("0"),
                        help="unresolved cost held by other runs on the same budget")
    parser.add_argument("--import-report", type=Path)
    parser.add_argument("--import-report-sha256")
    parser.add_argument("--import-prior-journal-sha256")
    parser.add_argument("--import-prior-rows", type=int)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args(argv)
    if (not args.baseline.is_absolute() or (args.report and not args.report.is_absolute())
            or (args.import_report and not args.import_report.is_absolute())):
        parser.error("baseline and report paths must be absolute")
    key = key_bytes(args.key_file)
    lock_fd = lock(args.baseline)
    try:
        if args.mode == "init":
            if (args.command or args.report or args.held_usd or args.import_report
                    or args.import_report_sha256 or args.import_prior_journal_sha256
                    or args.import_prior_rows is not None):
                parser.error("init takes no command, report, held costs, or import")
            current, remaining = usage(key)
            write_new(args.baseline, {"version": 1, "source": source_binding(),
                                      "key_sha256": hashlib.sha256(key).hexdigest(),
                                      "usage_usd": str(current), "limit_remaining_usd": str(remaining),
                                      "ceiling_usd": str(CEILING), "stop_at_usd": str(STOP_AT)})
            journal = Path(str(args.baseline) + ".observations.jsonl")
            fd = os.open(journal, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
            try:
                os.fsync(fd)
            finally:
                os.close(fd)
            print("OpenRouter baseline recorded; reuse this exact receipt for every budgeted arm")
            return 0
        baseline, baseline_sha256 = load_baseline(args.baseline, key)
        if args.mode == "import":
            if (args.command or args.report or args.held_usd or not args.import_report
                    or not args.import_report_sha256 or not args.import_prior_journal_sha256
                    or args.import_prior_rows is None or args.import_prior_rows < 0):
                parser.error("import requires report, hash and predecessor, without command or held costs")
            import_rejected_observation(args.baseline, baseline, baseline_sha256,
                                        args.import_report, args.import_report_sha256,
                                        args.import_prior_journal_sha256, args.import_prior_rows)
            print("OpenRouter rejected observation imported into the immutable journal")
            return 0
        if (args.import_report or args.import_report_sha256 or args.import_prior_journal_sha256
                or args.import_prior_rows is not None):
            parser.error("run does not take an import report")
        if not args.command:
            parser.error("run requires -- followed by the owned Harbor command")
        if not args.report:
            parser.error("run requires --report")
        if args.report.exists():
            raise ValueError("watch report already exists")
        jobs_flag = args.command.index("--jobs-dir") if "--jobs-dir" in args.command else -1
        if jobs_flag < 0 or jobs_flag + 1 >= len(args.command):
            raise ValueError("Harbor command needs an owned --jobs-dir")
        jobs_dir = Path(args.command[jobs_flag + 1])
        if not jobs_dir.is_absolute() or not jobs_dir.is_dir():
            raise ValueError("Harbor jobs directory is not an existing absolute path")
        source = source_binding()
        state = load_state(args.baseline, baseline)
        state, remaining = observe(key, state)
        append_observation(args.baseline, state)
        spent = check_budget(state, baseline, args.held_usd, remaining)
        previous_ids = docker_ids()
        previous_term = signal.signal(signal.SIGTERM, interrupted)
        process = None
        try:
            process = subprocess.Popen(args.command, start_new_session=True)
            while process.poll() is None:
                time.sleep(5)
                next_state, remaining = observe(key, state)
                append_observation(args.baseline, next_state)
                state = next_state
                spent = check_budget(state, baseline, args.held_usd, remaining)
            next_state, remaining = observe(key, state)
            append_observation(args.baseline, next_state)
            state = next_state
            spent = check_budget(state, baseline, args.held_usd, remaining)
            if source_binding() != source:
                raise ValueError("Harbor watch source changed during run")
        except (Exception, KeyboardInterrupt) as error:
            signal.signal(signal.SIGTERM, signal.SIG_IGN)
            previous_int = signal.signal(signal.SIGINT, signal.SIG_IGN)
            try:
                try:
                    joined = process is None or stop_owned(process)
                except OSError:
                    joined = False
                docker_joined = stop_owned_docker(jobs_dir, previous_ids)
                reason = str(error) if isinstance(error, ValueError) else type(error).__name__
                report(args.report, status="incomplete", reason=reason, joined=joined,
                       docker_cleanup_proven=docker_joined, usage_usd=str(state.highwater),
                       raw_usage_usd=str(state.raw),
                       regression_reserve_usd=str(state.regression_reserve),
                       held_usd=str(args.held_usd),
                       baseline_sha256=baseline_sha256, source=source)
            finally:
                signal.signal(signal.SIGINT, previous_int)
            raise ValueError("watch stopped Harbor; arm incomplete, inspect jobs") from error
        finally:
            signal.signal(signal.SIGTERM, previous_term)
        report(args.report, status="harbor_exited", returncode=process.returncode,
               usage_usd=str(state.highwater), raw_usage_usd=str(state.raw),
               regression_reserve_usd=str(state.regression_reserve),
               budget_spent_usd=str(spent),
               key_remaining_usd=str(remaining), docker_cleanup_proven=False,
               held_usd=str(args.held_usd),
               baseline_sha256=baseline_sha256, source=source)
        return process.returncode
    finally:
        os.close(lock_fd)


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        print(f"OpenRouter watch: {error}", file=sys.stderr)
        sys.exit(2)
