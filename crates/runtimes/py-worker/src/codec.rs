//! JSON and Monty value conversion for nested calls, and final-value rendering.

use std::{
    collections::HashSet,
    fmt::{self, Write as _},
    io,
};

use monty_types::{
    ExcType, ExtFunctionResult, MontyException, MontyObject, ObjectRef,
    unstable::{self, MontyNode, NodeId},
};
use plakous_cell::CellValue;
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

/// The value or failure a nested call resumes the cell with. A failure raises
/// `RuntimeError` with the host's diagnostic, bounded to 16 KiB.
pub(crate) fn nested_outcome_to_result(
    outcome: Result<CellValue, String>,
) -> Result<ExtFunctionResult, String> {
    match outcome {
        Ok(value) => Ok(ExtFunctionResult::Return(cell_to_monty(value, 0)?)),
        Err(reason) => Ok(ExtFunctionResult::Error(MontyException::new(
            ExcType::RuntimeError,
            Some(sanitize_nested_error(&reason)),
        ))),
    }
}

fn cell_to_monty(value: CellValue, depth: usize) -> Result<MontyObject, String> {
    if depth > MAX_VALUE_DEPTH {
        return Err(format!(
            "cell result nests deeper than {MAX_VALUE_DEPTH} levels"
        ));
    }
    match value {
        CellValue::Bytes(bytes) => Ok(MontyObject::bytes(bytes)),
        CellValue::Record { name, fields } => {
            let (names, values): (Vec<_>, Vec<_>) = fields.into_iter().unzip();
            let values = values
                .into_iter()
                .map(|value| cell_to_monty(value, depth + 1))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(MontyObject::named_tuple(name, names, values))
        }
        CellValue::List(values) => Ok(MontyObject::list(
            values
                .into_iter()
                .map(|value| cell_to_monty(value, depth + 1))
                .collect::<Result<Vec<_>, _>>()?,
        )),
        CellValue::Dict(fields) => Ok(MontyObject::dict(
            fields
                .into_iter()
                .map(|(key, value)| {
                    Ok((MontyObject::string(key), cell_to_monty(value, depth + 1)?))
                })
                .collect::<Result<Vec<_>, String>>()?,
        )),
        scalar => json_to_monty(&Value::try_from(scalar)?),
    }
}

fn sanitize_nested_error(message: &str) -> String {
    if message.len() <= 16 * 1024 {
        return message.to_owned();
    }
    let mut end = 16 * 1024;
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[diagnostic truncated]", &message[..end])
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

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, reason = "codec bound regressions")]
mod tests {
    use std::time::Duration;

    fn within<T: Send + 'static>(limit: Duration, work: impl FnOnce() -> T + Send + 'static) -> T {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(work());
        });
        rx.recv_timeout(limit)
            .unwrap_or_else(|_| panic!("host-side conversion ran past {limit:?}"))
    }

    #[test]
    fn unsupported_json_number_is_refused() {
        let value = serde_json::Value::Number(serde_json::Number::from(i64::MAX as u64 + 1));
        let err =
            super::json_to_monty(&value).expect_err("integers above i64::MAX are not monty-safe");
        assert!(err.contains("unsupported JSON integer magnitude"), "{err}");
    }

    #[test]
    fn nested_results_are_bounded_in_depth() {
        // Monty 1.x carries values as a flat node arena with no depth limit of
        // its own, so the worker bounds nesting where it converts JSON.
        let nested = |depth| {
            (0..depth).fold(serde_json::Value::Null, |value, _| {
                serde_json::json!([value])
            })
        };
        super::json_to_monty(&nested(128)).expect("JSON result at the bound");
        let error = super::json_to_monty(&nested(129)).expect_err("one level past the bound");
        assert!(error.contains("nests deeper than 128 levels"), "{error}");
    }

    #[test]
    fn shared_dicts_cannot_make_argument_conversion_walk_unbounded_pairs() {
        use monty_types::unstable::{MontyGraph, MontyNode, NodeId, object_from_graph};
        // One arena of ~2 MB: 50k references to one dict of 100k keys would be
        // 5e9 pairs if every reference were walked.
        for string_keys in [false, true] {
            let mut graph = MontyGraph::new();
            let zero = graph.push(MontyNode::Int(0));
            let pairs: Vec<(NodeId, NodeId)> = (0..100_000)
                .map(|key| {
                    let key = if string_keys {
                        MontyNode::String(key.to_string())
                    } else {
                        MontyNode::Int(key)
                    };
                    (graph.push(key), zero)
                })
                .collect();
            let shared = graph.push(MontyNode::Dict(pairs));
            let references = graph.push(MontyNode::List(vec![shared; 50_000]));
            let name = graph.push(MontyNode::String("x".into()));
            let root = graph.push(MontyNode::Dict(vec![(name, references)]));
            let argument = object_from_graph(graph, root).expect("valid graph");
            let outcome = within(Duration::from_secs(10), move || {
                super::call_to_json(argument.as_ref()).map(|json| json.to_string().len())
            });
            let expected = if string_keys {
                "Python tool argument exceeds 1 MiB"
            } else {
                "Python tool argument dict keys must be str"
            };
            assert_eq!(outcome, Err(expected.to_owned()));
        }
    }

    #[test]
    fn the_argument_budget_is_the_compact_json_byte_count() {
        use monty_types::MontyObject;
        let exceeded = Err("Python tool argument exceeds 1 MiB".to_owned());
        let ints = MontyObject::list((0..1_000_000).map(|_| MontyObject::int(i64::MIN)));
        assert_eq!(
            super::call_to_json(ints.as_ref()).map(|json| json.to_string().len()),
            exceeded
        );
        // A string's encoding is its text plus two quotes; a dict adds braces,
        // the quoted key and a colon.
        let fits = |text_bytes: usize| {
            let argument = MontyObject::dict([(
                MontyObject::string("k"),
                MontyObject::string("x".repeat(text_bytes)),
            )]);
            super::call_to_json(argument.as_ref()).map(|json| json.to_string().len())
        };
        let overhead = r#"{"k":""}"#.len();
        assert_eq!(fits(1_048_576 - overhead), Ok(1_048_576));
        assert_eq!(fits(1_048_576 - overhead + 1), exceeded);
        let escaped = MontyObject::string("\n".repeat(600_000));
        assert_eq!(
            super::call_to_json(escaped.as_ref()).map(|json| json.to_string().len()),
            exceeded,
            "escapes count as the bytes they encode to"
        );
    }

    #[test]
    fn argument_ints_past_4300_digits_are_refused_like_cpython() {
        use monty_types::unstable::{MontyNode, object_from_node};
        let largest = (num_bigint::BigInt::from(1) << 14_284_u32) - num_bigint::BigInt::from(1);
        let digits = largest.to_string();
        let accepted = object_from_node(MontyNode::BigInt(largest));
        assert_eq!(
            super::call_to_json(accepted.as_ref()),
            Ok(serde_json::Value::String(digits))
        );
        let refused = object_from_node(MontyNode::BigInt(
            num_bigint::BigInt::from(1) << 64_000_000_u32,
        ));
        let outcome = within(Duration::from_secs(10), move || {
            super::call_to_json(refused.as_ref())
        });
        assert_eq!(
            outcome,
            Err(
                "Python tool argument int exceeds the limit (4300 digits) for integer string conversion"
                    .to_owned()
            )
        );
    }

    #[test]
    fn the_host_renders_a_large_int_value_in_bounded_time() {
        use monty_types::unstable::{MontyGraph, MontyNode, object_from_graph, object_from_node};
        for bits in [8_000_000_u32, 64_000_000] {
            let value = object_from_node(MontyNode::BigInt(num_bigint::BigInt::from(1) << bits));
            let rendered = within(Duration::from_secs(10), move || {
                super::format_monty(&value, 1_048_576 - 64)
            });
            assert_eq!(rendered, format!("<int with {} bits>", bits + 1));
        }
        let mut graph = MontyGraph::new();
        let big = graph.push(MontyNode::BigInt(num_bigint::BigInt::from(1) << 20_000_u32));
        let small = graph.push(MontyNode::Int(7));
        let root = graph.push(MontyNode::List(vec![big, small, big]));
        let nested = object_from_graph(graph, root).expect("valid graph");
        assert_eq!(
            super::format_monty(&nested, 1_048_576 - 64),
            "[<int with 20001 bits>, 7, <int with 20001 bits>]"
        );
    }
}
