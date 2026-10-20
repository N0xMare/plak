//! The fixed bootstrap program that binds, at runtime, exactly the names the
//! stubs declare. It runs with type checking skipped, imports the private host
//! module once, and leaves no name the stubs do not declare except its own
//! spent installer, which it rebinds to `None`.

use std::fmt::Write as _;

use super::{PythonHarnessSdkAccessV1, PythonHarnessSdkOpOrderV1};

const HEAD: &str = r"def _plakous_sdk_install():
    from collections import namedtuple
    import _sdk_host as h
    Ref = namedtuple('Ref', ['id'])
    Omission = namedtuple('Omission', ['what', 'count', 'bytes', 'full'])
    Image = namedtuple('Image', ['id', 'path', 'media_type', 'bytes', 'width', 'height'])
    File = namedtuple('File', ['path', 'text', 'first_line', 'lines', 'eof', 'revision', 'image', 'omitted'])
    Entry = namedtuple('Entry', ['path', 'kind', 'size'])
    Listing = namedtuple('Listing', ['entries', 'omitted'])
    Hit = namedtuple('Hit', ['path', 'line', 'text'])
    Hits = namedtuple('Hits', ['hits', 'omitted'])
    class ctx:
        async def read(target, lines=None):
            return await h.ctx_read(target, lines)
        async def list(path='.', glob=None, depth=1):
            return await h.ctx_list(path, glob, depth)
        async def grep(pattern, path='.', glob=None):
            return await h.ctx_grep(pattern, path, glob)
";

const FS_HEAD: &str = "    class fs:\n";
const FS_REPLACE: &str = "        Replace = namedtuple('Replace', ['path', 'old', 'new', 'count', 'expect'], defaults=[1, None])\n";
const FS_PATCH: &str = "        Patch = namedtuple('Patch', ['text'])\n";
const FS_TAIL: &str = r"        Write = namedtuple('Write', ['path', 'content', 'expect'], defaults=[None])
        Delete = namedtuple('Delete', ['path', 'expect'], defaults=[None])
        Move = namedtuple('Move', ['src', 'dst', 'expect'], defaults=[None])
        Change = namedtuple('Change', ['path', 'action', 'revision'])
        async def apply(*ops):
            return await h.fs_apply(list(ops))
";

const TAIL: &str = r"    class proc:
        Job = namedtuple('Job', ['id'])
        Result = namedtuple('Result', ['status', 'exit_code', 'output', 'omitted'])
        Status = namedtuple('Status', ['job', 'status', 'exit_code', 'output', 'omitted'])
        async def run(argv, cwd=None, stdin=None, timeout_s=120):
            return await h.proc_run(argv, cwd, stdin, timeout_s)
        async def shell(cmd, cwd=None, stdin=None, timeout_s=120):
            return await h.proc_shell(cmd, cwd, stdin, timeout_s)
        async def start(cmd, cwd=None, tty=False):
            return await h.proc_start(cmd, cwd, tty)
        async def poll(job):
            return await h.proc_poll(job)
        async def stdin(job, data, close=False):
            return await h.proc_stdin(job, data, close)
        async def cancel(job):
            return await h.proc_cancel(job)
        async def join(job, timeout_s):
            return await h.proc_join(job, timeout_s)
    class state:
        Slot = namedtuple('Slot', ['name', 'bytes', 'tainted'])
        async def get(name, default=None):
            return await h.state_get(name, default)
        async def put(name, value):
            return await h.state_put(name, value)
        async def list():
            return await h.state_list()
        async def drop(name):
            return await h.state_drop(name)
    class plan:
        Step = namedtuple('Step', ['text', 'status'], defaults=['pending'])
        async def update(items):
            return await h.plan_update(items)
    import asyncio
";

const VALUE_NAMES: &str = "asyncio, Ref, Omission, Image, File, Entry, Listing, Hit, Hits, ctx";

/// The host functions the `_sdk_host` module exposes, in a fixed order.
pub(super) fn host_functions(access: PythonHarnessSdkAccessV1) -> Vec<&'static str> {
    let mut names = vec!["ctx_read", "ctx_list", "ctx_grep"];
    if access == PythonHarnessSdkAccessV1::Full {
        names.push("fs_apply");
    }
    names.extend([
        "proc_run",
        "proc_shell",
        "proc_start",
        "proc_poll",
        "proc_stdin",
        "proc_cancel",
        "proc_join",
        "state_get",
        "state_put",
        "state_list",
        "state_drop",
        "plan_update",
    ]);
    names
}

/// Renders the bootstrap for one access ceiling and op order.
pub(super) fn render(access: PythonHarnessSdkAccessV1, order: PythonHarnessSdkOpOrderV1) -> String {
    let mut source = String::from(HEAD);
    let bound = if access == PythonHarnessSdkAccessV1::Full {
        source.push_str(FS_HEAD);
        match order {
            PythonHarnessSdkOpOrderV1::ReplaceFirst => {
                source.push_str(FS_REPLACE);
                source.push_str(FS_PATCH);
            }
            PythonHarnessSdkOpOrderV1::PatchFirst => {
                source.push_str(FS_PATCH);
                source.push_str(FS_REPLACE);
            }
        }
        source.push_str(FS_TAIL);
        format!("{VALUE_NAMES}, fs, proc, state, plan")
    } else {
        format!("{VALUE_NAMES}, proc, state, plan")
    };
    source.push_str(TAIL);
    let _ = write!(
        source,
        "    return {bound}\n{bound} = _plakous_sdk_install()\n_plakous_sdk_install = None\n"
    );
    source
}
