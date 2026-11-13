"""Derive public control fixtures from complete real-client recordings."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import re

UUID = re.compile(r'^[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}$')
IDENTITIES = {'session', 'tree_id', 'agent_id', 'client_id', 'request_id'}
LEGAL_READS = {
    'refresh_composer_chrome': 'timer-driven background chrome',
    'resync_transcript': 'resync at a reconnect sequence boundary',
    'list_agent_acknowledgements': 'acknowledgements read',
}
READ_ACK = {'ok': {'answer': 'accepted', 'duplicate': False}}


def capture_audit(raw: dict) -> dict:
    """Keep real identities and complete discovery exclusions outside controls."""
    grouped: dict[int, list[dict]] = {}
    for entry in raw['entries']:
        if 'connection' in entry:
            grouped.setdefault(entry['connection'], []).append(entry)
    probes = []
    connections = []
    for connection, entries in grouped.items():
        if (
            len(entries) == 3
            and entries[0] == {'connection': connection, 'opened': True}
            and {entry.get('eof') for entry in entries[1:]} == {'client', 'host'}
            and all(set(entry) == {'connection', 'eof'} for entry in entries[1:])
        ):
            probes.append(
                {
                    'connection': connection,
                    'reason': 'complete frame-free discovery probe',
                    'entries': entries,
                }
            )
        else:
            connections.append({'connection': connection, 'alias': len(connections) + 1})
    identities: dict[str, dict[str, dict]] = {}
    aliases: dict[str, dict[str, str]] = {'client': {}, 'host': {}}
    for entry in raw['entries']:
        frame = entry.get('frame', {})
        role = entry.get('direction')
        if (role, frame.get('frame')) not in {('client', 'hello'), ('host', 'welcome')}:
            continue
        binary = frame['binary_id']
        if not isinstance(binary, str):
            raise ValueError('binary identity is not a string')
        alias = aliases[role].setdefault(binary, f'<{role}-binary:{len(aliases[role]) + 1}>')
        roles = identities.setdefault(binary, {})
        provenance = roles.setdefault(role, {'role': role, 'alias': alias, 'connections': []})
        if entry['connection'] not in provenance['connections']:
            provenance['connections'].append(entry['connection'])
    return {
        'excluded_connections': probes,
        'provenance': {
            'capture': raw.get('provenance', {}),
            'connections': connections,
            'binaries': [
                {'binary_id': binary, 'roles': list(roles.values())}
                for binary, roles in identities.items()
            ],
        },
    }


def read_audit(raw: dict) -> tuple[set[int], dict]:
    """Validate the reviewed legal reads, retaining every complete pair.

    No other intent or response is eligible. Keep both complete frames in the
    audit fixture so the public read-only successor exercises every exclusion.
    The complete, unfiltered recording remains the capture evidence.
    """
    excluded: set[int] = set()
    pairs = []
    entries = raw['entries']
    requests = set()
    closed = set()
    attaches = set()
    attachments: dict[int, set[int]] = {}
    for index, entry in enumerate(entries):
        frame = entry.get('frame', {})
        op = frame.get('op', {})
        intent = op.get('intent', {})
        connection = entry.get('connection')
        if frame.get('frame') == 'bye':
            closed.add(connection)
        if entry.get('direction') == 'host' and frame.get('frame') == 'response':
            answer = frame.get('outcome', {}).get('ok', {})
            if (connection, frame.get('id')) in attaches and answer.get('answer') == 'attached':
                attachments.setdefault(connection, set()).add(answer['attachment'])
        if entry.get('direction') != 'client':
            continue
        if op.get('op') == 'attach' and connection not in closed:
            attaches.add((connection, frame.get('id')))
        if op.get('op') == 'detach':
            attachments.setdefault(connection, set()).discard(op.get('attachment'))
        kind, body = intent.get('kind'), intent.get('body')
        name = None
        expected_intent = None
        if kind == 'extension' and body == 'refresh_composer_chrome':
            name = 'refresh_composer_chrome'
            expected_intent = {'kind': 'extension', 'body': name}
        elif kind == 'resync_transcript':
            name = kind
            expected_intent = {'kind': kind, 'body': {'scope': 'root'}}
        elif (
            kind == 'extension' and isinstance(body, dict) and 'list_agent_acknowledgements' in body
        ):
            name = 'list_agent_acknowledgements'
            tree = body[name].get('tree_id') if isinstance(body[name], dict) else None
            if not isinstance(tree, str) or not 1 <= len(tree) <= 128:
                raise ValueError('malformed acknowledgements read tree identity')
            expected_intent = {'kind': kind, 'body': {name: {'tree_id': tree}}}
        if name is None:
            continue
        if connection in closed or op.get('attachment') not in attachments.get(connection, set()):
            raise ValueError('legal read is not on a live attachment of this connection')
        assert name in LEGAL_READS
        request = frame.get('id')
        attachment = op.get('attachment')
        if (
            type(request) is not int
            or request <= 0
            or type(attachment) is not int
            or attachment <= 0
            or frame
            != {
                'frame': 'request',
                'id': request,
                'op': {
                    'op': 'intent',
                    'attachment': attachment,
                    'blobs': [],
                    'intent': expected_intent,
                },
            }
        ):
            raise ValueError(f'malformed legal read: {name}')
        key = (entry['connection'], request)
        if key in requests:
            raise ValueError('duplicate legal-read request')
        requests.add(key)
        answers = [
            (position, answer)
            for position, answer in enumerate(entries)
            if answer.get('connection') == key[0]
            and answer.get('direction') == 'host'
            and answer.get('frame', {}).get('frame') == 'response'
            and answer['frame'].get('id') == request
        ]
        expected = {'frame': 'response', 'id': request, 'outcome': READ_ACK}
        if len(answers) != 1 or answers[0][0] <= index or answers[0][1]['frame'] != expected:
            raise ValueError('legal read lacks one matching accepted acknowledgement')
        position, answer = answers[0]
        excluded.update((index, position))
        pairs.append(
            {
                'read': name,
                'reason': LEGAL_READS[name],
                'connection': key[0],
                'request': frame,
                'acknowledgement': answer['frame'],
            }
        )
    return excluded, {'fixture': raw['fixture'], 'pairs': pairs, **capture_audit(raw)}


def normalize(raw: dict) -> dict:
    """Keep action frames, full control answers and explicit barriers.

    Unsolicited observations are not control answers: retain them in the raw
    recording and assert their runtime effects at replay barriers. Do not
    normalize semantic values that merely happen to resemble generated IDs.
    """
    identities: dict[str, str] = {}
    generations: dict[int, str] = {}
    excluded, audit = read_audit(raw)
    probes = {entry['connection'] for entry in audit['excluded_connections']}
    connections = {
        entry['connection']: entry['alias'] for entry in audit['provenance']['connections']
    }
    binaries = {
        (role['role'], binary['binary_id']): role['alias']
        for binary in audit['provenance']['binaries']
        for role in binary['roles']
    }

    def value(item, key=''):
        if isinstance(item, dict):
            return {name: value(part, name) for name, part in item.items()}
        if isinstance(item, list):
            return [value(part, key) for part in item]
        if type(item) is int and key in {'created_at', 'updated_at'}:
            return '@time'
        if type(item) is int and key == 'host_generation':
            return generations.setdefault(item, f'@generation:{len(generations)}')
        if isinstance(item, str):
            if key in IDENTITIES and UUID.fullmatch(item):
                return identities.setdefault(item, f'@uuid:{len(identities)}')
            if key == 'workspace' and item == raw['workspace']:
                return '@workspace'
        return item

    seed = value(raw['session'], 'session')
    # Allocate aliases in direction order, not a duplex scheduling order.
    # A response can introduce an identity while an unrelated request is sent.
    for role in ('client', 'host'):
        for index, entry in enumerate(raw['entries']):
            frame = entry.get('frame')
            if (
                index not in excluded
                and entry.get('connection') not in probes
                and entry.get('direction') == role
                and frame is not None
                and (role == 'client' or frame['frame'] in ('welcome', 'refused', 'response'))
            ):
                value(entry)
    entries = []
    correlations: dict[int, dict[int, int]] = {}
    for index, entry in enumerate(raw['entries']):
        if index in excluded or entry.get('connection') in probes:
            continue
        frame = entry.get('frame')
        if (
            frame is not None
            and entry.get('direction') == 'host'
            and frame['frame'] not in ('welcome', 'refused', 'response')
        ):
            continue
        normalized = value(entry)
        if 'connection' in entry:
            normalized['connection'] = connections[entry['connection']]
        role = entry.get('direction')
        if frame is not None and (role, frame['frame']) in {
            ('client', 'hello'),
            ('host', 'welcome'),
        }:
            normalized['frame']['binary_id'] = binaries[role, frame['binary_id']]
        if frame is not None and frame['frame'] in ('request', 'response'):
            aliases = correlations.setdefault(entry.get('connection', 0), {})
            original = frame['id']
            if frame['frame'] == 'request':
                aliases.setdefault(original, len(aliases) + 1)
            if original not in aliases:
                raise ValueError('control response has no preceding retained request')
            normalized['frame']['id'] = aliases[original]
        entries.append(normalized)
    return {
        'fixture': raw['fixture'],
        'source': 'real public TUI capture',
        'seed_session': seed,
        'entries': entries,
    }


def control_sequences(document: dict) -> dict:
    """Project exact directional controls, validating cross-direction causality.

    Shared connection opens and semantic barriers fence both sequences. EOFs
    fence their own direction; a write half-close may precede drained answers.
    No frames are sorted or removed here. The entries remain the original
    duplex stimulus for replay, not a claimed canonical duplex schedule.
    """
    sequences: dict[str, list[dict]] = {'client': [], 'host': []}
    requests: dict[tuple[int, int], dict] = {}
    answered = set()
    attachments: dict[int, set[int]] = {}
    closed: set[tuple[int, str]] = set()
    hellos = set()
    for entry in document['entries']:
        connection = entry.get('connection', 0)
        role = entry.get('direction', entry.get('eof'))
        frame = entry.get('frame')
        if frame is not None:
            if role not in sequences:
                raise ValueError('control frame has no direction')
            if (connection, role) in closed:
                raise ValueError('control frame follows direction close')
            kind = frame['frame']
            if kind == 'hello' and role == 'client':
                hellos.add(connection)
            elif kind == 'welcome' and role == 'host' and connection not in hellos:
                raise ValueError('welcome precedes hello')
            elif kind == 'request' and role == 'client':
                key = (connection, frame['id'])
                if key in requests:
                    raise ValueError('duplicate control request')
                op = frame.get('op', {})
                live = attachments.setdefault(connection, set())
                if 'attachment' in op and op['attachment'] not in live:
                    raise ValueError('control request is not on a live attachment')
                if op.get('op') == 'detach':
                    live.remove(op['attachment'])
                requests[key] = op
            elif kind == 'response' and role == 'host':
                key = (connection, frame['id'])
                if key not in requests:
                    raise ValueError('control response precedes its request')
                if key in answered:
                    raise ValueError('duplicate control response')
                answered.add(key)
                answer = frame.get('outcome', {}).get('ok', {})
                if answer.get('answer') == 'attached':
                    if requests[key].get('op') != 'attach' or (connection, 'client') in closed:
                        raise ValueError('attachment grant outside its lifetime')
                    attachments.setdefault(connection, set()).add(answer['attachment'])
            if kind == 'bye':
                closed.add((connection, role))
                if role == 'host':
                    closed.add((connection, 'client'))
        if 'eof' in entry:
            closed.add((connection, role))
        if role in sequences:
            sequences[role].append(entry)
        else:
            for sequence in sequences.values():
                sequence.append(entry)
    return {**{key: value for key, value in document.items() if key != 'entries'}, **sequences}


def compare_controls(expected: dict, actual: dict) -> None:
    """Reject any directional control or semantic-boundary difference."""
    if control_sequences(expected) != control_sequences(actual):
        raise ValueError('directional controls or semantic boundaries differ')


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--check', action='store_true')
    parser.add_argument(
        '--check-controls',
        action='store_true',
        help='compare normalized directional controls only, not capture-specific reads/provenance',
    )
    parser.add_argument('recordings', type=Path, nargs='+')
    args = parser.parse_args()
    checking = args.check or args.check_controls
    if not checking:
        args.output.mkdir(parents=True, exist_ok=True)
    for recording in args.recordings:
        raw = json.loads(recording.read_text())
        document = normalize(raw)
        control_sequences(document)
        _, audit = read_audit(raw)
        audit_target = args.output / f'{document["fixture"]}-reads-audit.json'
        audit_encoded = json.dumps(audit, indent=2) + '\n'
        if not checking:
            audit_target.write_text(audit_encoded)
        target = args.output / f'{document["fixture"]}.json'
        encoded = json.dumps(document, indent=2) + '\n'
        if checking:
            if args.check and audit_target.read_text() != audit_encoded:
                raise SystemExit(f'reads audit differs from real client: {audit_target}')
            try:
                compare_controls(json.loads(target.read_text()), document)
            except ValueError as error:
                raise SystemExit(
                    f'control fixture differs from real client: {target}: {error}'
                ) from error
        else:
            target.write_text(encoded)
        print(f'{target.name}: {len(document["entries"])} control entries')


if __name__ == '__main__':
    main()
