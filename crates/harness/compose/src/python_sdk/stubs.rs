//! The exact core stub text of §9.8.3, split where the route and the access
//! ceiling change it. The docstrings are part of the pinned text.

/// Module docstring, imports, value types and `ctx`.
pub(super) const CORE_HEAD: &str = r#""""This workspace's SDK. I/O functions are async: await one call, or asyncio.gather several."""
import asyncio as asyncio
import builtins
from typing import Any, Literal, NamedTuple

type Json = None | bool | int | float | str | builtins.list[Json] | dict[str, Json]

class Ref(NamedTuple):
    """A full stream the host keeps; ctx.read(ref) reads it."""
    id: str

class Omission(NamedTuple):
    """Something a bounded result left out; read `full` for all of it."""
    what: str
    count: int
    bytes: int
    full: Ref | None

class Image(NamedTuple):
    """An image; printing it attaches it to the cell's output."""
    id: str
    path: str
    media_type: str
    bytes: int
    width: int | None
    height: int | None

class File(NamedTuple):
    path: str
    text: str
    first_line: int
    lines: int
    eof: bool
    revision: str
    image: Image | None
    omitted: builtins.list[Omission]

class Entry(NamedTuple):
    path: str
    kind: Literal["file", "dir", "link", "other"]
    size: int

class Listing(NamedTuple):
    entries: builtins.list[Entry]
    omitted: builtins.list[Omission]

class Hit(NamedTuple):
    path: str
    line: int
    text: str

class Hits(NamedTuple):
    hits: builtins.list[Hit]
    omitted: builtins.list[Omission]

class ctx:
    """Read the workspace."""
    async def read(target: str | Ref, lines: tuple[int, int] | None = None) -> File:
        """Read a file or a Ref; `lines` is (first, last), 1-based. `revision` guards fs ops."""
    async def list(path: str = ".", glob: str | None = None, depth: int = 1) -> Listing:
        """List entries under `path`, honouring .gitignore."""
    async def grep(pattern: str, path: str = ".", glob: str | None = None) -> Hits:
        """Regex search under `path`."""

"#;

/// The `fs` class header.
pub(super) const FS_HEAD: &str = r#"class fs:
    """Change files. apply() checks every op and revision before writing any."""
"#;

/// The `fs.Replace` op.
pub(super) const FS_REPLACE: &str = r"    class Replace(NamedTuple):
        path: str
        old: str
        new: str
        count: int = 1
        expect: str | None = None
";

/// The `fs.Patch` op.
pub(super) const FS_PATCH: &str = r#"    class Patch(NamedTuple):
        """A patch envelope: *** Begin Patch ... *** End Patch."""
        text: str
"#;

/// The remaining `fs` ops, `fs.Change` and `fs.apply`.
pub(super) const FS_TAIL: &str = r#"    class Write(NamedTuple):
        path: str
        content: str
        expect: str | None = None
    class Delete(NamedTuple):
        path: str
        expect: str | None = None
    class Move(NamedTuple):
        src: str
        dst: str
        expect: str | None = None
    class Change(NamedTuple):
        path: str
        action: Literal["add", "update", "delete", "move"]
        revision: str | None
    async def apply(*ops: Replace | Patch | Write | Delete | Move) -> builtins.list[Change]:
        """Apply ops atomically. `expect` is a revision from ctx.read, or "absent"."""

"#;

/// `proc`, `state` and `plan`.
pub(super) const CORE_TAIL: &str = r#"class proc:
    """Run processes. They are not sandboxed."""
    class Job(NamedTuple):
        id: str
    class Result(NamedTuple):
        status: Literal["exited", "timed_out", "failed"]
        exit_code: int | None
        output: str
        omitted: builtins.list[Omission]
    class Status(NamedTuple):
        job: Job
        status: Literal["running", "exited", "stopped", "outcome_unknown"]
        exit_code: int | None
        output: str
        omitted: builtins.list[Omission]
    async def run(argv: builtins.list[str], cwd: str | None = None, stdin: str | None = None, timeout_s: float = 120) -> Result:
        """Run argv without a shell and wait for it."""
    async def shell(cmd: str, cwd: str | None = None, stdin: str | None = None, timeout_s: float = 120) -> Result:
        """Run a /bin/sh command and wait for it."""
    async def start(cmd: str | builtins.list[str], cwd: str | None = None, tty: bool = False) -> Job:
        """Start a long job; its completion wakes you. A list is argv, a str is a shell command."""
    async def poll(job: Job) -> Status:
        """Status and output since the last observation."""
    async def stdin(job: Job, data: str, close: bool = False) -> None: ...
    async def cancel(job: Job) -> Status: ...
    async def join(job: Job, timeout_s: float) -> Status:
        """Wait up to timeout_s for the job to end."""

class state:
    """Named state: the only memory that outlives a cell."""
    class Slot(NamedTuple):
        name: str
        bytes: int
        tainted: bool
    async def get(name: str, default: Json = None) -> Any: ...
    async def put(name: str, value: Json) -> None: ...
    async def list() -> builtins.list[Slot]: ...
    async def drop(name: str) -> bool: ...

class plan:
    class Step(NamedTuple):
        text: str
        status: Literal["pending", "in_progress", "completed"] = "pending"
    async def update(items: builtins.list[Step]) -> int:
        """Replace the plan; returns its revision."""
"#;
