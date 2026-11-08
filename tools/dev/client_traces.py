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
        if entry.get('direction') == 'host' and frame.get('frame') == 'response':
            answer = frame.get('outcome', {}).get('ok', {})
            if (connection, frame.get('id')) in attaches and answer.get('answer') == 'attached':
                attachments.setdefault(connection, set()).add(answer['attachment'])
        if entry.get('direction') != 'client':
            continue
        if frame.get('frame') == 'bye':
            closed.add(connection)
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
    return excluded, {'fixture': raw['fixture'], 'pairs': pairs}


def normalize(raw: dict) -> dict:
    """Keep action frames, full control answers and explicit barriers.

    Unsolicited observations are not control answers: retain them in the raw
    recording and assert their runtime effects at replay barriers. Do not
    normalize semantic values that merely happen to resemble generated IDs.
    """
    identities: dict[str, str] = {}
    generations: dict[int, str] = {}
    excluded, _ = read_audit(raw)

    def value(item, key=''):
        if isinstance(item, dict):
            return {name: value(part, name) for name, part in item.items()}
        if isinstance(item, list):
            return [value(part, key) for part in item]
        if type(item) is int and key == 'updated_at':
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
    entries = []
    correlations: dict[int, dict[int, int]] = {}
    for index, entry in enumerate(raw['entries']):
        if index in excluded:
            continue
        frame = entry.get('frame')
        if (
            frame is not None
            and entry.get('direction') == 'host'
            and frame['frame'] not in ('welcome', 'refused', 'response')
        ):
            continue
        normalized = value(entry)
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


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--check', action='store_true')
    parser.add_argument('recordings', type=Path, nargs='+')
    args = parser.parse_args()
    if not args.check:
        args.output.mkdir(parents=True, exist_ok=True)
    for recording in args.recordings:
        raw = json.loads(recording.read_text())
        document = normalize(raw)
        _, audit = read_audit(raw)
        audit_target = args.output / f'{document["fixture"]}-reads-audit.json'
        audit_encoded = json.dumps(audit, indent=2) + '\n'
        if not args.check:
            audit_target.write_text(audit_encoded)
        target = args.output / f'{document["fixture"]}.json'
        encoded = json.dumps(document, indent=2) + '\n'
        if args.check:
            if audit_target.read_text() != audit_encoded:
                raise SystemExit(f'reads audit differs from real client: {audit_target}')
            if target.read_text() != encoded:
                raise SystemExit(f'control fixture differs from real client: {target}')
        else:
            target.write_text(encoded)
        print(f'{target.name}: {len(document["entries"])} control entries')


if __name__ == '__main__':
    main()
