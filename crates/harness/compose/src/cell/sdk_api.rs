//! One catalog for SDK Python declarations, wrappers and argument validation.

use serde::Serialize;
use serde_json::Value;
use std::fmt::Write as _;

use super::render_py::{PythonBootstrap, PythonBootstrapBinding, PythonInput};

#[derive(Clone, Debug, Serialize)]
pub(super) struct SdkApi {
    records: Vec<Record>,
    functions: Vec<Function>,
    constructors: Vec<Constructor>,
}

#[derive(Clone, Debug, Serialize)]
struct Constructor {
    name: &'static str,
    operation: &'static str,
    parameters: Vec<Parameter>,
    wire: Vec<&'static str>,
}

#[derive(Clone, Debug, Serialize)]
struct Record {
    name: &'static str,
    fields: Vec<(&'static str, &'static str, Option<&'static str>)>,
}

#[derive(Clone, Debug, Serialize)]
struct Function {
    namespace: &'static str,
    name: &'static str,
    parameters: Vec<Parameter>,
    returns: &'static str,
    description: &'static str,
}

#[derive(Clone, Debug, Serialize)]
struct Parameter {
    name: &'static str,
    annotation: &'static str,
    default: Option<&'static str>,
    shape: Shape,
    expression: &'static str,
}

#[derive(Clone, Debug, Serialize)]
enum Shape {
    Text,
    ReadTarget,
    Bool,
    Integer,
    Number,
    Json,
    Array,
    Strings,
    Command,
    Optional(Box<Self>),
}

impl Shape {
    fn accepts(&self, value: &Value) -> bool {
        match self {
            Self::Text => value.is_string(),
            Self::ReadTarget => {
                value.is_string()
                    || value.as_object().is_some_and(|fields| {
                        fields.len() == 1 && fields.get("ref").is_some_and(Value::is_string)
                    })
            }
            Self::Bool => value.is_boolean(),
            Self::Integer => value.as_i64().is_some(),
            Self::Number => value.as_f64().is_some_and(f64::is_finite),
            Self::Json => true,
            Self::Array => value.is_array(),
            Self::Strings => value
                .as_array()
                .is_some_and(|items| items.iter().all(Value::is_string)),
            Self::Command => value.is_string() || Self::Strings.accepts(value),
            Self::Optional(inner) => value.is_null() || inner.accepts(value),
        }
    }
}

fn p(name: &'static str, annotation: &'static str, shape: Shape) -> Parameter {
    Parameter {
        name,
        annotation,
        default: None,
        shape,
        expression: name,
    }
}

impl Parameter {
    fn default(mut self, value: &'static str) -> Self {
        self.default = Some(value);
        self
    }
    fn encode(mut self, value: &'static str) -> Self {
        self.expression = value;
        self
    }
}

impl SdkApi {
    pub(super) fn core() -> Self {
        Self {
            records: Self::records(),
            functions: [
                Self::context_functions(),
                Self::process_functions(),
                Self::state_functions(),
            ]
            .into_iter()
            .flatten()
            .collect(),
            constructors: Self::constructors(),
        }
    }

    fn records() -> Vec<Record> {
        [
            ("Ref", vec![("id", "str", None)]),
            (
                "Omission",
                vec![
                    ("what", "str", None),
                    ("count", "int", None),
                    ("bytes", "int", None),
                    ("full", "Ref | None", None),
                ],
            ),
            (
                "Image",
                vec![
                    ("id", "str", None),
                    ("path", "str", None),
                    ("media_type", "str", None),
                    ("bytes", "int", None),
                    ("width", "int | None", None),
                    ("height", "int | None", None),
                ],
            ),
            (
                "File",
                vec![
                    ("path", "str", None),
                    ("text", "str", None),
                    ("first_line", "int", None),
                    ("lines", "int", None),
                    ("eof", "bool", None),
                    ("revision", "str", None),
                    ("image", "Image | None", None),
                    ("omitted", "list[Omission]", None),
                ],
            ),
            (
                "Entry",
                vec![
                    ("path", "str", None),
                    ("kind", "str", None),
                    ("size", "int", None),
                ],
            ),
            (
                "Listing",
                vec![
                    ("entries", "list[Entry]", None),
                    ("omitted", "list[Omission]", None),
                ],
            ),
            (
                "Hit",
                vec![
                    ("path", "str", None),
                    ("line", "int", None),
                    ("text", "str", None),
                ],
            ),
            (
                "Hits",
                vec![
                    ("hits", "list[Hit]", None),
                    ("omitted", "list[Omission]", None),
                ],
            ),
            ("Job", vec![("id", "str", None)]),
            (
                "ProcResult",
                vec![
                    ("job", "Job | None", None),
                    ("status", "str", None),
                    ("exit_code", "int | None", None),
                    ("output", "str", None),
                    ("omitted", "list[Omission]", None),
                ],
            ),
            ("Slot", vec![("name", "str", None), ("bytes", "int", None)]),
            (
                "Step",
                vec![("text", "str", None), ("status", "str", Some("'pending'"))],
            ),
        ]
        .into_iter()
        .map(|(name, fields)| Record { name, fields })
        .collect()
    }

    fn context_functions() -> Vec<Function> {
        use Shape::{Array, Integer, Optional, ReadTarget, Text};
        let path = || p("path", "str", Text).default("'.'");
        let glob = || p("glob", "str | None", Optional(Box::new(Text))).default("None");
        vec![
            Function {
                namespace: "ctx",
                name: "read",
                parameters: vec![
                    p("target", "str | Ref", ReadTarget)
                        .encode("target if isinstance(target, str) else {'ref': target.id}"),
                    p("lines", "tuple[int, int] | None", Optional(Box::new(Array))).default("None"),
                ],
                returns: "File",
                description: "Read a file or output Ref. lines is an inclusive 1-based range; revision guards edits.",
            },
            Function {
                namespace: "ctx",
                name: "list",
                parameters: vec![path(), glob(), p("depth", "int", Integer).default("1")],
                returns: "Listing",
                description: "List repository entries with workspace-root ignore rules.",
            },
            Function {
                namespace: "ctx",
                name: "grep",
                parameters: vec![p("pattern", "str", Text), path(), glob()],
                returns: "Hits",
                description: "Search repository content using a regular expression.",
            },
            Function {
                namespace: "fs",
                name: "apply",
                parameters: vec![p("ops", "list[dict[str, Any]]", Array)],
                returns: "list[dict[str, Any]]",
                description: "Validate all ordered operations, then publish one edit_files transaction. Use the fs op constructors.",
            },
        ]
    }

    fn process_functions() -> Vec<Function> {
        use Shape::{Bool, Command, Number, Optional, Strings, Text};
        let cwd = || p("cwd", "str | None", Optional(Box::new(Text))).default("None");
        let stdin = || p("stdin", "str | None", Optional(Box::new(Text))).default("None");
        let timeout = || p("timeout_s", "float", Number).default("120");
        let job = || p("job", "Job", Text).encode("job.id");
        vec![
            Function {
                namespace: "proc",
                name: "run",
                parameters: vec![p("argv", "list[str]", Strings), cwd(), stdin(), timeout()],
                returns: "ProcResult",
                description: "Run literal argv and await completion, bounded by timeout_s.",
            },
            Function {
                namespace: "proc",
                name: "shell",
                parameters: vec![p("cmd", "str", Text), cwd(), stdin(), timeout()],
                returns: "ProcResult",
                description: "Run a /bin/sh command and await completion. Processes carry full user authority.",
            },
            Function {
                namespace: "proc",
                name: "start",
                parameters: vec![
                    p("cmd", "str | list[str]", Command),
                    cwd(),
                    p("tty", "bool", Bool).default("False"),
                ],
                returns: "Job",
                description: "Start a long-running job. A list is literal argv; a string is a shell command.",
            },
            Function {
                namespace: "proc",
                name: "poll",
                parameters: vec![job()],
                returns: "ProcResult",
                description: "Observe status and unread output without waiting.",
            },
            Function {
                namespace: "proc",
                name: "stdin",
                parameters: vec![
                    job(),
                    p("data", "str", Text),
                    p("close", "bool", Bool).default("False"),
                ],
                returns: "None",
                description: "Write job stdin; close=True closes the input pipe.",
            },
            Function {
                namespace: "proc",
                name: "cancel",
                parameters: vec![job()],
                returns: "ProcResult",
                description: "Terminate and join a job.",
            },
            Function {
                namespace: "proc",
                name: "join",
                parameters: vec![job(), p("timeout_s", "float", Number)],
                returns: "ProcResult",
                description: "Wait up to timeout_s for a job's terminal state.",
            },
        ]
    }

    fn state_functions() -> Vec<Function> {
        use Shape::{Array, Json, Text};
        vec![
            Function {
                namespace: "state",
                name: "get",
                parameters: vec![
                    p("name", "str", Text),
                    p("default", "Any", Json).default("None"),
                ],
                returns: "Any",
                description: "Read named session state, or default when absent. Globals do not survive a cell.",
            },
            Function {
                namespace: "state",
                name: "put",
                parameters: vec![p("name", "str", Text), p("value", "Any", Json)],
                returns: "None",
                description: "Stage JSON state. Writes publish only with a successful presented cell result.",
            },
            Function {
                namespace: "state",
                name: "list",
                parameters: vec![],
                returns: "list[Slot]",
                description: "List sorted state keys and canonical JSON sizes.",
            },
            Function {
                namespace: "state",
                name: "drop",
                parameters: vec![p("name", "str", Text)],
                returns: "bool",
                description: "Stage deletion; return whether the key existed.",
            },
            Function {
                namespace: "plan",
                name: "update",
                parameters: vec![
                    p("items", "list[Step]", Array).encode("[item._asdict() for item in items]"),
                ],
                returns: "int",
                description: "Replace the plan and return its revision.",
            },
        ]
    }

    fn constructors() -> Vec<Constructor> {
        use Shape::{Bool, Optional, Text};
        let expect = || p("expect", "str | None", Optional(Box::new(Text))).default("None");
        vec![
            Constructor {
                name: "Patch",
                operation: "patch",
                parameters: vec![p("text", "str", Text), expect()],
                wire: vec!["patch", "expect"],
            },
            Constructor {
                name: "Replace",
                operation: "replace",
                parameters: vec![
                    p("path", "str", Text),
                    p("old", "str", Text),
                    p("new", "str", Text),
                    p("all", "bool", Bool).default("False"),
                    expect(),
                ],
                wire: vec!["path", "old", "new", "all", "expect"],
            },
            Constructor {
                name: "Write",
                operation: "write",
                parameters: vec![p("path", "str", Text), p("content", "str", Text), expect()],
                wire: vec!["path", "content", "expect"],
            },
            Constructor {
                name: "Delete",
                operation: "delete",
                parameters: vec![p("path", "str", Text), expect()],
                wire: vec!["path", "expect"],
            },
            Constructor {
                name: "Move",
                operation: "move",
                parameters: vec![p("path", "str", Text), p("to", "str", Text), expect()],
                wire: vec!["path", "to", "expect"],
            },
        ]
    }

    pub(super) fn validate(&self, name: &str, input: &Value) -> Result<(), String> {
        let function = self
            .functions
            .iter()
            .find(|f| format!("{}.{}", f.namespace, f.name) == name)
            .ok_or_else(|| "unknown SDK function".to_owned())?;
        let args = input
            .as_object()
            .ok_or_else(|| "SDK arguments must be an object".to_owned())?;
        if args.len() != function.parameters.len()
            || function
                .parameters
                .iter()
                .any(|p| !args.get(p.name).is_some_and(|v| p.shape.accepts(v)))
        {
            return Err(format!("invalid arguments for {name}"));
        }
        Ok(())
    }

    pub(super) fn stubs(&self) -> String {
        let mut text =
            "import asyncio as asyncio\nfrom typing import Any, NamedTuple\nimport builtins\n\n"
                .to_owned();
        for record in &self.records {
            let _ = writeln!(text, "class {}(NamedTuple):", record.name);
            for (name, ty, default) in &record.fields {
                let _ = writeln!(
                    text,
                    "    {name}: {ty}{}",
                    default.map_or_else(String::new, |v| format!(" = {v}"))
                );
            }
        }
        for namespace in ["ctx", "fs", "proc", "state", "plan"] {
            let _ = writeln!(text, "\nclass {namespace}:");
            if namespace == "fs" {
                for constructor in &self.constructors {
                    let _ = writeln!(
                        text,
                        "    def {}({}) -> dict[str, Any]: ...",
                        constructor.name,
                        signature(&constructor.parameters, true)
                    );
                }
            }
            if namespace == "plan" {
                text.push_str("    Step = Step\n");
            }
            for function in self.functions.iter().filter(|f| f.namespace == namespace) {
                Self::stub_function(&mut text, function);
            }
        }
        text.replace("list[", "builtins.list[")
    }

    fn stub_function(text: &mut String, function: &Function) {
        // These are unbound namespace functions, matching the bootstrap. The
        // sandbox's typeshed has no staticmethod type; that decorator would
        // erase the signature to Any instead of checking SDK calls.
        let parameters = function
            .parameters
            .iter()
            .map(|p| {
                format!(
                    "{}: {}{}",
                    p.name,
                    p.annotation,
                    p.default.map_or_else(String::new, |v| format!(" = {v}"))
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(
            text,
            "    async def {}({parameters}) -> {}:\n        \"\"\"{}\"\"\"\n        ...",
            function.name, function.returns, function.description
        );
    }

    pub(super) fn bootstrap(&self) -> PythonBootstrap {
        let mut source = "from collections import namedtuple\nimport asyncio\n".to_owned();
        for record in &self.records {
            let fields = record
                .fields
                .iter()
                .map(|(name, _, _)| format!("'{name}'"))
                .collect::<Vec<_>>()
                .join(", ");
            let defaults = record
                .fields
                .iter()
                .filter_map(|(_, _, default)| *default)
                .collect::<Vec<_>>()
                .join(", ");
            let _ = writeln!(
                source,
                "{} = namedtuple('{}', [{fields}], defaults=[{defaults}])",
                record.name, record.name
            );
        }
        source.push_str(UNWRAP);
        let mut bindings = Vec::new();
        for namespace in ["ctx", "fs", "proc", "state", "plan"] {
            let _ = writeln!(source, "class {namespace}:");
            if namespace == "fs" {
                for constructor in &self.constructors {
                    let arguments = constructor
                        .parameters
                        .iter()
                        .zip(&constructor.wire)
                        .filter(|(p, _)| p.name != "expect")
                        .map(|(p, wire)| format!("'{wire}': {}", p.name))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let _ = writeln!(
                        source,
                        "    def {}({}):\n        fields = {{'op': '{}', {arguments}}}\n        if expect is not None:\n            fields['expect'] = expect\n        return fields",
                        constructor.name,
                        signature(&constructor.parameters, false),
                        constructor.operation
                    );
                }
            }
            if namespace == "plan" {
                source.push_str("    Step = Step\n");
            }
            self.bootstrap_functions(namespace, &mut source, &mut bindings);
        }
        PythonBootstrap { source, bindings }
    }

    fn bootstrap_functions(
        &self,
        namespace: &str,
        source: &mut String,
        bindings: &mut Vec<PythonBootstrapBinding>,
    ) {
        for function in self.functions.iter().filter(|f| f.namespace == namespace) {
            let parameters = function
                .parameters
                .iter()
                .map(|p| {
                    format!(
                        "{}{}",
                        p.name,
                        p.default.map_or_else(String::new, |v| format!("={v}"))
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            let arguments = function
                .parameters
                .iter()
                .map(|p| format!("'{}': {}", p.name, p.expression))
                .collect::<Vec<_>>()
                .join(", ");
            let private = format!("_sdk_host_{}", bindings.len());
            let prefix = "    async def";
            let name = function.name;
            let indent = "        ";
            let _ = writeln!(
                source,
                "{prefix} {name}({parameters}):\n{indent}return _sdk_unwrap(await {private}({{{arguments}}}))"
            );
            bindings.push(PythonBootstrapBinding {
                function: private,
                tool: format!("{}.{}", function.namespace, function.name),
                input: PythonInput::Dictionary,
            });
        }
    }
}

fn signature(parameters: &[Parameter], typed: bool) -> String {
    parameters
        .iter()
        .map(|p| {
            format!(
                "{}{}{}",
                p.name,
                if typed {
                    format!(": {}", p.annotation)
                } else {
                    String::new()
                },
                p.default
                    .map_or_else(String::new, |value| format!(" = {value}"))
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}
const UNWRAP: &str = r"
def _sdk_unwrap(value):
    if isinstance(value, tuple) and hasattr(value, 'kind') and hasattr(value, 'message'):
        if value.kind == 'value':
            raise ValueError(value.message)
        if value.kind == 'quota':
            raise OverflowError(value.message)
        raise RuntimeError(value.message)
    return value
";

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn example(shape: &Shape) -> Value {
        match shape {
            Shape::Text | Shape::ReadTarget => json!("text"),
            Shape::Bool => json!(false),
            Shape::Integer => json!(1),
            Shape::Number => json!(1.5),
            Shape::Json | Shape::Optional(_) => Value::Null,
            Shape::Array | Shape::Strings => json!([]),
            Shape::Command => json!(["echo"]),
        }
    }
    #[test]
    fn every_sdk_stub_and_wrapper_uses_the_validator_keyword_set() {
        let api = SdkApi::core();
        let bootstrap = api.bootstrap();
        let stubs = api.stubs();
        assert_eq!(bootstrap.bindings.len(), api.functions.len());
        for function in &api.functions {
            let name = format!("{}.{}", function.namespace, function.name);
            let args = function
                .parameters
                .iter()
                .map(|p| (p.name.into(), example(&p.shape)))
                .collect::<serde_json::Map<_, _>>();
            assert!(
                api.validate(&name, &Value::Object(args.clone())).is_ok(),
                "{name}"
            );
            let mut extra = args.clone();
            extra.insert("unadvertised".into(), Value::Null);
            assert!(
                api.validate(&name, &Value::Object(extra)).is_err(),
                "{name}"
            );
            for parameter in &function.parameters {
                let mut missing = args.clone();
                missing.remove(parameter.name);
                assert!(
                    api.validate(&name, &Value::Object(missing)).is_err(),
                    "{name}: {}",
                    parameter.name
                );
            }
            assert!(stubs.contains(&format!("async def {}(", function.name)));
            let binding = bootstrap
                .bindings
                .iter()
                .find(|binding| binding.tool == name);
            assert!(binding.is_some(), "{name}");
        }
        assert!(api.validate("web.ask", &json!({})).is_err());
        assert!(
            api.validate("ctx.read", &json!({"target":{"ref":"ref_id"},"lines":null}))
                .is_ok()
        );
        assert!(
            api.validate(
                "ctx.read",
                &json!({"target":{"ref":"ref_id","path":"fallback"},"lines":null})
            )
            .is_err()
        );
    }
    #[test]
    fn sdk_generated_text_goldens() -> Result<(), Box<dyn std::error::Error>> {
        let api = SdkApi::core();
        let digest = |text: &str| {
            plakous_model::Digest::sha256(
                plakous_model::DigestDomain::ToolDefinition,
                1,
                &[text.as_bytes()],
            )
            .map(|d| d.to_string())
        };
        assert_eq!(
            vec![digest(&api.stubs())?, digest(&api.bootstrap().source)?],
            vec![
                "228e933e6cca14d0611df2c39702cb57b64188cb3b23cc5c2d093c8c96923692",
                "ff59fc459f24eca8c830bada9c49302c3c4da08e2cef16813cf34115be0f9446"
            ]
        );
        Ok(())
    }
}
