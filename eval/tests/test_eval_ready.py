"""Offline readiness fixtures: real local TLS, no provider or credential access."""
from __future__ import annotations

import copy
import hashlib
import http.client
import io
import json
import os
from pathlib import Path
import ssl
import shutil
import subprocess
import sys
import tarfile
import tempfile
import threading
from types import SimpleNamespace
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from harbor_adapter import campaign_builder as builder
from harbor_adapter.campaign import validate_campaign
from harbor_adapter.leases import account_leases
from harbor_adapter.lifecycle import parse_sample
from harbor_adapter.observed_config import request_config
from harbor_adapter.preflight import PreflightError
from harbor_adapter.recording_proxy import make_certificate, ProxyHandler, ProxyServer, Recorder, require_hosts
from harbor_adapter.tuples import RUNG_A_AB_TASKS, RUNG_A_PLUMBING_TASKS
from harbor_adapter.wire import bounded_command
from test_eval_validity import campaign


class RecorderTests(unittest.TestCase):
    def test_structural_only_first_request_order_and_mode(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / 'reduced.json'
            recorder = Recorder(output)
            first = recorder.begin(b'{"model":"fixture","instructions":"private-system","input":"private-task","reasoning":{"effort":"low"}}')
            second = recorder.begin(b'{"model":"later"}')
            recorder.finish(second, b'data: {"model":"served-later"}\n\n')
            recorder.finish(first, b'data: {"response":{"model":"served-first","output":"private-answer"}}\n\n')
            self.assertIsNone(recorder.begin(b'{"telemetry":"private-telemetry"}'))
            data = json.loads(output.read_text())
            self.assertEqual(data['first_request']['model'], 'fixture')
            self.assertEqual(data['requests'][0]['response']['served_model'], 'served-first')
            self.assertEqual(output.stat().st_mode & 0o777, 0o600)
            for content in ('private-system', 'private-task', 'private-answer', 'private-telemetry'):
                self.assertNotIn(content, output.read_text())
            self.assertEqual(list(Path(directory).iterdir()), [output])

    def test_allowlist_refuses_urls_wildcards_and_shell_text(self):
        for hosts in ([], ['https://api.example.test'], ['*.example.test'], ['x;cat'], ['x:443']):
            with self.assertRaises(ValueError):
                require_hosts(hosts)

    def test_real_connect_tls_stream_and_upstream_verification(self):
        real_connection = http.client.HTTPSConnection
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / 'private'
            make_certificate(root, ['api.example.test'])
            context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
            context.load_cert_chain(root / 'ca.pem', root / 'key.pem')
            server = ProxyServer(('127.0.0.1', 0), ProxyHandler)
            server.hosts, server.tls = ['api.example.test'], context
            server.recorder = Recorder(root / 'observed-config.json')
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            upstream = mock.Mock()
            response = mock.Mock(status=200)
            response.getheaders.return_value = [('Content-Type', 'text/event-stream')]
            response.read1.side_effect = [b'data: {"model":"served","content":"private-answer"}\n\n', b'']
            upstream.getresponse.return_value = response
            client = real_connection('127.0.0.1', server.server_port,
                                     context=ssl.create_default_context(cafile=str(root / 'ca.pem')))
            client.set_tunnel('api.example.test', 443)
            try:
                with mock.patch('harbor_adapter.recording_proxy.http.client.HTTPSConnection', return_value=upstream) as connect:
                    body = b'{"model":"fixture","messages":[{"role":"user","content":"private-task"}]}'
                    client.request('POST', '/v1/responses', body, {'Authorization': 'Bearer synthetic-only'})
                    result = client.getresponse()
                    self.assertEqual(result.status, 200)
                    self.assertIn(b'private-answer', result.read())
                    tls = connect.call_args.kwargs['context']
                    self.assertEqual(tls.verify_mode, ssl.CERT_REQUIRED)
                    self.assertTrue(tls.check_hostname)
                    self.assertEqual(connect.call_args.args, ('api.example.test',))
                    self.assertNotIn('Proxy-Authorization', upstream.request.call_args.args[3])
                    output = (root / 'observed-config.json').read_text()
                    for secret in ('synthetic-only', 'private-answer', 'private-task', 'Authorization'):
                        self.assertNotIn(secret, output)
                    self.assertEqual(json.loads(output)['requests'][0]['response']['served_model'], 'served')
            finally:
                client.close()
                server.shutdown()
                server.server_close()
                thread.join()

    def test_connect_denies_unlisted_host_before_upstream(self):
        server = ProxyServer(('127.0.0.1', 0), ProxyHandler)
        server.hosts = ['api.example.test']
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        client = http.client.HTTPConnection('127.0.0.1', server.server_port)
        try:
            client.request('CONNECT', 'not-allowed.test:443')
            self.assertEqual(client.getresponse().status, 403)
        finally:
            client.close()
            server.shutdown()
            server.server_close()
            thread.join()


class BuilderTests(unittest.TestCase):
    def test_private_output_never_overwrites_reviewed_plan(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'campaign.json'
            digest = builder.private_json(path, {'schemaVersion': 1})
            self.assertEqual(digest, hashlib.sha256(path.read_bytes()).hexdigest())
            self.assertEqual(path.stat().st_mode & 0o777, 0o600)
            with self.assertRaises(FileExistsError):
                builder.private_json(path, {'changed': True})

    def test_binary_and_archive_member_pins_and_path_escape(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            prefix = root / '.plak/installed/amd64'
            prefix.mkdir(parents=True)
            (prefix / 'binary').write_bytes(b'fixture-executable')
            archive = prefix / 'cli.tar.gz'
            with tarfile.open(archive, 'w:gz') as tar:
                member = tarfile.TarInfo('prefix/bin/cli')
                member.size = len(b'fixture-cli')
                tar.addfile(member, io.BytesIO(b'fixture-cli'))
            self.assertEqual(builder.artifact_digest(root, 'amd64', '.plak/installed/amd64/binary'),
                             hashlib.sha256(b'fixture-executable').hexdigest())
            self.assertEqual(builder.artifact_digest(root, 'amd64', {'archive': str(archive), 'member': 'prefix/bin/cli'}),
                             hashlib.sha256(b'fixture-cli').hexdigest())
            with self.assertRaises(PreflightError):
                builder.artifact_digest(root, 'amd64', '/etc/passwd')

    def test_generator_binds_real_local_files_and_calibration(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            prefix = root / '.plak/installed/amd64'
            prefix.mkdir(parents=True)
            binary = prefix / 'cli'
            binary.write_bytes(b'fixture')
            capture = root / 'capture.json'
            observed = request_config({'model': 'fixture', 'reasoning': {'effort': 'low'}})
            capture.write_text(json.dumps({'source': 'exchange', 'wire_verified': True, 'first_request': observed,
                'calibration_binding': {'source_commit': 'a' * 40, 'arch': 'amd64',
                                        'binaries': {'codex': hashlib.sha256(b'fixture').hexdigest()}}}))
            arm = {'account': 'codex', 'binaries': {'codex': str(binary)}, 'calibration_file': str(capture)}
            inventory = {'arch': 'amd64', 'artifact_paths': [str(binary)],
                         'tasks': {t: {} for t in RUNG_A_AB_TASKS}, 'arms': {'codex': arm, 'pi-codex-oauth': arm}}
            with mock.patch.object(builder.subprocess, 'check_output', side_effect=['a' * 40, b'']), \
                 mock.patch.object(builder, 'task_binding', return_value=campaign()['tasks']['task']):
                plan = builder.generate(root, inventory, phase='scored', slice_name='ab', blocks=3)
            self.assertEqual(validate_campaign(plan), [])
            self.assertEqual(plan['arms']['codex']['observed_config'], observed)
            self.assertEqual(plan['arms']['codex']['calibration_sha256'], hashlib.sha256(capture.read_bytes()).hexdigest())
            self.assertEqual(plan['artifact_sha256'][str(binary)], hashlib.sha256(b'fixture').hexdigest())

    def test_task_probe_binds_checksum_budgets_arch_and_resource_limits(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            task = root / 'task'
            (task / 'environment').mkdir(parents=True)
            (task / 'task.toml').write_text('[agent]\ntimeout_sec=1200\n[verifier]\ntimeout_sec=90\n[environment]\ncpus=2\nmemory_mb=4096\n')
            image = 'sha256:' + 'a' * 64
            with mock.patch.object(builder, 'docker', side_effect=[json.dumps([{'Id': image, 'Architecture': 'amd64'}]), '0\nmanifest']) as docker:
                binding = builder.task_binding(root, 'amd64', {'path': str(task), 'image': image})
            self.assertEqual(binding['agent_timeout'], 1200)
            self.assertEqual(binding['verifier_timeout'], 90)
            self.assertEqual(binding['cell_manifest_sha256'], hashlib.sha256(b'0\nmanifest\n').hexdigest())
            self.assertIn('--cpus', docker.call_args.args)
            self.assertIn('4096m', docker.call_args.args)
            self.assertIn('--network=none', docker.call_args.args)
            with mock.patch.object(builder, 'docker', return_value=json.dumps([{'Id': image, 'Architecture': 'arm64'}])):
                with self.assertRaises(PreflightError):
                    builder.task_binding(root, 'amd64', {'path': str(task), 'image': image})

    def test_phase0_has_no_circular_wire_pin_requirement_and_is_not_scored(self):
        plan = campaign()
        plan.update(phase='calibration', blocks=1)
        plan['arms'] = {'codex': plan['arms']['a']}
        del plan['arms']['codex']['observed_config']
        del plan['arms']['codex']['calibration_sha256']
        self.assertEqual(validate_campaign(plan), [])
        plan['blocks'] = 3
        self.assertIn('calibration_is_one_trial_per_arm', validate_campaign(plan))

    def test_null_requires_identical_binary_tuple_and_eight_tasks_three_blocks(self):
        plan = campaign()
        plan.update(phase='null', tasks={t: copy.deepcopy(plan['tasks']['task']) for t in RUNG_A_AB_TASKS})
        plan['arms'] = {'plak-codex': plan['arms']['a'], 'plak-codex-aa': plan['arms']['b']}
        self.assertEqual(validate_campaign(plan), [])
        plan['arms']['plak-codex-aa']['binaries']['binary'] = 'e' * 64
        self.assertIn('null_requires_identical_arms', validate_campaign(plan))


class RuntimeTests(unittest.TestCase):
    def test_account_lease_is_exclusive_across_owners_and_released(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'leases'
            with account_leases(['codex'], directory=path):
                with self.assertRaises(PreflightError):
                    with account_leases(['codex'], directory=path):
                        self.fail('second owner admitted')
            with account_leases(['codex'], directory=path):
                pass
            path.chmod(0o755)
            with self.assertRaises(PreflightError):
                with account_leases(['codex'], directory=path):
                    pass

    def test_timeout_stops_inside_shell_not_only_docker_client(self):
        if not shutil.which('timeout'):
            self.skipTest('GNU timeout is a Linux cell prerequisite')
        result = subprocess.run(bounded_command('sleep 10', .05), shell=True, capture_output=True, timeout=3)
        self.assertEqual(result.returncode, 124)
        with self.assertRaises(PreflightError):
            bounded_command('true', 0)

    def test_cgroup_metrics_are_numeric_only(self):
        parsed = parse_sample('RESOURCES\nmemory.peak\n8192\nmemory.events\noom 2\noom_kill 1\ncpu.stat\nusage_usec 3000\nprivate invalid\n', 'after')
        self.assertEqual(parsed['resources'], {'memory_peak_bytes': 8192, 'oom': 2, 'oom_kill': 1, 'cpu_usage_usec': 3000})

    def test_developer_input_is_digested_without_retaining_content(self):
        a = request_config({'model': 'fixture', 'input': [{'role': 'developer', 'content': 'first'}]})
        b = request_config({'model': 'fixture', 'input': [{'role': 'developer', 'content': 'second'}]})
        self.assertNotEqual(a['instructions_sha256'], b['instructions_sha256'])
        self.assertNotIn('first', json.dumps(a))

    def test_recipes_route_to_explicit_phases_without_rust_or_paid_tests(self):
        root = Path(__file__).resolve().parents[2]
        just = (root / 'Justfile').read_text()
        for name, phase in [('harbor-phase0', 'calibration'), ('harbor-null', 'null'), ('harbor-codex', 'codex')]:
            self.assertIn(name + ':\n    uv run --project eval python eval/scripts/campaign.py ' + phase, just)
        for script in ('campaign.py', 'campaign_builder.py', 'recording_proxy.py'):
            path = root / ('eval/scripts' if script == 'campaign.py' else 'eval/harbor_adapter') / script
            compile(path.read_text(), str(path), 'exec')

class EvidenceTests(unittest.TestCase):
    def test_argv_parsers_exclude_instruction_and_shell_redirection(self):
        from harbor_adapter.argv import native_argv
        instruction = "Task with 'quotes' and\nprivate content"
        import shlex
        prompt = shlex.quote(instruction)
        for harness, command, expected in (
            ('codex', f'. nvm.sh; codex exec --json -- {prompt} 2>&1 | tee /logs/output', ['codex', 'exec', '--json', '--']),
            ('grok-build', f'grok --no-auto-update --single {prompt} --always-approve &', ['grok', '--no-auto-update', '--single', '--always-approve']),
            ('pi', f'pi --print --mode json {prompt} 2>&1 | tee output', ['pi', '--print', '--mode', 'json']),
            ('opencode', f'opencode --model=openai/fixture run --format=json -- {prompt} 2>&1 | tee output', ['opencode', '--model=openai/fixture', 'run', '--format=json', '--']),
        ):
            with self.subTest(harness=harness):
                argv = native_argv(command, instruction, harness)
                self.assertEqual(argv, expected)
                self.assertNotIn('private', json.dumps(argv))
        self.assertIsNone(native_argv('printf codex', instruction, 'codex'))

    def test_nested_tools_and_sse_without_space(self):
        from harbor_adapter.observed_config import json_records
        value = request_config({'model': 'fixture', 'tools': [{'type': 'namespace', 'name': 'functions',
                                'tools': [{'type': 'function', 'name': 'exec'}, {'type': 'custom', 'name': 'patch'}]}]})
        self.assertEqual(value['tool_names'], ['functions.exec', 'functions.patch'])
        self.assertEqual(json_records('event: message\ndata:{"model":"served"}\n\ndata: [DONE]'), [{'model': 'served'}])

    def test_retry_lower_bound_never_claims_complete_zero(self):
        from harbor_adapter.observed_config import retry_telemetry
        value = retry_telemetry({'requests': [
            {'request_sha256': 'a', 'response': {'status_code': 429}},
            {'request_sha256': 'a', 'response': {'transport_error': True}},
            {'request_sha256': 'a', 'response': {'status_code': 200}},
            {'request_sha256': 'b', 'response': {'status_code': 200}},
        ]})
        self.assertEqual(value['observed_retries'], 2)
        self.assertEqual(value['causes'], {'http_429': 1, 'transport': 1})
        self.assertFalse(value['complete'])
        self.assertFalse(retry_telemetry({})['complete'])

    def test_retained_capture_cannot_substitute_for_full_run(self):
        from harbor_adapter.observed_config import extract_exchanges
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'exchange-0129.request').write_text('{"model":"fixture"}')
            (root / 'exchange-0129.body').write_text('{"model":"served"}')
            (root / 'exchange-0129.info').write_text('status=Some(200)\nincomplete=false\n')
            self.assertTrue(extract_exchanges(root)['wire_verified'])
            (root / 'retention.info').write_text('full_run=false\nselected_successful_exchange_ranges=[(1, 128)]\n')
            reduced = extract_exchanges(root)
            self.assertFalse(reduced['wire_verified'])
            self.assertEqual(reduced['requests'][0]['response']['status_code'], 200)

    def test_inventory_discovery_and_duplicate_task_refusal(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name in RUNG_A_PLUMBING_TASKS:
                (root / name).mkdir()
                (root / name / 'task.toml').write_text('')
            value = builder.inventory(root, 'amd64', 'plumbing', ['plak-codex', 'codex'], root / 'captures')
            self.assertEqual(set(value['tasks']), set(RUNG_A_PLUMBING_TASKS))
            self.assertNotIn('resolved_composition', value['arms']['plak-codex'])
            (root / 'duplicate/write-hello').mkdir(parents=True)
            (root / 'duplicate/write-hello/task.toml').write_text('')
            with self.assertRaises(PreflightError):
                builder.inventory(root, 'amd64', 'plumbing', ['codex'], None)

    def test_freeze_is_once_per_task_and_never_inference(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'environment').mkdir()
            (root / 'task.toml').write_text('[environment]\ndocker_image="public/task:tag"\n')
            value = {'arch': 'amd64', 'tasks': {'task': {'path': str(root)}}}
            with mock.patch.object(builder, 'docker', side_effect=['pulled', json.dumps([{'Id': 'sha256:base'}]), '', 'sha256:prepared']) as docker:
                frozen = builder.freeze(value)
            self.assertEqual(frozen['tasks']['task']['image'], 'sha256:prepared')
            self.assertEqual(docker.call_count, 4)
            self.assertIn('USER root', docker.call_args.kwargs['input'])
            self.assertIn(builder.PACKAGES, docker.call_args.kwargs['input'])
            self.assertTrue(frozen['tasks']['task']['base_tag'].startswith('plak-harbor-freeze-'))
            self.assertEqual(frozen['tasks']['task']['base_image'], 'sha256:base')
            self.assertFalse(any(call.args[:2] == ('image', 'rm') for call in docker.call_args_list))

    def test_native_state_copy_excludes_auth_before_common_scan(self):
        from harbor_adapter.hygiene import NATIVE_STATE_HARVEST
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            sessions = root / 'source'
            sessions.mkdir()
            (sessions / 'session.jsonl').write_text('{"type":"synthetic"}\n')
            (sessions / 'auth.json').write_text('{"access_token":"synthetic-private-value"}')
            script = NATIVE_STATE_HARVEST.replace('/tmp/pi-cell/sessions', str(sessions))
            script = script.replace('/logs/agent/pi/sessions', str(root / 'harvest'))
            script = script.replace('/tmp/opencode-cell/xdg-data', str(root / 'absent'))
            subprocess.run([sys.executable, '-c', script], check=True)
            self.assertTrue((root / 'harvest/session.jsonl').is_file())
            self.assertFalse((root / 'harvest/auth.json').exists())

    def test_paired_supporting_tests_are_bounded_and_equivalence_is_not_superiority(self):
        from harbor_adapter.paired import estimate
        same = estimate({'one': ([True] * 3, [True] * 3)}, replicates=100)
        self.assertEqual(same['permutation_p'], 1)
        self.assertEqual(same['mcnemar_majority_p'], 1)
        self.assertTrue(same['equivalent_within_10pp'])
        different = estimate({str(i): ([True] * 3, [False] * 3) for i in range(8)}, replicates=100)
        self.assertLess(different['permutation_p'], .05)
        self.assertLess(different['mcnemar_majority_p'], .05)
        self.assertFalse(different['equivalent_within_10pp'])

class WireLifecycleTests(unittest.IsolatedAsyncioTestCase):
    async def test_environment_overlay_cell_local_data_and_bounded_actual_command(self):
        from harbor_adapter.wire import WireAgent
        class Base:
            async def exec_as_agent(self, environment, command, **kwargs):
                return SimpleNamespace(command=command, **kwargs)
        class Agent(WireAgent, Base):
            _native_harness = 'opencode'
        with tempfile.TemporaryDirectory() as directory:
            agent = Agent()
            agent.logs_dir = Path(directory)
            agent._eval_instruction = 'private-prompt'
            agent._wire_env = {'HTTPS_PROXY': 'http://127.0.0.1:18080'}
            agent._eval_deadline = 130
            with mock.patch('harbor_adapter.wire.time.monotonic', return_value=100):
                result = await agent.exec_as_agent(object(),
                    command='opencode --model=openai/fixture run -- private-prompt 2>&1 | tee /logs/out',
                    env={'XDG_DATA_HOME': '/logs/agent/opencode/xdg-data'})
            self.assertTrue(result.command.startswith('timeout --signal=TERM --kill-after=2s 30.000s'))
            self.assertEqual(result.env['XDG_DATA_HOME'], '/tmp/opencode-cell/xdg-data')
            self.assertEqual(result.env['HTTPS_PROXY'], 'http://127.0.0.1:18080')
            self.assertNotIn('private-prompt', (agent.logs_dir / 'argv.json').read_text())

    async def test_recorder_upload_readiness_and_private_trust_bundle(self):
        from harbor_adapter.wire import start_recording, harvest_recording
        agent = SimpleNamespace(_native_harness='opencode', model_name='openai/fixture',
                                exec_as_root=mock.AsyncMock(return_value=SimpleNamespace(return_code=0)))
        environment = SimpleNamespace(upload_file=mock.AsyncMock(),
                                      exec=mock.AsyncMock(return_value=SimpleNamespace(return_code=0)))
        await start_recording(agent, environment)
        self.assertEqual(environment.upload_file.await_count, 2)
        commands = [call.kwargs['command'] for call in agent.exec_as_root.await_args_list]
        self.assertTrue(any('--host chatgpt.com' in command and '--host api.openai.com' in command for command in commands))
        self.assertNotIn('/logs', agent._wire_env['NODE_EXTRA_CA_CERTS'])
        self.assertNotIn('HTTP_PROXY', agent._wire_env)
        self.assertNotIn('PLAK_HOME', agent._wire_env)
        self.assertNotIn('ANTHROPIC_BASE_URL', agent._wire_env)
        await harvest_recording(agent, environment)
        self.assertEqual(agent._wire_env, {})
        self.assertIsNone(agent._eval_deadline)
        self.assertNotIn('kill', agent.exec_as_root.call_args.kwargs['command'])
        agent.exec_as_root.return_value = SimpleNamespace(return_code=1)
        with self.assertRaises(PreflightError):
            await harvest_recording(agent, environment)

    async def test_start_failure_still_harvests_and_runs_secret_scan(self):
        from harbor_adapter.lifecycle import observe_run
        class Agent:
            @observe_run
            async def run(self, *args):
                raise AssertionError('must not dispatch after recorder startup failure')
        with tempfile.TemporaryDirectory() as directory:
            agent = Agent()
            agent.logs_dir = Path(directory)
            agent.exec_as_root = mock.AsyncMock()
            with mock.patch('harbor_adapter.cell_prep.record_cell', new_callable=mock.AsyncMock), \
                 mock.patch('harbor_adapter.preflight.task_budgets', return_value=(900, 600)), \
                 mock.patch('harbor_adapter.wire.start_recording', side_effect=PreflightError('start failed')), \
                 mock.patch('harbor_adapter.wire.harvest_recording', new_callable=mock.AsyncMock) as stop, \
                 mock.patch('harbor_adapter.lifecycle.sample_cell', new_callable=mock.AsyncMock, return_value={}):
                with self.assertRaisesRegex(PreflightError, 'start failed'):
                    await agent.run('private-prompt', object(), object())
            stop.assert_awaited_once()
            self.assertEqual(agent.exec_as_root.await_count, 1)

    def test_opencode_subscription_arm_is_calibration_only(self):
        plan = campaign()
        plan['arms']['opencode-codex-oauth'] = plan['arms'].pop('a')
        self.assertIn('calibration_only_arm', validate_campaign(plan))
        plan.update(phase='calibration', blocks=1)
        self.assertNotIn('calibration_only_arm', validate_campaign(plan))

class FreezePinTests(unittest.TestCase):
    def test_default_archive_members_are_discovered_and_node_bytes_are_verified(self):
        from harbor_adapter.vendor_pins import CLI_PINS, NODE_VERSION, node_tarball_name
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            prefix = root / '.plak/installed/amd64/vendor'
            prefix.mkdir(parents=True)
            node = prefix / node_tarball_name('amd64')
            cli = prefix / 'codex.tar.gz'
            with tarfile.open(node, 'w:gz') as archive:
                info = tarfile.TarInfo('node-version/bin/node')
                info.size = 4
                archive.addfile(info, io.BytesIO(b'node'))
            with tarfile.open(cli, 'w:gz') as archive:
                info = tarfile.TarInfo('./lib/cli.js')
                info.size = 3
                archive.addfile(info, io.BytesIO(b'cli'))
                link = tarfile.TarInfo('./bin/codex')
                link.type, link.linkname = tarfile.SYMTYPE, '../lib/cli.js'
                archive.addfile(link)
            manifest = {'arch': 'amd64', 'node': {'version': NODE_VERSION, 'file': node.name},
                        'clis': {'codex': {'version': CLI_PINS['codex'], 'file': cli.name}}}
            (prefix / 'manifest.json').write_text(json.dumps(manifest))
            with mock.patch.dict('harbor_adapter.vendor_pins.NODE_SHA256', {'amd64': hashlib.sha256(node.read_bytes()).hexdigest()}):
                binaries = builder.frozen_binaries(root, 'amd64', 'codex')
                self.assertEqual(set(binaries), {'codex', 'node', 'codex-archive', 'node-archive'})
                self.assertEqual(builder.artifact_digest(root, 'amd64', binaries['codex']), hashlib.sha256(b'cli').hexdigest())
                node.write_bytes(b'replaced')
                with self.assertRaisesRegex(PreflightError, 'Node archive'):
                    builder.frozen_binaries(root, 'amd64', 'codex')
            manifest['clis']['codex']['version'] = 'stale'
            (prefix / 'manifest.json').write_text(json.dumps(manifest))
            with self.assertRaisesRegex(PreflightError, 'vendor version'):
                builder.frozen_binaries(root, 'amd64', 'codex')

    def test_temperature_and_thinking_budgets_are_not_implicit_parity(self):
        for key, value in [('temperature', .5), ('thinking_budget', 2048)]:
            plan = campaign()
            plan['arms']['a']['observed_config'][key] = value
            self.assertIn('policy_or_wire_parity', validate_campaign(plan))
        plan = campaign()
        plan['arms']['a']['observed_config']['tool_names'] = ['functions.web_search']
        self.assertIn('web_tool_policy_mismatch', validate_campaign(plan))

    def test_argv_redaction_survives_first_party_prompt_templates(self):
        from harbor_adapter.argv import native_argv
        argv = native_argv("codex exec --json -- 'template contains private content'", 'original-task', 'codex')
        self.assertEqual(argv, ['codex', 'exec', '--json', '--'])
        argv = native_argv("grok --no-auto-update --single 'template private content' --always-approve", 'original-task', 'grok-build')
        self.assertEqual(argv, ['grok', '--no-auto-update', '--single', '--always-approve'])
        self.assertIsNone(native_argv("pi --print 'template private content'", 'original-task', 'pi'))
        self.assertEqual(native_argv('pi --print --thinking low low', 'low', 'pi'), ['pi', '--print', '--thinking', 'low'])

class PhaseTests(unittest.TestCase):
    def runner(self, root):
        import importlib.util
        path = Path(__file__).resolve().parents[1] / 'scripts/campaign.py'
        spec = importlib.util.spec_from_file_location('fixture_ready_runner', path)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        module.ROOT = root
        return module

    def test_calibration_is_one_trial_per_arm_exports_only_reduced_evidence(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            module = self.runner(root)
            plan = campaign()
            plan.update(phase='calibration', blocks=1)
            plan['arms'] = {'plak-codex': plan['arms']['a'], 'codex': plan['arms']['b']}
            plan['tasks'] = {'write-hello': plan['tasks']['task']}
            rows = []
            for arm in plan['arms']:
                trial = root / arm
                (trial / 'agent').mkdir(parents=True)
                reduced = {'source': 'exchange', 'wire_verified': True,
                           'first_request': request_config({'model': 'fixture', 'reasoning': {'effort': 'low'}}),
                           'requests': [{'response': {'served_model': 'fixture'}}]}
                (trial / 'agent/observed-config.json').write_text(json.dumps(reduced))
                (trial / 'agent/plak-stderr.log').write_text('plak: composition Code Mode (native-ptc): fixture\n')
                rows.append({'trials': [{'arm': arm, 'task': 'write-hello', 'failure_class': 'agent',
                    'pass': True, 'evidence': {'result': str(trial / 'result.json')}}]})
            with mock.patch.object(module.subprocess, 'run') as launch, \
                 mock.patch.object(module, 'build_report', side_effect=rows), \
                 mock.patch.object(module, 'discover', side_effect=lambda value: value), \
                 mock.patch.dict(os.environ, {'HARBOR_CAMPAIGN_SHA256': 'a' * 64}):
                self.assertEqual(module.run(plan, 'plumbing'), 0)
            self.assertEqual(launch.call_count, 2)
            output = next((root / '.plak/harbor/jobs').glob('campaign-*'))
            receipt = json.loads((output / 'calibration-receipt.json').read_text())
            self.assertFalse(receipt['scored'])
            self.assertFalse((output / 'selection.json').exists())
            captured = json.loads((output / 'calibrations/plak-codex.json').read_text())
            self.assertEqual(captured['resolved_composition'], 'native-ptc')

    def test_codex_recipe_refuses_missing_or_mismatched_null_receipt_before_launch(self):
        from contextlib import nullcontext
        from harbor_adapter.tuples import RUNG_A_EXPANDED_TASKS
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            module = self.runner(root)
            plan = campaign()
            plan.update(phase='scored', slice='expanded', blocks=5)
            plan['arms'] = {'plak-codex': plan['arms']['a'], 'codex': plan['arms']['b']}
            plan['tasks'] = {name: plan['tasks']['task'] for name in RUNG_A_EXPANDED_TASKS}
            with mock.patch.object(module, 'campaign_gate', return_value=plan), \
                 mock.patch.object(module, 'account_leases', return_value=nullcontext()), \
                 mock.patch.object(module, 'run', return_value=0) as run, \
                 mock.patch.object(sys, 'argv', ['campaign.py', 'codex']):
                with self.assertRaises(PreflightError):
                    module.main()
                run.assert_not_called()
                receipt = root / 'null-receipt.json'
                value = {'phase': 'null', 'passed': True, 'source_commit': plan['source_commit'],
                         'arch': plan['arch'], 'plak_binaries': plan['arms']['plak-codex']['binaries']}
                receipt.write_text(json.dumps(value))
                plan.update(null_receipt_file=str(receipt), null_receipt_sha256=hashlib.sha256(receipt.read_bytes()).hexdigest())
                self.assertEqual(module.main(), 0)
                self.assertEqual(run.call_count, 1)
                value['source_commit'] = 'f' * 40
                receipt.write_text(json.dumps(value))
                plan['null_receipt_sha256'] = hashlib.sha256(receipt.read_bytes()).hexdigest()
                with self.assertRaises(PreflightError):
                    module.main()
                self.assertEqual(run.call_count, 1)
                plan['phase'] = 'calibration'
                with self.assertRaisesRegex(PreflightError, 'phase'):
                    module.main()

class CapabilityTests(unittest.IsolatedAsyncioTestCase):
    async def test_pre_amp_jobs_binaries_refuse_before_dispatch(self):
        from harbor_adapter.preflight import probe_run_capabilities
        environment = SimpleNamespace(exec=mock.AsyncMock(return_value=SimpleNamespace(return_code=0, stdout='plak run help')))
        with self.assertRaisesRegex(PreflightError, 'amp-jobs'):
            await probe_run_capabilities(environment, '/installed-agent/plak')
        environment.exec.return_value.stdout = 'plak run --leave-jobs-running'
        await probe_run_capabilities(environment, '/installed-agent/plak')
        self.assertIn('run --help', environment.exec.call_args.kwargs['command'])
        environment.exec.return_value.return_code = 1
        with self.assertRaises(PreflightError):
            await probe_run_capabilities(environment, '/installed-agent/plak')

class CaptureTerminalTests(unittest.TestCase):
    def test_plak_capture_io_failure_is_not_a_transport_retry(self):
        from harbor_adapter.observed_config import extract_exchanges
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'exchange-0001.request').write_text('{"model":"fixture"}')
            (root / 'exchange-0001.body').write_text('')
            info = root / 'exchange-0001.info'
            info.write_text('status=None\nterminal=Failed(Failure)\nincomplete=false\n')
            reduced = extract_exchanges(root)
            self.assertTrue(reduced['wire_verified'])
            self.assertTrue(reduced['requests'][0]['response']['transport_error'])
            info.write_text('status=Some(200)\nterminal=Complete\nincomplete=true\n')
            reduced = extract_exchanges(root)
            self.assertFalse(reduced['wire_verified'])
            self.assertFalse(reduced['requests'][0]['response']['transport_error'])
            info.write_text('status=Some(200)\nterminal=ClosedEarly\nincomplete=false\n')
            reduced = extract_exchanges(root)
            self.assertTrue(reduced['requests'][0]['response']['client_cancelled'])

class RecorderCapacityTests(unittest.TestCase):
    def test_capacity_or_parser_loss_is_never_an_admitted_wire_observation(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'observed.json'
            recorder = Recorder(path)
            index = recorder.begin(b'{"model":"fixture"}')
            recorder.finish(index, b'{"model":"served"}')
            self.assertTrue(json.loads(path.read_text())['wire_verified'])
            recorder.fail()
            value = json.loads(path.read_text())
            self.assertFalse(value['wire_verified'])
            self.assertTrue(value['capture_incomplete'])

class ToolNetworkParityTests(unittest.TestCase):
    def test_non_provider_tool_traffic_is_opaque_and_not_recorded(self):
        import socketserver
        class Echo(socketserver.BaseRequestHandler):
            def handle(self):
                self.request.sendall(self.request.recv(4096))
        with tempfile.TemporaryDirectory() as directory:
            echo = socketserver.TCPServer(('127.0.0.1', 0), Echo)
            echo_thread = threading.Thread(target=echo.serve_forever, daemon=True)
            echo_thread.start()
            proxy = ProxyServer(('127.0.0.1', 0), ProxyHandler)
            proxy.hosts, proxy.passthrough = ['api.example.test'], True
            output = Path(directory) / 'observed.json'
            proxy.recorder = Recorder(output)
            proxy_thread = threading.Thread(target=proxy.serve_forever, daemon=True)
            proxy_thread.start()
            client = http.client.HTTPConnection('127.0.0.1', proxy.server_port, timeout=5)
            client.set_tunnel('127.0.0.1', echo.server_address[1])
            try:
                client.connect()
                client.sock.sendall(b'opaque-private-tool-content')
                self.assertEqual(client.sock.recv(4096), b'opaque-private-tool-content')
                self.assertFalse(output.exists())
                self.assertEqual(proxy.recorder.requests, [])
            finally:
                client.close()
                proxy.shutdown()
                proxy.server_close()
                echo.shutdown()
                echo.server_close()
                proxy_thread.join()
                echo_thread.join()

class RotatedCredentialHarvestTests(unittest.TestCase):
    def test_rotated_opencode_credentials_are_known_to_the_final_scan(self):
        from harbor_adapter.hygiene import HARVEST_SCRIPT
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'home').mkdir()
            (root / 'logs/agent').mkdir(parents=True)
            credential = root / 'synthetic-auth.json'
            credential.write_text(json.dumps({'openai': {'access': 'synthetic-rotated-access-value-123456789'}}))
            artifact = root / 'logs/agent/artifact.txt'
            artifact.write_text('unlabelled synthetic-rotated-access-value-123456789')
            script = HARVEST_SCRIPT.replace('/tmp/plak-cell/home', str(root / 'home')).replace('/logs', str(root / 'logs'))
            script = script.replace('/tmp/opencode-cell/xdg-data/opencode/auth.json', str(credential))
            for name in ('grok', 'pi', 'opencode', 'codex', 'claude'):
                script = script.replace('/tmp/' + name + '-secrets', str(root / (name + '-secrets')))
            script = script.replace('/tmp/codex-home', str(root / 'codex-home'))
            result = subprocess.run([sys.executable, '-c', script], capture_output=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertFalse(artifact.exists())
            self.assertTrue(json.loads((root / 'logs/agent/harvest.json').read_text())['secrets_harvested'])
            self.assertNotIn(b'synthetic-rotated-access-value', result.stdout + result.stderr)

class SourceImageFreezeTests(unittest.TestCase):
    def test_explicit_source_build_does_not_pull_an_amd64_only_task_tag(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'environment').mkdir()
            (root / 'task.toml').write_text('[environment]\ndocker_image="public/task:tag"\n')
            value = {'arch': 'arm64', 'tasks': {'task': {'path': str(root)}}}
            with mock.patch.object(builder, 'docker', side_effect=['sha256:base', '', 'sha256:prepared']) as docker:
                result = builder.freeze(value, build_tasks=True)
            self.assertEqual(result['tasks']['task']['image'], 'sha256:prepared')
            self.assertEqual(docker.call_count, 3)
            self.assertEqual(docker.call_args_list[0].args[0], 'build')
            self.assertIn('linux/arm64', docker.call_args_list[0].args)

class ClaudeBillingProjectionTests(unittest.TestCase):
    def body(self):
        return {'model': 'same-model', 'output_config': {'effort': 'low'},
                'thinking': {'type': 'adaptive'},
                'system': [{'type': 'text', 'text': 'x-anthropic-billing-header: cc_version=2.1.295.8a8; cc_entrypoint=cli; cch=123ab; cc_prompt_id=12345678-1234-1234-1234-123456789abc; cc_turn_origin=human;'},
                           {'type': 'text', 'text': 'All other instructions stay pinned.'}], 'tools': []}

    def evidence(self, plan, first):
        return dict(plan['tasks']['task'], secrets_harvested=False,
                    artifact={'binaries': {'binary': {'sha256': 'b' * 64}}},
                    observed_config={'wire_verified': True, 'first_request': first,
                                     'requests': [{'response': {'served_model': 'same-model'}}]})

    def test_only_known_billing_nonces_change_without_losing_exact_digests(self):
        from harbor_adapter.campaign import trial_gate
        before = self.body()
        after = copy.deepcopy(before)
        after['system'][0]['text'] = after['system'][0]['text'].replace('2.1.295.8a8', '2.1.295.05c').replace('cch=123ab', 'cch=abcde').replace('12345678-1234-1234-1234-123456789abc', 'abcdef01-2345-6789-abcd-0123456789ab')
        a, b = request_config(before), request_config(after)
        self.assertNotEqual(a['instructions_sha256'], b['instructions_sha256'])
        self.assertEqual(a['claude_instruction_projection_sha256'], b['claude_instruction_projection_sha256'])
        plan = campaign()
        plan['arms']['a'].update(account='claude', observed_config=a)
        self.assertEqual(trial_gate(plan, 'a', 'task', self.evidence(plan, b)), [])
        # Body digests remain exact rather than hashes of the projection.
        with tempfile.TemporaryDirectory() as directory:
            recorder = Recorder(Path(directory) / 'reduced.json')
            for body in (before, after):
                recorder.begin(json.dumps(body).encode())
            self.assertNotEqual(recorder.requests[0]['request_sha256'], recorder.requests[1]['request_sha256'])
            self.assertEqual(recorder.requests[0]['request']['instructions_sha256'], a['instructions_sha256'])
            self.assertEqual(recorder.requests[1]['request']['instructions_sha256'], b['instructions_sha256'])
        # No other account is admitted by this Claude-only exception.
        plan['arms']['a']['account'] = 'codex'
        self.assertIn('first_request_unverified_or_mismatched', trial_gate(plan, 'a', 'task', self.evidence(plan, b)))

    def test_unknown_billing_fields_base_version_entrypoint_or_other_content_fail(self):
        from harbor_adapter.campaign import trial_gate
        baseline = request_config(self.body())
        for mutation in ('base_version', 'entrypoint', 'unknown_field', 'other_instruction', 'effort'):
            with self.subTest(mutation=mutation):
                body = self.body()
                if mutation == 'base_version':
                    body['system'][0]['text'] = body['system'][0]['text'].replace('2.1.295', '2.1.296')
                elif mutation == 'entrypoint':
                    body['system'][0]['text'] = body['system'][0]['text'].replace('cc_entrypoint=cli', 'cc_entrypoint=ide')
                elif mutation == 'unknown_field':
                    body['system'][0]['text'] += ' cc_new_field=unreviewed;'
                elif mutation == 'effort':
                    body['output_config']['effort'] = 'high'
                else:
                    body['system'][1]['text'] = 'Changed real instruction.'
                plan = campaign()
                plan['arms']['a'].update(account='claude', observed_config=baseline)
                observed = request_config(body)
                self.assertIn('first_request_unverified_or_mismatched', trial_gate(plan, 'a', 'task', self.evidence(plan, observed)))

    def test_nonce_shaped_text_outside_the_billing_line_is_never_normalized(self):
        from harbor_adapter.observed_config import first_request_matches
        first = request_config({'model': 'fixture', 'system': 'A real instruction: cch=123ab;'})
        second = request_config({'model': 'fixture', 'system': 'A real instruction: cch=abcde;'})
        self.assertNotIn('claude_instruction_projection_sha256', first)
        self.assertFalse(first_request_matches(first, second, 'claude'))
