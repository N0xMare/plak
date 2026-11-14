"""Real-process bootstrap and simulated-key dormant ownership-move cases."""

from __future__ import annotations

import importlib.util
import json
from pathlib import Path
import re
import shutil
import signal
import subprocess
import uuid


class Case:
    def __init__(self, support, root, name, images, dormant, child_mode="completed"):
        self.s = support
        self.root = root
        self.name = name
        self.path = root / "owned" / "cases" / name
        self.path.mkdir(parents=True, mode=0o700)
        self.env = support.environment(self.path)
        self.images = images
        self.dormant = dormant
        self.child_mode = child_mode
        self.source = "B" if dormant else "A"
        self.children = []
        self.host = None
        self.cut = None
        self.workspace = self.path / "workspace"
        self.workspace.mkdir()
        # Bound project discovery before it can reach an operator ancestor.
        for directory in [self.path, self.workspace]:
            project = directory / ".plak"
            project.mkdir()
            (project / "config.toml").write_text("")
        git = shutil.which("git")
        support.require(git, "git is required to seed the scratch workspace")
        support.run([git, "init", "-q", str(self.workspace)], env=self.env)
        support.run(
            [
                git,
                "-C",
                str(self.workspace),
                "-c",
                "user.name=Rehearsal",
                "-c",
                "user.email=rehearsal@example.invalid",
                "commit",
                "--allow-empty",
                "-qm",
                "scratch",
            ],
            env=self.env,
        )
        home = Path(self.env["PLAK_HOME"])
        (home / "agents").mkdir()
        (home / "config.toml").write_text(
            '[root]\nagent = "lead"\n[orchestration]\nsubagents = false\n[compaction]\nenabled = false\n'
        )
        template = 'account = "codex"\nmodel = "offline-loopback-v1"\neffort = "medium"\ncomposition = "direct"\ndescription = "offline rehearsal"\n'
        for name in ["lead", "review", "explore", "implement", "orchestrate", "note"]:
            (home / "agents" / f"{name}.toml").write_text(template)
        spec = importlib.util.spec_from_file_location(
            "rehearsal_host_client", images["checkout"] / "tools/examples/host_client.py"
        )
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        self.client_type = module.Client
        self.script("idle")

    def command(self, role, arguments, *, check=True, extra=None, timeout=120):
        env = dict(self.env)
        if role == "source" and self.dormant:
            env["PLAK_TEST_STATE_KEY"] = self.s.SOURCE_KEY
        if extra:
            env.update(extra)
        image = self.images[self.source if role == "source" else "B"]["binary"]
        return self.s.run(
            [image, *arguments], cwd=self.workspace, env=env, check=check, timeout=timeout
        )

    def state(self, role):
        return (
            Path(self.env["PLAK_HOME"])
            / "state"
            / (self.s.SOURCE_KEY if role == "source" else self.s.DESTINATION_KEY)
        )

    def socket(self, role):
        key = self.s.SOURCE_KEY if role == "source" else self.s.DESTINATION_KEY
        path = Path(self.env["PLAK_HOME"]) / "host/run" / f"{key}.sock"
        self.s.require(
            len(bytes(path)) < 108,
            "scratch path is too long for a Unix socket; choose a shorter --root",
        )
        return path

    def client(self, role="source"):
        client = self.client_type(str(self.socket(role)), timeout=90)
        client.hello()
        return client

    def start(self, cut=None):
        cut = cut or self.cut
        env = dict(self.env)
        if self.dormant:
            env["PLAK_TEST_STATE_KEY"] = self.s.SOURCE_KEY
        if cut:
            env["PLAK_TEST_HOST_UPGRADE_CUT"] = cut
        log = self.path / f"host-{len(self.children)}.log"
        with log.open("w") as output:
            self.host = subprocess.Popen(
                [self.images[self.source]["binary"], "host"],
                cwd=self.workspace,
                env=env,
                stdout=output,
                stderr=subprocess.STDOUT,
                start_new_session=True,
            )
        self.children.append(self.host)

        def ready():
            self.s.require(self.host.poll() is None, f"source exited: {log.read_text()}")
            return "plak host: serving on " in log.read_text()

        self.s.wait_for(ready, "source host ready")

        def runner_ready():
            with self.client() as client:
                options = client.request(90, {"op": "session_options"})["options"]
                return any(
                    row["runner"]["runner_id"] == "local" and row["status"]["status"] == "ready"
                    for row in options["runners"]
                )

        self.s.wait_for(runner_ready, "home-local runner ownership reconciliation")

    def stop(self):
        if self.host and self.host.poll() is None:
            self.host.send_signal(signal.SIGTERM)
            self.s.require(self.host.wait(timeout=60) == 0, "source host did not stop cleanly")
        self.host = None

    def script(self, kind):
        child = self.path / "child.json"
        if self.child_mode == "completed":
            self.s.write_json(self.path / "grandchild-script.json", {"version": 1, "steps": []})
        self.s.write_json(
            child,
            {
                "version": 1,
                "steps": [
                    {
                        "stream": [
                            {"text": "real child ready"},
                            {"gate": {"file": str(self.path / "child-release")}},
                        ],
                        "output": {
                            "tool": {
                                "name": "submit_result",
                                "arguments": {
                                    "turn_token": 1,
                                    "output_json": '{"summary":"real child result"}',
                                },
                            }
                        },
                        "finish": "tool_calls",
                    }
                ],
            },
        )
        steps = [{"output": {"text": "rehearsal idle"}}]
        if kind == "child":
            steps.insert(
                0,
                {
                    "output": {
                        "tool": {
                            "name": "spawn_agent",
                            "arguments": {
                                "template": "review",
                                "tier": "read_only",
                                "description": "rehearsal child",
                                "prompt": "return one result",
                                "output_schema_json": '{"type":"object","properties":{"summary":{"type":"string"}},"required":["summary"],"additionalProperties":false}',
                            },
                        }
                    },
                    "finish": "tool_calls",
                },
            )
        if kind == "job":
            release = str(self.path / "job-release").replace("'", "'\\''")
            done = str(self.path / "job-done").replace("'", "'\\''")
            steps.insert(
                0,
                {
                    "output": {
                        "tool": {
                            "name": "process_session_start",
                            "arguments": {
                                "command": f"while ! test -f '{release}'; do sleep 0.05; done; printf retained-rehearsal-output; printf done > '{done}'",
                                "workdir": ".",
                                "login": False,
                                "tty": False,
                                "keep_stdin_open": False,
                                "yield_time_ms": 0,
                                "max_output_bytes": 4096,
                            },
                        }
                    },
                    "finish": "tool_calls",
                },
            )
        self.s.write_json(
            self.path / "script.json",
            {
                "version": 1,
                "tree": True,
                "durable_tree": True,
                "tree_process_tools": True,
                "product_child_script": str(child),
                "steps": steps,
            },
        )

    def rows(self, role, query, database="product", parameters=()):
        return self.s.read_rows(self.state(role) / f"{database}.sqlite3", query, parameters)

    def idle(self, session, minimum=1, role="source"):
        views = self.rows(
            role,
            "SELECT root_turn_count FROM session_views WHERE session_id=?",
            parameters=(session,),
        )
        return (
            not self.rows(
                role,
                "SELECT * FROM session_open_turn_views WHERE session_id=?",
                parameters=(session,),
            )
            and views
            and views[0]["root_turn_count"] >= minimum
        )

    def seed(self):
        document = json.loads(
            (
                self.images["checkout"] / "bin/plak/tests/fixtures/session-export-v12.json"
            ).read_text()
        )
        document["source"]["session_id"] = str(uuid.uuid4())
        document["workspace_root"] = str(self.workspace)
        document["named_state"] = {"rehearsal": {"answer": 42, "nested": [True, "kept"]}}
        document["queue"] = ["paused text must not execute during the move"]
        path = self.path / "seed.json"
        self.s.write_json(path, document)
        result = self.command("source", ["session", "import", str(path)])
        match = re.search(r"resume with: plak --resume (\S+)", result.stdout)
        self.s.require(match, f"import did not return a session: {result.stdout}")
        return match[1]

    def create(self, kind, *, release=True):
        self.script(kind)
        self.start()
        with self.client() as client:
            created = client.request(
                1,
                {
                    "op": "new_session",
                    "runner": {"name": "local", "runner_id": "local"},
                    "profile": None,
                    "model": None,
                    "template": None,
                    "workspace": str(self.workspace),
                    "request_id": str(uuid.uuid4()),
                },
            )
            attachment = client.request(2, {"op": "attach", "node": created["node"]})["attachment"]
            client.request(
                3,
                {
                    "op": "intent",
                    "attachment": attachment,
                    "client_id": str(uuid.uuid4()),
                    "blobs": [],
                    "intent": {
                        "kind": "prompt",
                        "body": {"text": f"rehearse {kind}", "images": []},
                    },
                },
            )
            self.s.wait_for(lambda: self.idle(created["session"]), f"{kind}: root idle")
            client.request(4, {"op": "detach", "attachment": attachment})
        if kind == "child":
            self.s.wait_for(
                lambda: self.rows(
                    "source",
                    "SELECT agent_id FROM agent_nodes WHERE parent_agent_id IS NOT NULL AND session_locator IS NOT NULL AND fold_order IS NOT NULL",
                    "subagents",
                ),
                "real bound child",
            )
        if kind in {"child", "job"} and release:
            (self.path / f"{kind}-release").touch()
            if kind == "child":
                self.s.wait_for(
                    lambda: self.idle(created["session"], minimum=2),
                    "child result woke its idle root",
                )
                self.s.wait_for(
                    lambda: (
                        not self.rows(
                            "source", "SELECT * FROM agent_nodes WHERE active_turn<>0", "subagents"
                        )
                    ),
                    "child settled",
                )
            else:
                self.s.wait_for(
                    lambda: (self.path / "job-done").exists(),
                    "real process reached terminal output",
                )
        if release and kind != "job":
            self.stop()
        return created["session"]

    def exports(self, role, sessions):
        documents = {}
        for session in sessions:
            path = self.path / f"export-{role}-{session}.json"
            self.command(role, ["session", "export", session, "--out", str(path)])
            documents[session] = json.loads(path.read_text())
        return documents

    def snapshot(self, role):
        result = {
            "products": self.rows(
                role, "SELECT id,node_tree_id,node_agent_id,frozen_tools FROM sessions ORDER BY id"
            ),
            "branches": self.rows(
                role,
                "SELECT session_id,branch_id,line_id FROM branches ORDER BY session_id,branch_id",
                "runtime",
            ),
            "nodes": self.rows(
                role,
                "SELECT tree_id,agent_id,parent_agent_id,template,tuple_id,account,effort,tier,workspace_id,output_schema_json,schema_digest,fold_order,line_id FROM agent_nodes ORDER BY tree_id,agent_id",
                "subagents",
            ),
            "terminals": self.rows(
                role,
                "SELECT line_id,hex(generation) AS generation,job,hex(evidence) AS evidence FROM line_job_terminals ORDER BY line_id,job",
                "runtime",
            ),
        }
        for row in result["products"]:
            if row["frozen_tools"]:
                row["frozen_tools"] = json.loads(row["frozen_tools"])
        return result

    def child_history(self, role, root, child):
        result = self.command(
            role, ["session", "log", f"{root}.{child}", "--json", "--tail", "10000"], check=False
        )
        return {
            "exit": result.returncode,
            "rows": [json.loads(line) for line in result.stdout.splitlines()]
            if result.returncode == 0
            else [],
            "remedy": result.stderr,
        }

    def history(self, role, session):
        result = self.command(role, ["session", "log", session, "--json", "--tail", "10000"])
        return [json.loads(line) for line in result.stdout.splitlines()]

    def interrupt_child(self, session):
        child = self.rows(
            "source",
            "SELECT agent_id FROM agent_nodes WHERE parent_agent_id IS NOT NULL",
            "subagents",
        )[0]["agent_id"]
        with self.client() as client:
            node = next(
                row["node"]
                for row in client.request(1, {"op": "list_sessions", "include_archived": False})[
                    "rows"
                ]
                if row["session"] == session
            )
            attachment = client.request(2, {"op": "attach", "node": node})["attachment"]
            client.request(
                3,
                {
                    "op": "intent",
                    "attachment": attachment,
                    "blobs": [],
                    "intent": {"kind": "interrupt_topology_node", "body": {"agent_id": child}},
                },
            )
            self.s.wait_for(
                lambda: (
                    not self.rows(
                        "source", "SELECT * FROM agent_nodes WHERE active_turn<>0", "subagents"
                    )
                ),
                "interrupted bound child quiescent",
            )
            client.request(4, {"op": "detach", "attachment": attachment})
        self.stop()

    def receipts(self):
        paths = Path(self.env["PLAK_HOME"]).glob("host/upgrades/*/receipt.json")
        return sorted(
            (json.loads(path.read_text()) for path in paths), key=lambda row: row["sequence"]
        )

    def upgrade(self, cut=None):
        extra = {"PLAK_TEST_HOST_UPGRADE_CUT": cut} if cut else None
        probe = json.loads(self.command("destination", ["version", "--host-upgrade-probe"]).stdout)
        self.s.require(
            probe["state_key"] == self.s.DESTINATION_KEY,
            "destination is not the requested natural key",
        )
        return self.command(
            "destination",
            ["host", "upgrade", "--expect-binary", probe["binary_id"]],
            check=False,
            extra=extra,
        )

    def persist_logs(self):
        for path in self.path.glob("*.log"):
            (self.root / f"{self.name}-{path.name}").write_bytes(path.read_bytes())


def carry(case, *, cut=None, jobs=True):
    case.cut = cut
    seeded = case.seed()
    child_root = case.create("child", release=case.child_mode == "completed")
    if case.child_mode != "completed":
        case.interrupt_child(child_root)
    sessions = [seeded, child_root]
    ordinary = list(sessions)
    job_session = case.create("job") if jobs else None
    if job_session:
        sessions.append(job_session)
    case.script("idle")
    before = case.exports("source", ordinary)
    histories = {session: case.history("source", session) for session in sessions}
    original = case.snapshot("source")
    child = next(row["agent_id"] for row in original["nodes"] if row["parent_agent_id"])
    child_before = case.child_history("source", child_root, child)
    case.s.require(
        child_before["exit"] == 0 and child_before["rows"],
        "source child has no readable canonical history",
    )
    if not case.host:
        case.start(cut)
    result = case.upgrade(cut)
    if cut:
        case.s.require(result.returncode != 0, f"crash cut was not reached: {cut}")
        case.s.wait_for(lambda: case.host.poll() is not None, "cut source process exited")
        case.s.require(
            case.host.returncode == 86,
            f"cut did not produce the explicit hook exit: {case.host.returncode}",
        )
        prepared = case.receipts()[-1]["phase"] == "prepared"
        case.cut = None
        if prepared:
            case.start()
            case.s.require(
                case.receipts()[-1]["phase"] == "aborted", "source did not withdraw prepared move"
            )
            case.s.require(
                case.snapshot("source") == original,
                "prepared cut changed source identities or bindings",
            )
        result = case.upgrade()
    case.s.require(result.returncode == 0, f"upgrade refused:\n{result.stderr}\n{result.stdout}")
    authority = case.receipts()[-1]
    case.s.require(
        authority["phase"] == "acknowledged", "destination did not acknowledge ownership"
    )
    mappings = {row["source"]: row["successor"] for row in authority["mappings"]}
    case.s.require(set(mappings) == set(sessions), "move silently omitted a source session")
    after = case.exports("destination", [mappings[session] for session in ordinary])
    for source, target in mappings.items():
        if case.dormant:
            case.s.require(
                case.history("destination", target) == histories[source],
                f"canonical history changed: {source}",
            )
        if source in before:
            for field in ["history", "named_state", "queue"]:
                case.s.require(
                    before[source][field] == after[target][field], f"{field} changed for {source}"
                )
        refused = case.command(
            "source", ["run", "--session", source, "--", "must not execute"], check=False
        )
        case.s.require(
            refused.returncode != 0
            and any(code in refused.stderr for code in ["source_fenced", "session_archived"]),
            f"old source was not fenced: {refused.stderr}",
        )
    report = {
        "mappings": mappings,
        "queue": "text preserved and paused",
        "old_source": "refused",
        "history": "root histories and named state identical",
    }
    if case.dormant:
        native = {
            row["operation"].lower(): row["digest"].lower()
            for row in case.rows(
                "destination",
                "SELECT hex(operation) AS operation,hex(digest) AS digest FROM runtime_moves",
                "runtime",
            )
        }
        job_ledgers = []
        for group in authority["groups"]:
            case.s.require(
                native.get(bytes(group["operation"]).hex()) == group["target_digest"],
                "published native digest differs from the reviewed reconstructed group (history/bindings/jobs)",
            )
            path = (
                Path(case.env["PLAK_HOME"])
                / "host/upgrades"
                / authority["id"]
                / f"{group['id']}.json"
            )
            capsule = json.loads(path.read_text())
            job_ledgers.extend(
                head["jobs"]
                for session in capsule["sessions"]
                for head in session["heads"]
                if head["jobs"]["rows"]
            )
        if jobs:
            case.s.require(
                job_ledgers
                and any(
                    row["terminal"]["tail"].find("retained-rehearsal-output") >= 0
                    for ledger in job_ledgers
                    for row in ledger["rows"]
                ),
                "no retained terminal output in the source group",
            )
            case.s.require(
                all(
                    ledger["next_number"] > max(row["number"] for row in ledger["rows"])
                    for ledger in job_ledgers
                ),
                "retained job number floor regressed",
            )
        case.s.write_json(
            case.root / f"{case.name}-invariants.json",
            {
                "before": original,
                "after": case.snapshot("destination"),
                "history_before": histories,
                "jobs": job_ledgers,
                "groups": authority["groups"],
            },
        )
        case.s.require(
            all(source == target for source, target in mappings.items()),
            "dormant move allocated successor IDs",
        )
        case.s.require(
            case.snapshot("destination") == original,
            "dormant move changed tree, child, native line, frozen tools or retained job evidence",
        )
        case.s.require(
            case.child_history("destination", child_root, child) == child_before,
            "dormant child canonical history changed",
        )
        case.s.require(
            authority["groups"]
            and all(
                group["phase"] == "ready" and group["source_retired"]
                for group in authority["groups"]
            ),
            "group did not retire and publish Ready",
        )
        report.update(
            child="bound child identity/history preserved",
            jobs="terminal IDs/evidence preserved",
            keys="simulated old key; both images use candidate codecs",
        )
    else:
        case.s.require(
            all(source != target for source, target in mappings.items()),
            "bootstrap unexpectedly claimed dormant preserved IDs",
        )
        document = after[mappings[child_root]]
        report.update(
            child={
                "source_subagents": document.get("source_subagents"),
                "source_resolved_count": document.get("source_resolved_count"),
                "destination_log": case.child_history("destination", mappings[child_root], child),
            },
            jobs="not included in successful bootstrap fixture",
            keys="real main codec to real candidate codec",
        )
    return report


def bootstrap_observation(case, kind):
    session = case.create(
        "child" if kind == "active-child" else "job", release=kind == "terminal-job"
    )
    if case.host is None:
        case.start()
    result = case.upgrade()
    receipts = case.receipts()
    if result.returncode:
        case.s.require(
            not any(row["phase"] in {"committed", "acknowledged"} for row in receipts),
            "refusal surrendered a source",
        )
        return {
            "session": session,
            "outcome": "refused before surrender",
            "operator_remedy": result.stderr,
            "exit": result.returncode,
        }
    authority = receipts[-1]
    return {
        "session": session,
        "outcome": "carried",
        "mappings": authority["mappings"],
        "operator_output": result.stdout + result.stderr,
    }


def reproduce_terminal_job(case):
    session = case.create("job")
    result = case.upgrade()
    case.s.write_json(
        case.root / "repro-terminal-job.json",
        {
            "session": session,
            "process_finished": (case.path / "job-done").read_text(),
            "heads": case.rows(
                "source",
                "SELECT b.session_id,b.branch_id,c.kind FROM branches b JOIN checkpoints c ON c.checkpoint_id=b.checkpoint_id",
                "runtime",
            ),
            "exit": result.returncode,
            "refusal": result.stderr,
            "receipts": case.receipts(),
        },
    )
    case.s.require(result.returncode == 0, f"terminal-job move refused: {result.stderr}")
    return {"session": session, "outcome": "terminal job moved"}


def reproduce_finished_child(case):
    session = case.create("child")
    case.start()
    result = case.upgrade()
    case.s.write_json(
        case.root / "repro-finished-child.json",
        {
            "session": session,
            "root_turns": case.rows("source", "SELECT root_turn_count FROM session_views"),
            "nodes": case.rows(
                "source",
                "SELECT agent_id,parent_agent_id,status,active_turn,session_locator FROM agent_nodes",
                "subagents",
            ),
            "heads": case.rows(
                "source",
                "SELECT b.session_id,b.branch_id,c.kind FROM branches b JOIN checkpoints c ON c.checkpoint_id=b.checkpoint_id",
                "runtime",
            ),
            "exit": result.returncode,
            "refusal": result.stderr,
            "receipts": case.receipts(),
        },
    )
    case.s.require(result.returncode == 0, f"finished-child move refused: {result.stderr}")
    return {"session": session, "outcome": "finished child moved"}


def rehearse(args, root, support):
    images = json.loads((root / "builds.json").read_text())
    images["checkout"] = root / "owned/checkout-B"
    report = {
        "images": {key: value for key, value in images.items() if key != "checkout"},
        "cases": [],
        "increment4": "not qualified: live/parked-tree upgrade cases are a separate future extension",
    }
    definitions = []
    if args.arm in {"all", "bootstrap"}:
        definitions += [
            ("b-carry", False, lambda case: carry(case, jobs=False)),
            ("b-child", False, lambda case: bootstrap_observation(case, "active-child")),
            ("b-job", False, lambda case: bootstrap_observation(case, "terminal-job")),
            ("b-live-job", False, lambda case: bootstrap_observation(case, "active-job")),
        ]
    if args.arm in {"all", "dormant"}:
        definitions += [
            ("repro-job", True, reproduce_terminal_job),
            ("repro-child", True, reproduce_finished_child),
        ]
        definitions.append(("d-carry", True, carry))
        definitions += [
            (f"d-cut-{index}", True, lambda case, cut=cut: carry(case, cut=cut))
            for index, cut in enumerate(support.CUTS)
        ]
    failed = False
    for name, dormant, body in definitions:
        if args.case and name not in args.case:
            continue
        print(
            f"CASE {name}: {'DORMANT simulated key' if dormant else 'BOOTSTRAP real revisions'}",
            flush=True,
        )
        case = None
        entry = {"case": name, "arm": "dormant" if dormant else "bootstrap"}
        try:
            case = Case(support, root, name, images, dormant)
            entry.update(body(case), status="passed")
        except Exception as error:
            entry.update(status="failed", error=f"{type(error).__name__}: {error}")
            failed = True
        finally:
            if case:
                case.persist_logs()
            path = root / "owned/cases" / name
            if path.exists():
                support.cleanup(path, case.children if case else [])
            entry["isolation"] = (
                "no live scratch processes; owned leaders reaped and case state removed"
            )
        report["cases"].append(entry)
        support.write_json(root / "report.json", report)
        print(json.dumps(entry), flush=True)
    support.require(report["cases"], "selection ran zero rehearsal cases")
    support.require(not failed, "one or more rehearsal cases failed; see report.json")
    return report
