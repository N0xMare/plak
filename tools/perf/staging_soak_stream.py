"""Root presentation checks for the staging soak's synthetic prose."""

import re
from collections import Counter
from dataclasses import asdict, dataclass

DELTA_MS = 50
CHUNKS = 64
QUICK_CHUNKS = 16
PROVISIONAL = 1 << 63
MARK = re.compile(r"^soak stream (\d{4}) part (\d{3}) — steady synthetic prose\.(?:\n|$)", re.M)
LATE_PREFIX = "late_attach_missing_live_prefix"
CHECKPOINT_MISSING = "checkpoint_missing_deltas"


def chunk(turn, part):
    return f"soak stream {turn:04d} part {part:03d} — steady synthetic prose.\n"


def scripted_turn(tool, turn, chunks):
    preamble = {"reasoning": "Check the synthetic process result."}
    postamble = {"reasoning": "Use that result in the reply."}
    pieces = [chunk(turn, part) for part in range(chunks)]
    return [
        {**tool, "stream": [preamble]},
        {
            "stream": [
                postamble,
                *[piece for text in pieces for piece in ({"text": text}, {"stall": DELTA_MS})],
            ],
            "output": {"text": "".join(pieces)},
        },
    ]


def key(raw):
    return raw["anchor"], raw["lane"], raw["ordinal"]


def parts(text):
    matched = list(MARK.finditer(text))
    if "".join(match[0] for match in matched) != text:
        raise ValueError("malformed synthetic reply")
    return [(int(match[1]), int(match[2])) for match in matched]


@dataclass
class Counts:
    baselines: int = 0
    patches: int = 0
    gap_frames: int = 0
    revision_gaps: int = 0
    assistant_frames: int = 0
    evicted_deltas: int = 0
    unseeded_deltas: int = 0
    reasoning_frames: int = 0
    tool_upserts: int = 0
    completed_texts: int = 0
    authority_checks: int = 0
    mismatches: int = 0


class RootTrace:
    """Project keyed rows and live appends separately and compare at boundaries."""

    def __init__(self, chunks, *, continuous=False):
        self.chunks = chunks
        self.continuous = continuous
        self.counts = Counts()
        self.first_issue = None
        self.reasons = Counter()
        self.pending_prefixes = {}
        self.paired_prefixes = 0
        self.seq = 0
        self.revision = None
        self.pages = {}
        self.rows = {}
        self.live = {}
        self.settled = {}
        self.checkpoints = {}
        self.streamed = set()
        self.latest_parts = {}

    def issue(self, reason, **fields):
        self.counts.mismatches += 1
        self.reasons[reason] += 1
        if self.first_issue is None:
            self.first_issue = {"reason": reason, **fields}

    def assistant(self, text):
        self.counts.assistant_frames += 1
        try:
            tokens = parts(text)
        except ValueError:
            self.issue("malformed_delta")
            return
        for turn, part in tokens:
            if turn in self.settled:
                self.issue("delta_after_settlement", turn=turn, part=part)
                continue
            seen = self.live.setdefault(turn, set())
            previous = self.latest_parts.get(turn, -1)
            if part <= previous or part >= self.chunks:
                self.issue("unordered_delta", turn=turn, part=part, previous=previous)
            elif part > previous + 1:
                missing = part - previous - 1
                if previous == -1 and not self.continuous:
                    self.counts.unseeded_deltas += missing
                    self.pending_prefixes[turn] = missing
                    self.issue(
                        LATE_PREFIX,
                        turn=turn,
                        part=part,
                        missing=missing,
                    )
                else:
                    self.counts.evicted_deltas += missing
                    self.issue("missing_live_delta", turn=turn, part=part, missing=missing)
            self.latest_parts[turn] = part
            seen.add(part)
            self.streamed.add(turn)

    def boundary(self, previous_rows, *, initial, patch=False):
        for row_key, text in self.rows.items():
            if not text.startswith("soak stream ") or previous_rows.get(row_key) == text:
                continue
            try:
                tokens = parts(text)
            except ValueError:
                self.issue("malformed_checkpoint")
                continue
            if not tokens:
                self.issue("empty_checkpoint")
                continue
            turn = tokens[0][0]
            supplied = {part for _, part in tokens}
            if tokens != [(turn, part) for part in range(len(tokens))] or len(tokens) > self.chunks:
                self.issue("non_prefix_checkpoint", turn=turn, parts=len(tokens))
                continue
            committed = row_key[0] < PROVISIONAL
            if committed and len(tokens) != self.chunks:
                self.issue(
                    "short_committed_text",
                    turn=turn,
                    expected=self.chunks,
                    actual=len(tokens),
                )
            if turn in self.settled:
                if self.settled[turn] != text:
                    self.issue("changed_settled_text", turn=turn)
                continue
            seen = self.live.get(turn)
            if not initial and seen is None and self.continuous:
                self.counts.evicted_deltas += len(supplied)
                self.issue("checkpoint_without_deltas", turn=turn, missing=len(supplied))
            if not initial and seen is not None and (self.continuous or turn in self.streamed):
                missing = supplied - seen
                extra = seen - supplied
                prefix = self.pending_prefixes.pop(turn, None)
                if missing:
                    self.counts.evicted_deltas += sum(
                        part > self.latest_parts[turn] for part in missing
                    )
                    self.issue(CHECKPOINT_MISSING, turn=turn, missing=len(missing))
                    if patch and prefix and missing == set(range(prefix)):
                        self.paired_prefixes += 1
                if extra:
                    self.issue("checkpoint_missing_host_text", turn=turn, missing=len(extra))
            self.live[turn] = supplied
            self.latest_parts[turn] = max(supplied)
            self.streamed.discard(turn)
            if committed:
                self.settled[turn] = text
                self.counts.completed_texts += 1
                self.live.pop(turn)
                self.latest_parts.pop(turn)
        state = tuple(sorted(self.rows.items()))
        previous = self.checkpoints.setdefault(self.revision, state)
        if previous != state:
            self.issue("same_revision_different_text", revision=self.revision)

    def upsert(self, items, target):
        for item in items:
            row = item["row"]
            row_key = key(item["key"])
            if row["kind"] == "assistant":
                target[row_key] = row["text"]
            else:
                target.pop(row_key, None)
                self.counts.tool_upserts += row["kind"] == "tool"

    def observe(self, frame):
        kind = frame["frame"]
        if kind == "snapshot" and frame["scope"] == "root":
            page = frame["page"]
            if page == 0:
                self.seq += 1
                self.pages = {}
            self.upsert(frame["items"], self.pages)
            if frame["last"]:
                self.counts.baselines += 1
                self.revision = frame["revision"]
                previous, self.rows = self.rows, self.pages
                self.boundary(previous, initial=self.counts.baselines == 1)
        elif kind == "event":
            self.counts.gap_frames += max(0, frame["seq"] - self.seq - 1)
            self.seq = frame["seq"]
            observation = frame["observation"]
            event = observation["kind"]
            if event == "assistant_delta":
                self.assistant(observation["body"])
            elif event == "reasoning_delta":
                self.counts.reasoning_frames += 1
            elif event == "transcript_patch" and observation["body"]["scope"] == "root":
                patch = observation["body"]
                if self.revision != patch["from_revision"]:
                    self.counts.revision_gaps += 1
                    self.issue(
                        "revision_gap",
                        held=self.revision,
                        incoming=patch["from_revision"],
                    )
                    return
                previous = self.rows.copy()
                for deleted in patch["deletes"]:
                    self.rows.pop(key(deleted), None)
                self.upsert(patch["upserts"], self.rows)
                self.counts.patches += 1
                self.revision = patch["to_revision"]
                self.boundary(previous, initial=False, patch=True)

    def receipt(self, client, attachment, node, durable, reference):
        for revision, rows in self.checkpoints.items():
            if revision not in reference:
                self.issue("host_checkpoint_not_observed", revision=revision)
                continue
            self.counts.authority_checks += 1
            if reference[revision] != rows:
                self.issue("host_checkpoint_mismatch", revision=revision)
        for turn, text in self.settled.items():
            if durable.get(turn) != text:
                self.issue("durable_text_mismatch", turn=turn)
        return {
            "client": client,
            "attachment": attachment,
            "root": node,
            "continuous": self.continuous,
            **asdict(self.counts),
            "reconcile_reasons": dict(self.reasons),
            "paired_prefixes": self.paired_prefixes,
            "first_issue": self.first_issue,
        }
