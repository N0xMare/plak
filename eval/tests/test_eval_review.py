"""Behavioral, zero-inference checks for campaign launch and cell boundaries."""

from __future__ import annotations

import base64
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest import mock

from test_eval_validity import campaign
from harbor_adapter.arguments import run_env
from harbor_adapter.campaign import campaign_gate, validate_campaign
from harbor_adapter.cell_prep import prepare_cell, record_cell, RG_PINS
from harbor_adapter.hygiene import HARVEST_SCRIPT
from harbor_adapter.preflight import PreflightError
from harbor_adapter.termination import classify_trial_dir

ROOT = Path(__file__).resolve().parents[2]


class CampaignGateTests(unittest.TestCase):
    def test_real_files_and_mocked_git_docker_boundaries(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            plan = campaign()
            relative = '.plak/installed/arm64/binary'
            binary = root / relative
            binary.parent.mkdir(parents=True)
            binary.write_bytes(b'synthetic executable')
            plan['artifact_sha256'] = {relative: hashlib.sha256(binary.read_bytes()).hexdigest()}
            calibration = root / 'calibration.json'
            reduced = {'source': 'exchange', 'wire_verified': True,
                       'first_request': plan['arms']['a']['observed_config']}
            calibration.write_text(json.dumps(reduced))
            for arm in plan['arms'].values():
                arm['calibration_file'] = str(calibration)
                arm['calibration_sha256'] = hashlib.sha256(calibration.read_bytes()).hexdigest()
            info = {'Id': plan['tasks']['task']['image'], 'Architecture': 'arm64'}
            state = {'commit': plan['source_commit'], 'dirty': b''}

            def output(argv, **kwargs):
                if argv[:2] == ['git', 'rev-parse']:
                    return state['commit']
                if argv[:2] == ['git', 'status']:
                    self.assertEqual(argv[-2:], ['eval', 'Justfile'])
                    return state['dirty']
                self.assertEqual(argv[:3], ['docker', 'image', 'inspect'])
                return json.dumps([info])

            with mock.patch('harbor_adapter.campaign.load_campaign', return_value=plan), \
                 mock.patch('harbor_adapter.campaign.subprocess.check_output', side_effect=output), \
                 mock.patch.dict(os.environ, {'HARBOR_ARCH': 'arm64', 'HARBOR_N_CONCURRENT': '1',
                                              'HARBOR_N_ATTEMPTS': '1'}):
                self.assertEqual(campaign_gate(root), plan)
                for key, invalid in (('commit', 'f' * 40), ('dirty', b' M eval/file')):
                    original = state[key]
                    state[key] = invalid
                    with self.assertRaises(PreflightError):
                        campaign_gate(root)
                    state[key] = original
                for key, invalid in (('HARBOR_ARCH', 'amd64'), ('HARBOR_N_CONCURRENT', '2'),
                                     ('HARBOR_N_ATTEMPTS', '3')):
                    with mock.patch.dict(os.environ, {key: invalid}), self.assertRaises(PreflightError):
                        campaign_gate(root)
                binary.write_bytes(b'changed')
                with self.assertRaises(PreflightError):
                    campaign_gate(root)
                binary.write_bytes(b'synthetic executable')
                outside = root / 'outside'
                outside.write_bytes(binary.read_bytes())
                pins = plan['artifact_sha256']
                plan['artifact_sha256'] = {'outside': next(iter(pins.values()))}
                with self.assertRaises(PreflightError):
                    campaign_gate(root)
                plan['artifact_sha256'] = pins
                reduced['first_request'] = dict(reduced['first_request'], effort='high')
                calibration.write_text(json.dumps(reduced))
                with self.assertRaises(PreflightError):
                    campaign_gate(root)
                # Even a correctly hashed calibration must equal the declared request.
                for arm in plan['arms'].values():
                    arm['calibration_sha256'] = hashlib.sha256(calibration.read_bytes()).hexdigest()
                with self.assertRaises(PreflightError):
                    campaign_gate(root)
                reduced['first_request'] = plan['arms']['a']['observed_config']
                calibration.write_text(json.dumps(reduced))
                for arm in plan['arms'].values():
                    arm['calibration_sha256'] = hashlib.sha256(calibration.read_bytes()).hexdigest()
                for key, invalid in (('Id', 'sha256:' + 'f' * 64), ('Architecture', 'amd64')):
                    original = info[key]
                    info[key] = invalid
                    with self.assertRaises(PreflightError):
                        campaign_gate(root)
                    info[key] = original

    def test_minimum_blocks_and_capture_env(self):
        for blocks in (0, 1, 2, True):
            plan = campaign()
            plan['blocks'] = blocks
            self.assertIn('repeat_blocks', validate_campaign(plan))
        self.assertNotIn('PLAK_TEST_EXCHANGE_CAPTURE_DIR', run_env())
        self.assertNotIn('PLAK_TEST_EXCHANGE_CAPTURE_DIR',
                         run_env(extra_env={'PLAK_TEST_EXCHANGE_CAPTURE_DIR': '/invalid'}))


class CellTests(unittest.IsolatedAsyncioTestCase):
    async def test_record_cell_failure_modes_and_pmux_probe(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            plan = campaign()
            plan['arms']['a']['binaries'] = {'plak': 'b' * 64}
            stdout = '0\n/usr/local/bin/rg\nmanifest\n'
            task = plan['tasks']['task']
            task['cell_manifest_sha256'] = hashlib.sha256(stdout.encode()).hexdigest()
            environment = SimpleNamespace(environment_dir=root / 'task/environment',
                task_env_config=SimpleNamespace(docker_image=task['image']), exec=mock.AsyncMock())
            agent = SimpleNamespace(logs_dir=root)
            state = {'pin': 0, 'stdout': stdout, 'user': 'pmux', 'user_status': 0}

            async def execute(*, command):
                if 'sha256sum -c' in command:
                    self.assertIn('b' * 64 + '  /installed-agent/plak', command)
                    return SimpleNamespace(return_code=state['pin'], stdout='')
                if 'su -s /bin/bash' in command:
                    return SimpleNamespace(return_code=state['user_status'], stdout=state['user'])
                return SimpleNamespace(return_code=0, stdout=state['stdout'])

            environment.exec.side_effect = execute
            with mock.patch('harbor_adapter.cell_prep.load_campaign', return_value=plan), \
                 mock.patch('harbor_adapter.cell_prep.task_budgets', return_value=(1800, 600)), \
                 mock.patch('harbor_adapter.cell_prep.require_native_oauth_lifetime'), \
                 mock.patch.dict(os.environ, {'HARBOR_ARM': 'a'}):
                await record_cell(agent, environment)
                self.assertEqual(json.loads((root / 'cell-preflight.json').read_text())['image'], task['image'])
                for key, invalid in (('pin', 1), ('stdout', '99\nwrong manifest\n')):
                    original = state[key]
                    state[key] = invalid
                    with self.assertRaises(PreflightError):
                        await record_cell(agent, environment)
                    state[key] = original
                for key in ('cell_manifest_sha256', 'image', 'agent_timeout', 'verifier_timeout'):
                    original = task[key]
                    task[key] = 'different'
                    with self.assertRaises(PreflightError):
                        await record_cell(agent, environment)
                    task[key] = original
                pmux = type('FrozenPmux', (), {'logs_dir': root})()
                async def as_agent(env, **kwargs):
                    self.assertIs(env, environment)
                    return await execute(**kwargs)
                pmux.exec_as_agent = mock.AsyncMock(side_effect=as_agent)
                plan['arms']['a']['policy']['user'] = 'pmux'
                await record_cell(pmux, environment)
                for key, invalid in (('user', 'root'), ('user_status', 1)):
                    original = state[key]
                    state[key] = invalid
                    with self.assertRaises(PreflightError):
                        await record_cell(pmux, environment)
                    state[key] = original

    async def test_prepare_uploads_only_verified_binary(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'tools').mkdir()
            (root / 'tools/rg').write_bytes(b'fixture')
            environment = SimpleNamespace(upload_file=mock.AsyncMock())
            agent = SimpleNamespace(exec_as_root=mock.AsyncMock())
            with mock.patch('harbor_adapter.cell_prep.installed_root', return_value=root), \
                 mock.patch('harbor_adapter.cell_prep.load_campaign', return_value={'arch': 'arm64'}), \
                 mock.patch('harbor_adapter.cell_prep.digest_file', return_value=SimpleNamespace(sha256=RG_PINS['arm64'][2])), \
                 mock.patch('harbor_adapter.cell_prep.require_elf_arch'):
                await prepare_cell(agent, environment, 'arm64')
                environment.upload_file.assert_awaited_once_with(str(root / 'tools/rg'), '/usr/local/bin/rg')
                self.assertIn(RG_PINS['arm64'][2], agent.exec_as_root.call_args.kwargs['command'])

    async def test_rung_a_reuses_cached_ab_images_without_pull(self):
        from harbor_adapter.docker_retry import RetryingDocker, DockerEnvironment
        # Exercise the environment used by Rung A, not its source spelling.
        environment = object.__new__(RetryingDocker)
        environment.environment_dir = Path('/task/environment')
        environment.task_env_config = SimpleNamespace(docker_image='moving:tag')
        environment._env_vars = SimpleNamespace(prebuilt_image_name='moving:tag')
        plan = campaign()
        with mock.patch('harbor_adapter.docker_retry.load_campaign', return_value=plan), \
             mock.patch.object(DockerEnvironment, 'start', new_callable=mock.AsyncMock) as start, \
             mock.patch('subprocess.run') as dispatch:
            await environment.start(False)
            self.assertEqual(environment.task_env_config.docker_image, plan['tasks']['task']['image'])
            self.assertEqual(environment._env_vars.prebuilt_image_name, plan['tasks']['task']['image'])
            start.assert_awaited_once_with(False)
            dispatch.assert_not_called()
            with self.assertRaises(PreflightError):
                await environment.start(True)
            start.assert_awaited_once()


class HarvestTests(unittest.TestCase):
    def test_generic_content_is_preserved_but_exact_decoded_secrets_refuse(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            home, logs = root / 'home', root / 'logs'
            home.mkdir()
            (logs / 'agent').mkdir(parents=True)
            text = logs / 'agent/transcript.txt'
            content = 'task example: Bearer synthetic-content-123456789\n'
            text.write_text(content)
            script = HARVEST_SCRIPT.replace('/tmp/plak-cell/home', str(home)).replace('/logs', str(logs))
            # Override the cell-only private roots; never inspect host secrets.
            script = script.replace("source.parent / 'xdg/plak/subscriptions'", "source.parent / 'subscriptions'")
            for name in ('grok', 'pi', 'opencode', 'codex', 'claude'):
                script = script.replace('/tmp/' + name + '-secrets', str(root / (name + '-secrets')))
            script = script.replace('/tmp/codex-home', str(root / 'codex-home'))
            subprocess.run([sys.executable, '-c', script], check=True, capture_output=True)
            self.assertEqual(text.read_text(), content)
            self.assertTrue(json.loads((logs / 'agent/harvest.json').read_text())['generic_secret_pattern_hits'])
            private = root / 'subscriptions/codex'
            private.mkdir(parents=True)
            value = 'synthetic-known-secret-123456789'
            (private / 'credential.json').write_text(json.dumps({'format_version': 1,
                'payload': base64.b64encode(json.dumps({'access_token': value}).encode()).decode()}))
            text.write_text('leaked ' + value)
            result = subprocess.run([sys.executable, '-c', script], capture_output=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertFalse(text.exists())
            self.assertNotIn(value.encode(), result.stderr + result.stdout)

    def test_pmux_markers_are_not_hidden_or_supplied_by_transcripts(self):
        from test_eval_reporting import ReportTests
        fixture = ReportTests()
        with tempfile.TemporaryDirectory() as directory:
            trial = fixture._trial(Path(directory), 'claude', 'task__one', reward=0)
            agent = trial / 'agent'
            (agent / 'codex.txt').unlink()
            result_path = trial / 'result.json'
            result = json.loads(result_path.read_text())
            result['config']['agent']['name'] = 'harbor_adapter.pmux_agent:FrozenPmux'
            result_path.write_text(json.dumps(result))
            (agent / 'claude-transcripts').mkdir()
            transcript = agent / 'claude-transcripts/session.jsonl'
            transcript.write_text('{"type":"assistant","message":"unrelated"}\n')
            for marker, expected in (('NeedsLogin', 'turn_failed:credential'), ('DaemonLost', 'infra:daemon-loss')):
                (agent / 'pmux-stderr.log').write_text(marker + '\n')
                self.assertEqual(classify_trial_dir(trial).termination_class, expected)
            (agent / 'pmux-stderr.log').write_text('')
            transcript.write_text('{"type":"user","message":"NeedsLogin DaemonLost"}\n')
            self.assertEqual(classify_trial_dir(trial).termination_class, 'completed')


class RunnerTests(unittest.TestCase):
    def run_campaign(self, root, responses, *, health=None):
        spec = importlib.util.spec_from_file_location('fixture_campaign_runner', ROOT / 'eval/scripts/campaign.py')
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        module.ROOT = root
        scripts = root / 'eval/scripts'
        scripts.mkdir(parents=True)
        stub = scripts / 'rung-a.sh'
        stub.write_text('#!/bin/sh\nprintf "%s %s %s\\n" "$HARBOR_REPEAT_BLOCK" "$2" "$HARBOR_INCLUDE_TASK" >> "' + str(root / 'calls') + '"\n')
        stub.chmod(0o700)
        calls = []
        real_run = subprocess.run

        def launch(argv, **kwargs):
            self.assertEqual(Path(argv[0]), stub)
            self.assertEqual(kwargs['env']['HARBOR_N_ATTEMPTS'], '1')
            self.assertEqual(kwargs['env']['HARBOR_N_CONCURRENT'], '1')
            calls.append(kwargs['env'])
            return real_run(argv, **kwargs)

        def report(*args, **kwargs):
            index = len(calls) - 1
            arm = (root / 'calls').read_text().splitlines()[-1].split()[1]
            return {'trials': [dict({'arm': arm, 'task': 'task', 'failure_class': 'agent',
                'validity_failures': [], 'eligible_pass_rate': True,
                'evidence': {'result': 'fixture-result'}, 'result_sha256': 'f' * 64},
                **responses.get(index, {}))]}

        with mock.patch.object(module, 'campaign_gate', return_value=campaign()), \
             mock.patch.object(module.subprocess, 'run', side_effect=launch), \
             mock.patch.object(module, 'discover', side_effect=lambda paths: paths), \
             mock.patch.object(module, 'build_report', side_effect=report), \
             mock.patch.object(sys, 'argv', ['campaign.py', 'ab']), \
             mock.patch.dict(os.environ, {'HARBOR_CAMPAIGN_SHA256': 'c' * 64}):
            if health is None:
                result = module.main()
            else:
                with mock.patch.object(module, 'campaign_health', return_value=health):
                    result = module.main()
        return result, calls, next((root / '.plak/harbor/jobs').glob('campaign-*'))

    def test_selection_contains_paired_slots_with_hash_bindings(self):
        with tempfile.TemporaryDirectory() as directory:
            status, calls, output = self.run_campaign(Path(directory), {})
            selection = json.loads((output / 'selection.json').read_text())
            self.assertEqual(status, 0)
            self.assertEqual(len(calls), 6)
            self.assertEqual(selection['record_count'], 6)
            self.assertEqual([r['arm'] for r in selection['records']], ['a', 'b'] * 3)
            self.assertTrue(all(r['result_json_sha256'] == 'f' * 64 for r in selection['records']))
            self.assertFalse((output / 'halt.json').exists())

    def test_two_infra_reruns_then_symmetric_drop(self):
        with tempfile.TemporaryDirectory() as directory:
            infra = {'failure_class': 'infra', 'eligible_pass_rate': False}
            status, calls, output = self.run_campaign(Path(directory), {0: infra, 1: infra, 2: infra}, health=[])
            self.assertEqual(status, 0)
            self.assertEqual(len(calls), 8)
            self.assertEqual([p['HARBOR_REPEAT_BLOCK'] for p in calls[:4]], ['0'] * 4)
            selection = json.loads((output / 'selection.json').read_text())
            self.assertEqual(selection['record_count'], 4)
            self.assertNotIn('0:task', [r['slot'] for r in selection['records']])

    def test_halts_only_at_block_boundary(self):
        with tempfile.TemporaryDirectory() as directory:
            unknown = {'failure_class': 'unknown', 'eligible_pass_rate': False}
            status, calls, output = self.run_campaign(Path(directory), {0: unknown})
            self.assertEqual(status, 2)
            self.assertEqual(len(calls), 2)
            self.assertEqual(json.loads((output / 'halt.json').read_text())['blockers'], ['a:unknown_threshold'])
            self.assertFalse((output / 'selection.json').exists())

    def test_invalid_agent_row_stops_before_next_dispatch(self):
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaisesRegex(RuntimeError, 'post-run validity gate'):
                self.run_campaign(Path(directory), {0: {'validity_failures': ['first_request_unverified']}})
            self.assertEqual(len((Path(directory) / 'calls').read_text().splitlines()), 1)


class VendorReviewTests(unittest.IsolatedAsyncioTestCase):
    async def test_codex_writeback_failure_preserves_primary_error(self):
        from harbor_adapter.vendor_agents import FrozenCodex, _CanonicalNativeUsage
        from harbor_adapter.credentials import CredentialError
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            auth = root / 'synthetic-auth.json'
            auth.write_text('{}')
            agent = object.__new__(FrozenCodex)
            agent.logs_dir = root
            agent._resolve_auth_json_path = lambda: auth

            async def failed_run(*args):
                agent._codex_writeback_error = True
                raise RuntimeError('primary fixture failure')

            with mock.patch.object(_CanonicalNativeUsage, 'run', side_effect=failed_run):
                with self.assertRaisesRegex(RuntimeError, 'primary fixture failure'):
                    await agent.run('fixture', object(), object())
            evidence = json.loads((root / 'adapter-validity.json').read_text())
            self.assertTrue(evidence['codex_writeback_failed'])
            self.assertNotIn('primary fixture failure', json.dumps(evidence))

            async def completed_run(*args):
                agent._codex_writeback_error = True

            with mock.patch.object(_CanonicalNativeUsage, 'run', side_effect=completed_run):
                with self.assertRaisesRegex(CredentialError, 'write-back failed'):
                    await agent.run('fixture', object(), object())

    async def test_codex_exact_scan_precedes_harbor_secret_cleanup(self):
        from harbor_adapter.vendor_agents import FrozenCodex, Codex
        from harbor_adapter.hygiene import harvest_command
        agent = object.__new__(FrozenCodex)
        events = []

        async def root_exec(environment, *, command):
            self.assertEqual(command, harvest_command())
            events.append('exact-value scan')

        async def persist(environment):
            events.append('writeback')

        async def cleanup(*args, **kwargs):
            events.append('secret cleanup')

        agent.exec_as_root = root_exec
        agent._persist_codex_auth = persist
        command = 'rm -rf /tmp/codex-secrets "$CODEX_HOME"'
        with mock.patch.object(Codex, 'exec_as_agent', side_effect=cleanup):
            await agent.exec_as_agent(object(), command)
        self.assertEqual(events, ['exact-value scan', 'writeback', 'secret cleanup'])


class ReportReviewTests(unittest.TestCase):
    def test_generic_pattern_flags_are_unknown_not_scored_agent_failures(self):
        from test_eval_reporting import ReportTests
        from harbor_adapter.report import build_report, discover
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            trial = ReportTests()._trial(root, 'codex', 'task__one', reward=1)
            (trial / 'agent/harvest.json').write_text(json.dumps({
                'secrets_harvested': False, 'generic_secret_pattern_hits': True}))
            row = build_report(discover([root]))['trials'][0]
            self.assertEqual(row['failure_class'], 'unknown')
            self.assertFalse(row['eligible_pass_rate'])
