//! JSON ↔ Monty value conversion for py-ptc nested calls.

use std::{
    collections::HashSet,
    fmt::{self, Write as _},
    io,
};

use monty_types::{
    ExcType, MontyException, MontyObject, ObjectRef,
    unstable::{self, MontyNode, NodeId},
};
use serde_json::{Map, Value};

/// Bound on the compact JSON encoding of a nested-call argument, matching the
/// per-call argument limit. Monty values arrive as a shared-node arena, so a
/// small message can describe an exponentially larger tree; the conversion
/// charges every byte it would emit before building it and stops at this
/// budget instead of materializing the tree.
const MAX_ARGUMENT_BYTES: usize = 1_048_576;
const ARGUMENT_TOO_LARGE: &str = "Python tool argument exceeds 1 MiB";
/// Bound on value nesting in either direction. The arena wire has no depth
/// limit of its own, and both conversions recurse on the host's stack.
const MAX_VALUE_DEPTH: usize = 128;
/// The `CPython` default `sys.int_max_str_digits`. Decimal conversion of an `int`
/// is quadratic in its length, so the host never converts one past this:
/// `10**4300 < 2**14285`, so a value of at most this many bits has at most
/// 4300 digits.
const MAX_INT_DIGITS: u64 = 4300;
const MAX_INT_BITS: u64 = 14_284;

/// Converts the single positional argument of a Python tool call to JSON.
///
/// `list` and `tuple` become arrays, `dict` (whose keys must be `str`) an
/// object, an `int` beyond 64 bits its decimal string, and every other
/// non-JSON value its Python `repr()` string. The compact encoding of the
/// result is at most 1 MiB; an `int` past 4300 digits is refused, as `CPython`
/// refuses to convert it.
pub(crate) fn call_to_json(argument: ObjectRef<'_>) -> Result<Value, String> {
    let mut budget = MAX_ARGUMENT_BYTES;
    monty_to_json(argument, 0, &mut budget)
}

fn charge(budget: &mut usize, cost: usize) -> Result<(), String> {
    *budget = budget
        .checked_sub(cost)
        .ok_or_else(|| ARGUMENT_TOO_LARGE.to_owned())?;
    Ok(())
}

/// Counts the bytes `serde_json` writes without storing them.
struct EncodedLength(usize);

impl io::Write for EncodedLength {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 += bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// Writing into a counter cannot fail, and these values always serialize.
fn encoded_len(value: &Value) -> usize {
    let mut length = EncodedLength(0);
    let _ = serde_json::to_writer(&mut length, value);
    length.0
}

fn encoded_str_len(text: &str) -> usize {
    let mut length = EncodedLength(0);
    let _ = serde_json::to_writer(&mut length, text);
    length.0
}

fn charged_leaf(budget: &mut usize, json: Value) -> Result<Value, String> {
    charge(budget, encoded_len(&json))?;
    Ok(json)
}

fn int_limit_error() -> String {
    format!(
        "Python tool argument int exceeds the limit ({MAX_INT_DIGITS} digits) for integer string conversion"
    )
}

fn monty_to_json(value: ObjectRef<'_>, depth: usize, budget: &mut usize) -> Result<Value, String> {
    if depth > MAX_VALUE_DEPTH {
        return Err(format!(
            "Python tool argument nests deeper than {MAX_VALUE_DEPTH} levels"
        ));
    }
    match unstable::node(value) {
        MontyNode::None => charged_leaf(budget, Value::Null),
        MontyNode::Bool(flag) => charged_leaf(budget, Value::Bool(*flag)),
        MontyNode::Int(int) => charged_leaf(budget, Value::from(*int)),
        MontyNode::Float(float) => charged_leaf(budget, Value::from(*float)),
        MontyNode::BigInt(int) => {
            if int.bits() > MAX_INT_BITS {
                return Err(int_limit_error());
            }
            charged_leaf(budget, Value::String(int.to_string()))
        }
        MontyNode::String(text) => {
            charge(budget, encoded_str_len(text))?;
            Ok(Value::String(text.clone()))
        }
        MontyNode::List(ids) | MontyNode::Tuple(ids) => {
            charge(budget, 2 + ids.len().saturating_sub(1))?;
            ids.iter()
                .map(|id| monty_to_json(unstable::child(value, *id), depth + 1, budget))
                .collect::<Result<Vec<_>, _>>()
                .map(Value::Array)
        }
        MontyNode::Dict(pairs) => {
            charge(budget, 2 + pairs.len().saturating_sub(1))?;
            let mut map = Map::new();
            for (key, item) in pairs {
                let MontyNode::String(name) = unstable::node(unstable::child(value, *key)) else {
                    return Err("Python tool argument dict keys must be str".to_owned());
                };
                // The key, its quotes and the separating colon.
                charge(budget, encoded_str_len(name) + 1)?;
                let item = monty_to_json(unstable::child(value, *item), depth + 1, budget)?;
                map.insert(name.clone(), item);
            }
            Ok(Value::Object(map))
        }
        _ => {
            if oversized_int_bits(value).is_some() {
                return Err(int_limit_error());
            }
            // Not a `str` here, so `str()` is the value's `repr()`.
            let mut repr = Capped::new(*budget);
            if write!(repr, "{value}").is_err() {
                return Err(ARGUMENT_TOO_LARGE.to_owned());
            }
            charged_leaf(budget, Value::String(repr.text))
        }
    }
}

/// Bit length of the first `int` past [`MAX_INT_BITS`] reachable from `value`,
/// visiting each shared node once.
fn oversized_int_bits(value: ObjectRef<'_>) -> Option<u64> {
    let mut seen = HashSet::new();
    let mut pending = vec![value];
    while let Some(next) = pending.pop() {
        let node = unstable::node(next);
        if let MontyNode::BigInt(int) = node
            && int.bits() > MAX_INT_BITS
        {
            return Some(int.bits());
        }
        node.for_each_child(|id: NodeId| {
            if seen.insert(id) {
                pending.push(unstable::child(next, id));
            }
        });
    }
    None
}

pub(crate) fn json_to_monty(value: &Value) -> Result<MontyObject, String> {
    json_to_monty_within(value, 0)
}

fn json_to_monty_within(value: &Value, depth: usize) -> Result<MontyObject, String> {
    if depth > MAX_VALUE_DEPTH {
        return Err(format!(
            "JSON result nests deeper than {MAX_VALUE_DEPTH} levels"
        ));
    }
    match value {
        Value::Null => Ok(MontyObject::none()),
        Value::Bool(flag) => Ok(MontyObject::bool(*flag)),
        Value::Number(number) => {
            if let Some(int) = number.as_i64() {
                Ok(MontyObject::int(int))
            } else if let Some(uint) = number.as_u64() {
                Err(format!("unsupported JSON integer magnitude: {uint}"))
            } else if let Some(float) = number.as_f64() {
                if float.is_finite() {
                    Ok(MontyObject::float(float))
                } else {
                    Err(format!("unsupported JSON number: {number}"))
                }
            } else {
                Err(format!("unsupported JSON number: {number}"))
            }
        }
        Value::String(text) => Ok(MontyObject::string(text.clone())),
        Value::Array(items) => Ok(MontyObject::list(
            items
                .iter()
                .map(|item| json_to_monty_within(item, depth + 1))
                .collect::<Result<Vec<_>, _>>()?,
        )),
        Value::Object(map) => Ok(MontyObject::dict(
            map.iter()
                .map(|(key, value)| -> Result<_, String> {
                    Ok((
                        MontyObject::string(key.clone()),
                        json_to_monty_within(value, depth + 1)?,
                    ))
                })
                .collect::<Result<Vec<_>, _>>()?,
        )),
    }
}

pub(crate) fn nested_reply_to_resume(
    reply: super::NestedReply,
) -> Result<monty_pool::ResumeValue, String> {
    match reply {
        super::NestedReply::Ok(value) => {
            Ok(monty_pool::ResumeValue::Return(json_to_monty(&value)?))
        }
        super::NestedReply::Err(reason) => Ok(monty_pool::ResumeValue::Error(MontyException::new(
            ExcType::RuntimeError,
            Some(super::sanitize_nested_error(&reason)),
        ))),
    }
}

/// Renders a cell's final value as Python `str()` does: text as is, every
/// other value (including `complex`) its `repr()`. Shared sub-objects render
/// once per reference, so the rendering stops at `limit` bytes and says so.
/// An `int` past 4300 digits, whose decimal conversion is quadratic, renders
/// as `<int with N bits>` instead.
pub(crate) fn format_monty(value: &MontyObject, limit: usize) -> String {
    const NOTICE: &str = "\n[Python result truncated at 1 MiB]";
    let mut rendered = Capped::new(limit.saturating_sub(NOTICE.len()));
    match bounded_ints(value) {
        Some(Ok(bounded)) => {
            let _ = write!(rendered, "{bounded}");
        }
        Some(Err(())) => return "<value with an oversized int>".to_owned(),
        None => {
            let _ = write!(rendered, "{value}");
        }
    }
    if rendered.truncated {
        rendered.text.push_str(NOTICE);
    }
    rendered.text
}

/// Renders a value as Python `repr()` does, bounded like [`format_monty`]: a
/// top-level `str` is quoted, and every other value renders as its `str()`,
/// which is already its `repr()`.
pub(crate) fn repr_monty(value: &MontyObject, limit: usize) -> String {
    match unstable::node(value.as_ref()) {
        MontyNode::String(text) => {
            const NOTICE: &str = "\n[Python result truncated]";
            let mut rendered = Capped::new(limit.saturating_sub(NOTICE.len()));
            let _ = monty_types::string_repr_fmt(text, &mut rendered);
            if rendered.truncated {
                rendered.text.push_str(NOTICE);
            }
            rendered.text
        }
        _ => format_monty(value, limit),
    }
}

/// A copy of `value` whose oversized `int` nodes are replaced by verbatim
/// `<int with N bits>` text, or `None` when it holds none. The scan and copy
/// are linear in the arena, never in the expanded value.
fn bounded_ints(value: &MontyObject) -> Option<Result<MontyObject, ()>> {
    let oversized = |node: &MontyNode| match node {
        MontyNode::BigInt(int) if int.bits() > MAX_INT_BITS => Some(int.bits()),
        _ => None,
    };
    let (graph, root) = unstable::graph_parts(value);
    if !graph.nodes().iter().any(|node| oversized(node).is_some()) {
        return None;
    }
    let mut graph = graph.clone();
    for node in graph.nodes_mut() {
        if let Some(bits) = oversized(node) {
            // A `Cycle` leaf renders its text verbatim in `str()` and `repr()`.
            *node = MontyNode::Cycle(format!("<int with {bits} bits>"));
        }
    }
    Some(unstable::object_from_graph(graph, root).map_err(|_| ()))
}

/// A `fmt::Write` sink that keeps at most `limit` bytes and then fails, which
/// aborts the upstream renderer instead of letting it expand a shared graph.
struct Capped {
    text: String,
    limit: usize,
    truncated: bool,
}

impl Capped {
    fn new(limit: usize) -> Self {
        Self {
            text: String::new(),
            limit,
            truncated: false,
        }
    }
}

impl fmt::Write for Capped {
    fn write_str(&mut self, piece: &str) -> fmt::Result {
        let room = self.limit - self.text.len();
        if piece.len() <= room {
            self.text.push_str(piece);
            return Ok(());
        }
        let mut end = room;
        while !piece.is_char_boundary(end) {
            end -= 1;
        }
        self.text.push_str(&piece[..end]);
        self.truncated = true;
        Err(fmt::Error)
    }
}
