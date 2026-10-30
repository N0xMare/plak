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
    let err = super::json_to_monty(&value).expect_err("integers above i64::MAX are not monty-safe");
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
