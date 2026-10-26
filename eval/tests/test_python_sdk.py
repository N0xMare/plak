"""Zero-inference Python composition A/B and worker-cohort contracts."""
import copy
import contextlib
import hashlib
import io
import json
import os
from pathlib import Path
from types import SimpleNamespace
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from harbor_adapter.arguments import ArgumentError, require_composition_override, run_arguments, run_env
from harbor_adapter.artifact_identity import plak_artifact_record, require_plak_provenance
from harbor_adapter.campaign import trial_gate, validate_campaign
from harbor_adapter import campaign_builder as builder, cell_prep
from harbor_adapter.campaign_builder import frozen_binaries, inventory, main as campaign_builder_main, python_recipe
from harbor_adapter.preflight import PreflightError
from harbor_adapter.observed_config import request_config, resolved_composition
from harbor_adapter.tuples import RUNG_A_AB_TASKS
from test_eval_validity import campaign
from test_eval_harbor_adapter import _installed_agent_with_harbor_stub


def python_campaign():
    plan = campaign()
    arm = plan["arms"].pop("a")
    plan["arms"].clear()
    for composition in ("py-ptc", "py-sdk"):
        item = copy.deepcopy(arm)
        item.update(composition=composition, resolved_composition=composition,
                    binaries={name: "b" * 64 for name in
                              ("plak", "plakous-js-host", "plakous-py-host")})
        tool = "exec_py" if composition == "py-ptc" else "run_py"
        item["observed_config"] = request_config({"model": "same-model",
            "reasoning": {"effort": "low"}, "tools": [{"type":"function",
            "name":tool, "parameters":{"type":"object"}}]})
        plan["arms"]["plak-" + composition] = item
    return plan


def generate_fixture(root, plan, mutate_capture=lambda _name, _capture: None):
    """Exercise campaign generation with local captures and no artifact/network I/O."""
    inputs = {"arch": plan["arch"], "tasks": {name: {} for name in RUNG_A_AB_TASKS}, "arms": {}}
    for name, arm in plan["arms"].items():
        capture = {"source": "exchange", "wire_verified": True,
                   "first_request": copy.deepcopy(arm["observed_config"]),
                   "resolved_composition": arm.get("resolved_composition"),
                   "calibration_binding": {"source_commit": plan["source_commit"],
                       "arch": plan["arch"], "binaries": arm["binaries"]}}
        mutate_capture(name, capture)
        path = root / (name + ".json")
        path.write_text(json.dumps(capture))
        entry = {"account": arm["account"], "model": arm["observed_config"]["model"],
                 "effort": arm["observed_config"]["effort"], "calibration_file": str(path),
                 "binaries": {binary: str(root / binary) for binary in arm["binaries"]}}
        if "composition" in arm:
            entry["composition"] = arm["composition"]
        inputs["arms"][name] = entry
    with mock.patch.object(builder.subprocess, "check_output", side_effect=[plan["source_commit"], b""]), \
         mock.patch.object(builder, "task_binding", return_value=plan["tasks"]["task"]), \
         mock.patch.object(builder, "artifact_digest", return_value="b" * 64), \
         mock.patch("harbor_adapter.artifact_identity.require_plak_provenance",
                    return_value={"source": {"commit": plan["source_commit"]}}):
        return builder.generate(root, inputs, phase="scored", slice_name="ab", blocks=3)


class PythonSdkTests(unittest.TestCase):
    def test_default_plak_arm_cannot_claim_a_python_composition(self):
        plan = python_campaign()
        plak = plan["arms"].pop("plak-py-sdk")
        codex = plan["arms"].pop("plak-py-ptc")
        for arm in (plak, codex):
            arm.pop("composition")
        codex.pop("resolved_composition")
        plan["arms"] = {"plak-codex": plak, "codex": codex}
        for composition in ("py-ptc", "py-sdk"):
            with self.subTest(composition=composition), tempfile.TemporaryDirectory() as directory:
                plak["resolved_composition"] = composition
                self.assertIn("undeclared_python_composition", validate_campaign(plan))
                with self.assertRaisesRegex(PreflightError, "^undeclared_python_composition$"):
                    generate_fixture(Path(directory), plan)

    def test_trial_gate_refuses_an_unverified_declared_composition(self):
        plan = python_campaign()
        arm = plan["arms"]["plak-py-sdk"]
        for composition, refused in (("py-sdk", False), ("py-ptc", True)):
            with self.subTest(composition=composition):
                evidence = dict(plan["tasks"]["task"], secrets_harvested=False,
                    resolved_composition=composition,
                    artifact={"binaries": {name: {"sha256": digest} for name, digest in arm["binaries"].items()}},
                    observed_config={"wire_verified": True, "first_request": arm["observed_config"],
                        "requests": [{"response": {"served_model": "same-model"}}]})
                failures = trial_gate(plan, "plak-py-sdk", "task", evidence)
                if refused:
                    self.assertIn("declared_composition_unverified", failures)
                else:
                    self.assertEqual(failures, [])

    def test_composition_override_refuses_another_account(self):
        for account in ("codex", "grok", "claude"):
            with self.subTest(account=account), \
                 mock.patch("harbor_adapter.campaign.load_campaign", return_value=python_campaign()), \
                 mock.patch.dict(os.environ, {"HARBOR_ARM": "plak-py-sdk"}):
                if account == "codex":
                    self.assertEqual(require_composition_override("py-sdk", account), "py-sdk")
                else:
                    with self.assertRaisesRegex(ArgumentError, "composition override differs from the declared A/B campaign arm"):
                        require_composition_override("py-sdk", account)

    def test_generator_refuses_mismatched_calibration_effort_and_composition(self):
        for field, value, reason in (
            ("effort", "low", None),
            ("effort", "high", "calibration tuple differs from the declared Python A/B recipe"),
            ("composition", "py-ptc", "undeclared_python_composition"),
        ):
            with self.subTest(field=field, value=value), tempfile.TemporaryDirectory() as directory:
                def mutate(name, capture):
                    if name == "plak-py-sdk":
                        if field == "effort":
                            capture["first_request"]["effort"] = value
                        else:
                            capture["resolved_composition"] = value
                if reason:
                    with self.assertRaisesRegex(PreflightError, reason):
                        generate_fixture(Path(directory), python_campaign(), mutate)
                else:
                    self.assertEqual(validate_campaign(generate_fixture(Path(directory), python_campaign(), mutate)), [])

    def test_inventory_carries_declared_compositions_and_the_worker_cohort(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name in RUNG_A_AB_TASKS:
                task = root / name.rsplit("/", 1)[-1]
                task.mkdir()
                (task / "task.toml").write_text("version = '1.0'\n")
            result = inventory(root, "amd64", "ab", ["plak-py-ptc", "plak-py-sdk"], None)
            self.assertEqual(set(result["tasks"]), set(RUNG_A_AB_TASKS))
            self.assertEqual({name: arm["composition"] for name, arm in result["arms"].items()},
                             {"plak-py-ptc":"py-ptc", "plak-py-sdk":"py-sdk"})
            self.assertEqual(set(frozen_binaries(root, "amd64", "plak-py-sdk")),
                             {"plak", "plakous-js-host", "plakous-py-host"})
            recipe_path = Path(__file__).resolve().parents[1] / "rung-a-py.yaml"
            output = root / "inventory.json"
            with mock.patch.object(sys, "argv", ["campaign_builder", "inventory", "--recipe", str(recipe_path),
                    "--task-root", str(root), "--output", str(output)]), contextlib.redirect_stdout(io.StringIO()):
                campaign_builder_main()
            declared = json.loads(output.read_text())
            self.assertEqual(declared["recipe"]["blocks"], 3)
            self.assertEqual(declared["recipe"]["sha256"], hashlib.sha256(recipe_path.read_bytes()).hexdigest())
            self.assertEqual(declared["arms"]["plak-py-sdk"]["model"], "gpt-6-luna")
            wrong = python_recipe(recipe_path)
            wrong["arms"]["plak-py-sdk"]["effort"] = "medium"
            bad_path = root / "wrong-recipe.yaml"
            bad_path.write_text(json.dumps(wrong))
            with self.assertRaises(PreflightError):
                python_recipe(bad_path)

    def test_no_campaign_never_admits_an_override(self):
        with mock.patch.dict(os.environ, {}, clear=True):
            with self.assertRaises(ArgumentError):
                run_arguments(account="codex", model="gpt-6-luna", effort="low",
                              prompt="fixture", composition="py-sdk")

    def test_only_the_digest_bound_declared_arm_can_override_composition(self):
        plan = python_campaign()
        self.assertEqual(validate_campaign(plan), [])
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "campaign.json"
            path.write_text(json.dumps(plan))
            env = {"HARBOR_CAMPAIGN": str(path),
                   "HARBOR_CAMPAIGN_SHA256": hashlib.sha256(path.read_bytes()).hexdigest(),
                   "HARBOR_ARM": "plak-py-sdk"}
            with mock.patch.dict(os.environ, env):
                argv = run_arguments(account="codex", model="gpt-6-luna", effort="low",
                                     prompt="fixture", composition="py-sdk")
                self.assertEqual(argv[argv.index("--composition") + 1], "py-sdk")
                for composition in ("py-ptc", "direct", "js-ptc"):
                    with self.assertRaises(ArgumentError):
                        run_arguments(account="codex", model="gpt-6-luna", effort="low",
                                      prompt="fixture", composition=composition)
                with mock.patch.dict(os.environ, {"HARBOR_ARM": "missing"}):
                    with self.assertRaises(ArgumentError):
                        run_arguments(account="codex", model="gpt-6-luna", effort="low",
                                      prompt="fixture", composition="py-sdk")
                path.write_text(path.read_text() + " ")
                with self.assertRaises(ArgumentError):
                    run_arguments(account="codex", model="gpt-6-luna", effort="low",
                                  prompt="fixture", composition="py-sdk")

    def test_default_has_no_override_and_observation_requires_one_selection(self):
        argv = run_arguments(account="codex", model="gpt-6-luna", effort="low", prompt="fixture")
        self.assertNotIn("--composition", argv)
        self.assertEqual(run_env()["PLAK_PY_HOST"], "/installed-agent/plakous-py-host")
        for name in ("py-ptc", "py-sdk"):
            self.assertEqual(resolved_composition(f"plak: composition {name}: details"), name)
        self.assertIsNone(resolved_composition("plak: composition py-ptc: x\nplak: composition py-sdk: y"))

    def test_campaign_requires_both_worker_pins_and_declared_observation(self):
        for change, reason in (
            (lambda p: p["arms"]["plak-py-sdk"]["binaries"].pop("plakous-py-host"), "plak_worker_cohort_required"),
            (lambda p: p["arms"]["plak-py-sdk"].update(composition="direct"), "composition_override_not_declared_ab"),
            (lambda p: p["arms"]["plak-py-sdk"].update(resolved_composition="py-ptc"), "declared_composition_unverified"),
            (lambda p: p["arms"]["plak-py-sdk"]["binaries"].update(plak="f"*64), "python_ab_requires_same_build_account_and_two_compositions"),
            (lambda p: p["arms"]["plak-py-sdk"].pop("composition"), "python_ab_requires_same_build_account_and_two_compositions"),
        ):
            plan = python_campaign()
            change(plan)
            self.assertIn(reason, validate_campaign(plan))

    def test_uploaded_artifact_and_provenance_include_python_worker_bytes(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            names = ("plak", "plakous-js-host", "plakous-py-host")
            for name in names:
                (root / name).write_bytes(name.encode())
            artifact = plak_artifact_record(*(root / name for name in names))
            self.assertEqual(set(artifact["binaries"]), set(names))
            manifest = {"source":{"repo":"plak", "commit":"a"*40, "dirty":False},
                        "features":["maintainer"], "toolchain":"rustc 1.99.0",
                        "binaries":artifact["binaries"]}
            (root / "plak-build.json").write_text(json.dumps(manifest))
            require_plak_provenance(*(root / name for name in names))
            (root / "plakous-py-host").write_bytes(b"changed")
            with self.assertRaises(ValueError):
                require_plak_provenance(*(root / name for name in names))


class PythonWorkerUploadTests(unittest.IsolatedAsyncioTestCase):
    async def test_cell_refuses_an_agent_without_its_declared_composition_before_exec(self):
        for composition in (None, "py-ptc"):
            with self.subTest(composition=composition), tempfile.TemporaryDirectory() as directory, \
                 mock.patch.object(cell_prep, "load_campaign", return_value=python_campaign()), \
                 mock.patch.dict(os.environ, {"HARBOR_ARM": "plak-py-sdk"}):
                agent = SimpleNamespace(_composition=composition, logs_dir=Path(directory))
                environment = SimpleNamespace(exec=mock.AsyncMock())
                with self.assertRaisesRegex(PreflightError, "Plak composition differs from the declared campaign arm"):
                    await cell_prep.record_cell(agent, environment)
                environment.exec.assert_not_awaited()
                self.assertEqual(list(Path(directory).iterdir()), [])

    async def test_install_uploads_and_verifies_the_same_python_worker_bytes(self):
        with _installed_agent_with_harbor_stub("agent") as module, tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            names = ("plak", "plakous-js-host", "plakous-py-host")
            for name in names:
                (root / name).write_bytes(name.encode())
            artifact = plak_artifact_record(*(root / name for name in names))
            manifest = {"source":{"repo":"plak", "commit":"a"*40, "dirty":False},
                        "features":["maintainer"], "toolchain":"rustc 1.99.0",
                        "binaries":artifact["binaries"]}
            (root / "plak-build.json").write_text(json.dumps(manifest))
            agent = module.PlakAgent(root, account="codex", model="gpt-6-luna", effort="low",
                binary_path=str(root / "plak"), js_host_path=str(root / "plakous-js-host"),
                py_host_path=str(root / "plakous-py-host"))
            uploaded = {}
            async def upload(local, remote):
                uploaded[remote] = Path(local).read_bytes()
            environment = mock.Mock(default_user="root")
            environment.upload_file = mock.AsyncMock(side_effect=upload)
            environment.upload_dir = mock.AsyncMock()
            agent.exec_as_root = mock.AsyncMock()
            oauth = root / "fake-oauth"
            oauth.mkdir()
            with mock.patch.object(module, "require_elf_arch"), \
                 mock.patch.object(module, "probe_executable", new_callable=mock.AsyncMock), \
                 mock.patch.object(module, "task_budgets", return_value=(600,600)), \
                 mock.patch.object(module, "trial_oauth", return_value=contextlib.nullcontext(oauth)):
                await agent.install(environment)
            remote = "/installed-agent/plakous-py-host"
            self.assertEqual(uploaded[remote], b"plakous-py-host")
            digest = hashlib.sha256(uploaded[remote]).hexdigest()
            checks = [call.kwargs.get("command", "") for call in agent.exec_as_root.await_args_list]
            self.assertTrue(any(digest in command and remote in command and "sha256sum" in command for command in checks))
