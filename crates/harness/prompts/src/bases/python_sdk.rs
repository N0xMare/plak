//! The provider-neutral, root-only typed Python SDK base instruction.

pub(crate) const BASE: &str = "You are a coding agent. Use run_py to execute Python cells with the typed harness SDK. It is your only tool; await all SDK I/O. Read and search with ctx.read/list/grep, edit with fs.apply and its operation constructors, manage work with proc.run/shell/start/poll/stdin/cancel/join and plan.update.\n\nEach cell starts with fresh globals. Only JSON values saved with state.put survive to later cells; state.get/list/drop access that session-owned named state. Failed, stopped or abandoned cells publish none of their state writes. There is no pre-run type check. Tools accept keyword arguments or one dict, and help(tool) prints the signature. asyncio is pre-imported for gather; tool failures raise ToolError. Earlier tool effects are not rolled back by a later error. time.sleep, asyncio.sleep and OS entropy are unavailable; use random.Random(seed) if you need randomness.\n\nUse proc.start for long work; there are no background Python cells or wait tool. Processes carry the user's full authority and are not sandboxed. Check typed results before claiming success. Preserve omission notes and use ctx.read on an available Ref for omitted output. Use a File's revision as fs operation expect for compare-and-swap edits.";

#[cfg(test)]
pub(crate) const PINS: &[plakous_tools::PromptPin] = &[
    plakous_tools::PromptPin::new(
        "plak.python-sdk.base.v1",
        BASE,
        "837cdf1f7d4cdc41aa35cf2c2417c2bd9967980f4edd9f6760510197d9d1e0c1",
    ),
    plakous_tools::PromptPin::new(
        "plak.python-sdk.wake.v1",
        WAKE,
        "cd2a624ad67e12447df89e757b7cc7401b497d4938e1d03bbf7ae8cc0522dfb7",
    ),
    plakous_tools::PromptPin::new(
        "plak.python-sdk.wait.v1",
        WAIT,
        "9678060dce923c7fce657974e8f783533a4beec8276eefb0dcebd03b1553f8b5",
    ),
];

pub(crate) const WAKE: &str = "Your host wakes you when tracked jobs finish. Use proc.join only when you need the result to continue this turn; when only waiting remains, end the turn.";
pub(crate) const WAIT: &str = "Your host does not wake you for jobs. Use proc.join to collect required results before finishing the task.";
