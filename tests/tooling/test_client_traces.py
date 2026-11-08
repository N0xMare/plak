"""The trace pin may mask generated identities, never user semantics."""

import importlib.util
import copy
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

PATH = Path(__file__).resolve().parents[2] / 'tools/dev/client_traces.py'
SPEC = importlib.util.spec_from_file_location('client_traces', PATH)
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


def attach_entries():
    return [
        {
            'connection': 1,
            'direction': 'client',
            'frame': {
                'frame': 'request',
                'id': 1,
                'op': {'op': 'attach'},
            },
        },
        {
            'connection': 1,
            'direction': 'host',
            'frame': {
                'frame': 'response',
                'id': 1,
                'outcome': {'ok': {'answer': 'attached', 'attachment': 1}},
            },
        },
    ]


class ClientTraceTests(unittest.TestCase):
    def test_required_subsequence_is_exact_with_only_the_three_reviewed_reads(self):
        request = {
            'connection': 1,
            'direction': 'client',
            'frame': {
                'frame': 'request',
                'id': 10,
                'op': {'op': 'list_sessions', 'include_archived': False},
            },
        }
        response = {
            'connection': 1,
            'direction': 'host',
            'frame': {
                'frame': 'response',
                'id': 10,
                'outcome': {'ok': {'answer': 'sessions', 'rows': []}},
            },
        }
        raw = {
            'fixture': 'required',
            'session': 'source',
            'workspace': '/fixture',
            'entries': attach_entries()
            + [
                request,
                response,
                {'barrier': 'answered'},
                {'connection': 1, 'direction': 'client', 'frame': {'frame': 'bye'}},
            ],
        }
        pinned = MODULE.normalize(raw)
        intents = {
            'refresh_composer_chrome': {'kind': 'extension', 'body': 'refresh_composer_chrome'},
            'resync_transcript': {'kind': 'resync_transcript', 'body': {'scope': 'root'}},
            'list_agent_acknowledgements': {
                'kind': 'extension',
                'body': {'list_agent_acknowledgements': {'tree_id': 'source'}},
            },
        }
        self.assertEqual(set(MODULE.LEGAL_READS), set(intents))
        for name, intent in intents.items():
            for gap in range(2, len(raw['entries'])):
                interleaved = copy.deepcopy(raw)
                read = {
                    'connection': 1,
                    'direction': 'client',
                    'frame': {
                        'frame': 'request',
                        'id': 99,
                        'op': {'op': 'intent', 'attachment': 1, 'blobs': [], 'intent': intent},
                    },
                }
                ack = {
                    'connection': 1,
                    'direction': 'host',
                    'frame': {
                        'frame': 'response',
                        'id': 99,
                        'outcome': {'ok': {'answer': 'accepted', 'duplicate': False}},
                    },
                }
                interleaved['entries'][gap:gap] = [read, ack]
                with self.subTest(read=name, gap=gap):
                    self.assertEqual(MODULE.normalize(interleaved), pinned)
                    self.assertEqual(len(MODULE.read_audit(interleaved)[1]['pairs']), 1)
        mutations = [
            [request, {'barrier': 'answered'}, response, raw['entries'][-1]],
            raw['entries'] + [copy.deepcopy(request)],
            raw['entries'][:-1],
            raw['entries']
            + [
                {
                    'connection': 1,
                    'direction': 'client',
                    'frame': {
                        'frame': 'request',
                        'id': 99,
                        'op': {'op': 'rename_session', 'session': 'source', 'title': 'changed'},
                    },
                }
            ],
        ]
        for entries in mutations:
            changed = copy.deepcopy(raw)
            changed['entries'] = entries
            self.assertNotEqual(MODULE.normalize(changed), pinned)

    def test_only_exact_chrome_read_and_its_audited_acknowledgement_are_excluded(self):
        read = {
            'connection': 1,
            'direction': 'client',
            'frame': {
                'frame': 'request',
                'id': 4,
                'op': {
                    'op': 'intent',
                    'attachment': 1,
                    'blobs': [],
                    'intent': {'kind': 'extension', 'body': 'refresh_composer_chrome'},
                },
            },
        }
        answer = {
            'connection': 1,
            'direction': 'host',
            'frame': {
                'frame': 'response',
                'id': 4,
                'outcome': {'ok': {'answer': 'accepted', 'duplicate': False}},
            },
        }
        other = copy.deepcopy(read)
        other['frame']['id'] = 5
        other['frame']['op']['intent']['body'] = 'refresh_composer_chrome_extra'
        raw = {
            'fixture': 'audit',
            'session': 'source',
            'workspace': '/fixture',
            'entries': attach_entries() + [read, other, {'barrier': 'answered'}, answer],
        }
        excluded, audit = MODULE.read_audit(raw)
        self.assertEqual(excluded, {2, 5})
        self.assertEqual(
            audit['pairs'],
            [
                {
                    'read': 'refresh_composer_chrome',
                    'reason': 'timer-driven background chrome',
                    'connection': 1,
                    'request': read['frame'],
                    'acknowledgement': answer['frame'],
                }
            ],
        )
        retained = copy.deepcopy(other)
        retained['frame']['id'] = 2
        self.assertEqual(
            MODULE.normalize(raw)['entries'], attach_entries() + [retained, {'barrier': 'answered'}]
        )
        for mutate in (
            'missing',
            'duplicate',
            'refused',
            'malformed',
            'wrong_connection',
            'after_bye',
            'after_host_bye',
            'unknown_attachment',
            'before_attach',
        ):
            broken = copy.deepcopy(raw)
            if mutate == 'missing':
                broken['entries'].pop()
            elif mutate == 'duplicate':
                broken['entries'].append(answer)
            elif mutate == 'refused':
                broken['entries'][-1]['frame']['outcome'] = {'err': 'refused'}
            elif mutate == 'malformed':
                broken['entries'][2]['frame']['op']['blobs'] = ['unexpected']
            elif mutate == 'wrong_connection':
                broken['entries'][-1]['connection'] = 2
            elif mutate in ('after_bye', 'after_host_bye'):
                broken['entries'].insert(
                    2,
                    {
                        'connection': 1,
                        'direction': 'host' if mutate == 'after_host_bye' else 'client',
                        'frame': {'frame': 'bye'},
                    },
                )
            elif mutate == 'unknown_attachment':
                broken['entries'][2]['frame']['op']['attachment'] = 2
            else:
                broken['entries'] = broken['entries'][2:]
            with self.subTest(mutate=mutate), self.assertRaises(ValueError):
                MODULE.normalize(broken)

    def test_check_rejects_a_drifted_reads_audit(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            recording = root / 'raw.json'
            recording.write_text(
                json.dumps(
                    {
                        'fixture': 'check',
                        'session': 'source',
                        'workspace': '/fixture',
                        'entries': [],
                    }
                )
            )
            command = [sys.executable, str(PATH), '--output', str(root), str(recording)]
            subprocess.run(command, check=True, capture_output=True)
            subprocess.run(command + ['--check'], check=True, capture_output=True)
            (root / 'check-reads-audit.json').write_text('{}\n')
            result = subprocess.run(command + ['--check'], capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn('reads audit differs from real client', result.stderr)

    def test_control_order_identity_aliases_and_semantic_values_survive(self):
        first = '00000000-0000-4000-8000-000000000001'
        second = '00000000-0000-4000-8000-000000000002'
        raw = {
            'fixture': 'contract',
            'session': first,
            'workspace': '/fixture/workspace',
            'entries': [
                {'connection': 1, 'opened': True},
                {
                    'direction': 'client',
                    'frame': {
                        'frame': 'request',
                        'id': 7,
                        'op': {
                            'session': first,
                            'request_id': second,
                            'workspace': '/fixture/workspace',
                            'text': first,
                            'number': 37,
                            'caps': ['z', 'a'],
                        },
                    },
                },
                {
                    'direction': 'host',
                    'frame': {
                        'frame': 'response',
                        'id': 7,
                        'session': first,
                        'updated_at': 100,
                        'activity_seq': 9,
                    },
                },
                {'barrier': 'answered'},
                {
                    'direction': 'host',
                    'frame': {'frame': 'event', 'observation': 'raw retained separately'},
                },
                {'connection': 1, 'eof': 'client'},
            ],
        }
        result = MODULE.normalize(raw)
        self.assertEqual(result['seed_session'], '@uuid:0')
        entries = result['entries']
        self.assertEqual(len(entries), 5)
        request = entries[1]['frame']
        self.assertEqual(request['id'], 1)
        self.assertEqual(
            request['op'],
            {
                'session': '@uuid:0',
                'request_id': '@uuid:1',
                'workspace': '@workspace',
                'text': first,
                'number': 37,
                'caps': ['z', 'a'],
            },
        )
        self.assertEqual(
            entries[2]['frame'],
            {
                'frame': 'response',
                'id': 1,
                'session': '@uuid:0',
                'updated_at': '@time',
                'activity_seq': 9,
            },
        )
        self.assertEqual(entries[3:], [{'barrier': 'answered'}, {'connection': 1, 'eof': 'client'}])
