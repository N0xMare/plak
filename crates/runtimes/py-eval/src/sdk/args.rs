//! Host-side validation of one `_sdk_host` call. The type checker runs inside
//! the untrusted worker and proves nothing, so every argument is checked here
//! as if no checker had run; a bad one raises before any nested call.

use monty_types::{
    CallArgs, ExcType, MontyException, ObjectRef,
    unstable::{self, MontyNode},
};
use serde_json::{Value, json};

use super::values::state_json;

/// Most argv entries `process_session_exec` takes.
const MAX_ARGV: usize = 257;
/// Most lines one read page holds.
const MAX_READ_LINES: u64 = 2_000;
/// Deepest listing `list_entries` walks.
const MAX_LIST_DEPTH: i64 = 16;

/// What `ctx.read` names.
pub(super) enum ReadTarget {
    Path(String),
    Reference(String),
}

/// What `proc.start` runs.
pub(super) enum StartCommand {
    Argv(Vec<String>),
    Shell(String),
}

/// How `proc.run` and `proc.shell` start their process.
pub(super) struct RunSpec {
    pub(super) command: StartCommand,
    pub(super) cwd: Option<String>,
    pub(super) stdin: Option<String>,
    pub(super) timeout_ms: u64,
}

/// One validated SDK call.
pub(super) enum HostCall {
    Read {
        target: ReadTarget,
        lines: Option<(u64, u64)>,
    },
    List {
        path: String,
        glob: Option<String>,
        depth: i64,
    },
    Grep {
        pattern: String,
        path: String,
        glob: Option<String>,
    },
    Apply {
        ops: Vec<Value>,
    },
    Run(RunSpec),
    Start {
        command: StartCommand,
        cwd: Option<String>,
        tty: bool,
    },
    Poll {
        job: String,
    },
    Stdin {
        job: String,
        data: String,
        close: bool,
    },
    Cancel {
        job: String,
    },
    Join {
        job: String,
        timeout_ms: u64,
    },
    StateGet {
        name: String,
        default: Value,
    },
    StatePut {
        name: String,
        value: Value,
    },
    StateList,
    StateDrop {
        name: String,
    },
    PlanUpdate {
        steps: Vec<Value>,
    },
    /// An optional namespace's helper or fetch call: the canonical tool and
    /// its arguments, without the absent optional ones.
    Optional {
        tool: &'static str,
        input: Value,
    },
    /// `mcp.<server>.<tool>`: the canonical MCP tool and its arguments.
    Mcp {
        tool: String,
        input: Value,
    },
}

/// A refused argument, raised in the cell.
pub(super) struct ArgError(ExcType, String);

impl ArgError {
    fn type_error(message: impl Into<String>) -> Self {
        Self(ExcType::TypeError, message.into())
    }
    fn value_error(message: impl Into<String>) -> Self {
        Self(ExcType::ValueError, message.into())
    }
    pub(super) fn into_exception(self) -> MontyException {
        MontyException::new(self.0, Some(self.1))
    }
}

struct Args<'a> {
    function: &'a str,
    call: &'a CallArgs,
}

impl<'a> Args<'a> {
    fn arg(&self, index: usize, name: &str) -> Result<ObjectRef<'a>, ArgError> {
        self.call
            .arg(index)
            .ok_or_else(|| ArgError::type_error(format!("{} is missing {name}", self.function)))
    }

    fn wrong(&self, name: &str, expected: &str) -> ArgError {
        ArgError::type_error(format!("{} {name} must be {expected}", self.function))
    }

    fn string(&self, index: usize, name: &str) -> Result<String, ArgError> {
        let value = self.arg(index, name)?;
        value
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| self.wrong(name, "str"))
    }

    fn optional_string(&self, index: usize, name: &str) -> Result<Option<String>, ArgError> {
        let value = self.arg(index, name)?;
        match unstable::node(value) {
            MontyNode::None => Ok(None),
            MontyNode::String(text) => Ok(Some(text.clone())),
            _ => Err(self.wrong(name, "str or None")),
        }
    }

    fn boolean(&self, index: usize, name: &str) -> Result<bool, ArgError> {
        let value = self.arg(index, name)?;
        value.as_bool().ok_or_else(|| self.wrong(name, "bool"))
    }

    fn integer(&self, index: usize, name: &str) -> Result<i64, ArgError> {
        match unstable::node(self.arg(index, name)?) {
            MontyNode::Int(number) => Ok(*number),
            _ => Err(self.wrong(name, "int")),
        }
    }

    /// A positive number of seconds as whole milliseconds.
    fn timeout_ms(&self, index: usize, name: &str) -> Result<u64, ArgError> {
        let seconds = match unstable::node(self.arg(index, name)?) {
            #[allow(clippy::cast_precision_loss, reason = "timeouts are small")]
            MontyNode::Int(number) => *number as f64,
            MontyNode::Float(number) => *number,
            _ => return Err(self.wrong(name, "a number of seconds")),
        };
        if !seconds.is_finite() || seconds <= 0.0 || seconds > 86_400.0 {
            return Err(ArgError::value_error(format!(
                "{} {name} must be between 0 and 86400 seconds",
                self.function
            )));
        }
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "bounded positive seconds"
        )]
        Ok((seconds * 1000.0).ceil() as u64)
    }

    fn string_list(&self, index: usize, name: &str) -> Result<Vec<String>, ArgError> {
        let value = self.arg(index, name)?;
        let MontyNode::List(ids) = unstable::node(value) else {
            return Err(self.wrong(name, "list[str]"));
        };
        ids.iter()
            .map(|id| {
                unstable::child(value, *id)
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| self.wrong(name, "list[str]"))
            })
            .collect()
    }

    fn optional_string_list(
        &self,
        index: usize,
        name: &str,
    ) -> Result<Option<Vec<String>>, ArgError> {
        match unstable::node(self.arg(index, name)?) {
            MontyNode::None => Ok(None),
            _ => self
                .string_list(index, name)
                .map(Some)
                .map_err(|_| self.wrong(name, "list[str] or None")),
        }
    }

    fn job(&self) -> Result<String, ArgError> {
        let value = self.arg(0, "job")?;
        id_tuple(value, "Job").ok_or_else(|| self.wrong("job", "proc.Job"))
    }

    fn no_extra(&self, count: usize) -> Result<(), ArgError> {
        if self.call.args().len() == count && self.call.kwargs().len() == 0 {
            Ok(())
        } else {
            Err(ArgError::type_error(format!(
                "{} takes {count} arguments",
                self.function
            )))
        }
    }
}

/// The `id` of a named tuple of type `kind` with that single field.
fn id_tuple(value: ObjectRef<'_>, kind: &str) -> Option<String> {
    match unstable::node(value) {
        MontyNode::NamedTuple {
            type_name,
            field_names,
            values,
        } if type_name == kind && field_names == &["id"] => unstable::child(value, values[0])
            .as_str()
            .map(str::to_owned),
        _ => None,
    }
}

/// Validates one call of a bound host function.
pub(super) fn parse(function: &str, call: &CallArgs) -> Result<HostCall, ArgError> {
    let args = Args { function, call };
    let count = match function {
        "state_list" => 0,
        "fs_apply" | "proc_poll" | "proc_cancel" | "state_drop" | "plan_update"
        | "web_fetch_url" => 1,
        "ctx_read" | "proc_join" | "state_get" | "state_put" | "web_fetch" | "mcp_call" => 2,
        "ctx_list" | "ctx_grep" | "proc_stdin" | "proc_start" | "web_search" => 3,
        "proc_run" | "proc_shell" | "x_search" | "image_generate" => 4,
        _ => {
            return Err(ArgError::type_error(format!(
                "{function} is not an SDK function"
            )));
        }
    };
    args.no_extra(count)?;
    match function {
        "ctx_read" => parse_read(&args),
        "ctx_list" => parse_list(&args),
        "ctx_grep" => Ok(HostCall::Grep {
            pattern: args.string(0, "pattern")?,
            path: args.string(1, "path")?,
            glob: args.optional_string(2, "glob")?,
        }),
        "fs_apply" => parse_apply(&args),
        "proc_run" | "proc_shell" => parse_run(&args),
        "proc_start" => parse_start(&args),
        "proc_poll" => Ok(HostCall::Poll { job: args.job()? }),
        "proc_stdin" => Ok(HostCall::Stdin {
            job: args.job()?,
            data: args.string(1, "data")?,
            close: args.boolean(2, "close")?,
        }),
        "proc_cancel" => Ok(HostCall::Cancel { job: args.job()? }),
        "proc_join" => Ok(HostCall::Join {
            job: args.job()?,
            timeout_ms: args.timeout_ms(1, "timeout_s")?,
        }),
        "state_get" => Ok(HostCall::StateGet {
            name: args.string(0, "name")?,
            default: state_json(args.arg(1, "default")?).map_err(ArgError::value_error)?,
        }),
        "state_put" => Ok(HostCall::StatePut {
            name: args.string(0, "name")?,
            value: state_json(args.arg(1, "value")?).map_err(ArgError::value_error)?,
        }),
        "state_list" => Ok(HostCall::StateList),
        "state_drop" => Ok(HostCall::StateDrop {
            name: args.string(0, "name")?,
        }),
        "web_search" | "web_fetch" | "web_fetch_url" | "x_search" | "image_generate" => {
            parse_optional(&args)
        }
        "mcp_call" => parse_mcp(&args),
        _ => parse_plan(&args),
    }
}

/// Builds a JSON object from present fields only.
fn object(fields: Vec<(&str, Option<Value>)>) -> Value {
    Value::Object(
        fields
            .into_iter()
            .filter_map(|(name, value)| value.map(|value| (name.to_owned(), value)))
            .collect(),
    )
}

fn parse_optional(args: &Args<'_>) -> Result<HostCall, ArgError> {
    let list = |values: Option<Vec<String>>| values.map(|values| json!(values));
    let string = |value: Option<String>| value.map(Value::String);
    let (tool, input) = match args.function {
        "web_search" => (
            "web_search",
            object(vec![
                ("query", Some(json!(args.string(0, "query")?))),
                (
                    "allowed_domains",
                    list(args.optional_string_list(1, "allowed_domains")?),
                ),
                (
                    "blocked_domains",
                    list(args.optional_string_list(2, "blocked_domains")?),
                ),
            ]),
        ),
        "web_fetch" => (
            "web_fetch",
            json!({"url": args.string(0, "url")?, "prompt": args.string(1, "prompt")?}),
        ),
        "web_fetch_url" => ("fetch_url", json!({"url": args.string(0, "url")?})),
        "x_search" => (
            "x_search",
            object(vec![
                ("query", Some(json!(args.string(0, "query")?))),
                ("handles", list(args.optional_string_list(1, "handles")?)),
                ("from_date", string(args.optional_string(2, "from_date")?)),
                ("to_date", string(args.optional_string(3, "to_date")?)),
            ]),
        ),
        _ => (
            "image_gen",
            object(vec![
                ("prompt", Some(json!(args.string(0, "prompt")?))),
                ("path", Some(json!(args.string(1, "path")?))),
                ("size", string(args.optional_string(2, "size")?)),
                (
                    "edit_from",
                    list(args.optional_string_list(3, "edit_from")?),
                ),
            ]),
        ),
    };
    Ok(HostCall::Optional { tool, input })
}

/// `mcp_call(tool, arguments)`: only an MCP tool, with the arguments the
/// cell passed as strict JSON and its omitted (`None`) ones dropped.
fn parse_mcp(args: &Args<'_>) -> Result<HostCall, ArgError> {
    let tool = args.string(0, "tool")?;
    if !tool.starts_with("mcp__") {
        return Err(ArgError::value_error(format!("{tool} is not an MCP tool")));
    }
    let Value::Object(arguments) =
        state_json(args.arg(1, "arguments")?).map_err(ArgError::value_error)?
    else {
        return Err(args.wrong("arguments", "dict"));
    };
    let input = arguments
        .into_iter()
        .filter(|(_, value)| !value.is_null())
        .collect();
    Ok(HostCall::Mcp {
        tool,
        input: Value::Object(input),
    })
}

fn parse_read(args: &Args<'_>) -> Result<HostCall, ArgError> {
    let target = args.arg(0, "target")?;
    let target = if let Some(text) = target.as_str() {
        ReadTarget::Path(text.to_owned())
    } else {
        ReadTarget::Reference(
            id_tuple(target, "Ref").ok_or_else(|| args.wrong("target", "str or Ref"))?,
        )
    };
    let lines = args.arg(1, "lines")?;
    let lines = match unstable::node(lines) {
        MontyNode::None => None,
        MontyNode::Tuple(ids) if ids.len() == 2 => {
            let bound = |index: usize| match unstable::node(unstable::child(lines, ids[index])) {
                MontyNode::Int(number) => u64::try_from(*number).ok(),
                _ => None,
            };
            let (Some(first), Some(last)) = (bound(0), bound(1)) else {
                return Err(args.wrong("lines", "(first, last) ints"));
            };
            if first == 0 || last < first || last - first >= MAX_READ_LINES {
                return Err(ArgError::value_error(format!(
                    "ctx.read lines must be 1-based (first, last) with first <= last and at most {MAX_READ_LINES} lines"
                )));
            }
            Some((first, last))
        }
        _ => return Err(args.wrong("lines", "(first, last) or None")),
    };
    Ok(HostCall::Read { target, lines })
}

fn parse_list(args: &Args<'_>) -> Result<HostCall, ArgError> {
    let depth = args.integer(2, "depth")?;
    if !(1..=MAX_LIST_DEPTH).contains(&depth) {
        return Err(ArgError::value_error(format!(
            "ctx.list depth must be between 1 and {MAX_LIST_DEPTH}"
        )));
    }
    Ok(HostCall::List {
        path: args.string(0, "path")?,
        glob: args.optional_string(1, "glob")?,
        depth,
    })
}

fn parse_run(args: &Args<'_>) -> Result<HostCall, ArgError> {
    let command = if args.function == "proc_run" {
        StartCommand::Argv(argv(args, 0)?)
    } else {
        StartCommand::Shell(args.string(0, "cmd")?)
    };
    Ok(HostCall::Run(RunSpec {
        command,
        cwd: args.optional_string(1, "cwd")?,
        stdin: args.optional_string(2, "stdin")?,
        timeout_ms: args.timeout_ms(3, "timeout_s")?,
    }))
}

fn argv(args: &Args<'_>, index: usize) -> Result<Vec<String>, ArgError> {
    let list = args.string_list(index, "argv")?;
    if list.is_empty() || list.len() > MAX_ARGV {
        return Err(ArgError::value_error(format!(
            "argv must hold 1 to {MAX_ARGV} strings"
        )));
    }
    Ok(list)
}

fn parse_start(args: &Args<'_>) -> Result<HostCall, ArgError> {
    let command = match args.arg(0, "cmd")?.as_str() {
        Some(text) => StartCommand::Shell(text.to_owned()),
        None => {
            StartCommand::Argv(argv(args, 0).map_err(|_| args.wrong("cmd", "str or list[str]"))?)
        }
    };
    Ok(HostCall::Start {
        command,
        cwd: args.optional_string(1, "cwd")?,
        tty: args.boolean(2, "tty")?,
    })
}

/// One field of an op named tuple.
fn field<'a>(
    op: ObjectRef<'a>,
    names: &[String],
    values: &[unstable::NodeId],
    name: &str,
) -> Option<ObjectRef<'a>> {
    names
        .iter()
        .position(|field| field == name)
        .map(|index| unstable::child(op, values[index]))
}

fn parse_apply(args: &Args<'_>) -> Result<HostCall, ArgError> {
    let list = args.arg(0, "ops")?;
    let MontyNode::List(ids) = unstable::node(list) else {
        return Err(args.wrong("ops", "fs ops"));
    };
    if ids.is_empty() {
        return Err(ArgError::value_error("fs.apply needs at least one op"));
    }
    ids.iter()
        .enumerate()
        .map(|(index, id)| op_json(unstable::child(list, *id), index + 1))
        .collect::<Result<Vec<_>, _>>()
        .map(|ops| HostCall::Apply { ops })
}

fn op_json(op: ObjectRef<'_>, position: usize) -> Result<Value, ArgError> {
    let invalid = |detail: &str| ArgError::type_error(format!("fs.apply op {position}: {detail}"));
    let MontyNode::NamedTuple {
        type_name,
        field_names,
        values,
    } = unstable::node(op)
    else {
        return Err(invalid(
            "must be fs.Replace, fs.Patch, fs.Write, fs.Delete or fs.Move",
        ));
    };
    let fields: &[&str] = match type_name.as_str() {
        "Patch" => &["text"],
        "Replace" => &["path", "old", "new", "count", "expect"],
        "Write" => &["path", "content", "expect"],
        "Delete" => &["path", "expect"],
        "Move" => &["src", "dst", "expect"],
        _ => {
            return Err(invalid(
                "must be fs.Replace, fs.Patch, fs.Write, fs.Delete or fs.Move",
            ));
        }
    };
    if field_names
        .iter()
        .map(String::as_str)
        .ne(fields.iter().copied())
    {
        return Err(invalid("has the wrong fields"));
    }
    let mut object = serde_json::Map::new();
    object.insert("op".into(), json!(type_name.to_ascii_lowercase()));
    for name in fields {
        let value = field(op, field_names, values, name).ok_or_else(|| invalid("is incomplete"))?;
        let encoded = match (*name, unstable::node(value)) {
            ("expect", MontyNode::None) => continue,
            ("count", MontyNode::Int(count)) if *count >= 1 => json!(count),
            ("count", _) => {
                return Err(ArgError::value_error(format!(
                    "fs.apply op {position}: count must be an int >= 1"
                )));
            }
            (_, MontyNode::String(text)) => json!(text),
            _ => return Err(invalid(&format!("{name} must be str"))),
        };
        object.insert((*name).to_owned(), encoded);
    }
    Ok(Value::Object(object))
}

fn parse_plan(args: &Args<'_>) -> Result<HostCall, ArgError> {
    let list = args.arg(0, "items")?;
    let MontyNode::List(ids) = unstable::node(list) else {
        return Err(args.wrong("items", "list[plan.Step]"));
    };
    let steps = ids
        .iter()
        .map(|id| {
            let step = unstable::child(list, *id);
            let MontyNode::NamedTuple {
                type_name,
                field_names,
                values,
            } = unstable::node(step)
            else {
                return Err(args.wrong("items", "list[plan.Step]"));
            };
            if type_name != "Step" || field_names != &["text", "status"] {
                return Err(args.wrong("items", "list[plan.Step]"));
            }
            let text = unstable::child(step, values[0]).as_str().map(str::to_owned);
            let status = unstable::child(step, values[1]).as_str().map(str::to_owned);
            match (text, status.as_deref()) {
                (Some(text), Some(status @ ("pending" | "in_progress" | "completed"))) => {
                    Ok(json!({"step": text, "status": status}))
                }
                (Some(_), _) => Err(ArgError::value_error(
                    "plan.Step status must be pending, in_progress or completed",
                )),
                _ => Err(args.wrong("items", "list[plan.Step]")),
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(HostCall::PlanUpdate { steps })
}
