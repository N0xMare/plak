//! Canonical route bindings and plans with explicit synthetic workspace facts.

use super::{
    CompositionSelection, SessionToolFence, node_session, selected_coding_provider_configuration,
    selected_harness_profile, standard_model,
};
use crate::agent::rendered_prompts::suite::{self as prompts, Occupancy, ProviderKind, RouteSpec};
use plakous::agent::{HarnessProfileSpec, ReasoningEffort, ResolvedContextSourceFact};
use plakous::model::{CompiledProviderConfiguration, Digest, DigestDomain, ModelId, Timestamp};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, btree_map::Entry};
use std::path::Path;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const INTERNED_FIELDS: [&str; 2] = ["configuration", "registry"];

fn normalize_strings(value: &mut Value, workspace: &Path) {
    match value {
        Value::String(text) => {
            *text = text
                .replace(workspace.to_str().unwrap(), "/work/repo")
                .replace(
                    &format!("OS: {}.", std::env::consts::OS),
                    "OS: fixture-platform.",
                );
        }
        Value::Array(values) => {
            for value in values {
                normalize_strings(value, workspace);
            }
        }
        Value::Object(values) => {
            for value in values.values_mut() {
                normalize_strings(value, workspace);
            }
        }
        _ => {}
    }
}

fn json_bytes(value: &Value) -> Option<Vec<u8>> {
    value
        .as_array()?
        .iter()
        .map(|byte| byte.as_u64().and_then(|byte| u8::try_from(byte).ok()))
        .collect()
}

fn readable_schemas(value: &mut Value) {
    match value {
        Value::Object(fields) => {
            if fields.get("algorithm").and_then(Value::as_str) == Some("Sha256")
                && let Some(bytes) = fields.get("bytes").and_then(json_bytes)
                && bytes.len() == 32
            {
                fields.insert(
                    "bytes".to_owned(),
                    Value::String(plakous::model::hex_lower(&bytes)),
                );
            }
            if let Some(bytes) = fields.get("canonical").and_then(json_bytes)
                && let Ok(document) = serde_json::from_slice::<Value>(&bytes)
            {
                fields.insert("canonical".to_owned(), document);
            }
            for value in fields.values_mut() {
                readable_schemas(value);
            }
        }
        Value::Array(values) => {
            for value in values {
                readable_schemas(value);
            }
        }
        _ => {}
    }
}

fn map_canonical_bytes(
    value: &mut Value,
    map: impl Fn(&mut Value) -> TestResult + Copy,
) -> TestResult {
    match value {
        Value::Object(fields) => {
            for (key, value) in fields {
                if key == "canonical_bytes" {
                    map(value)?;
                } else {
                    map_canonical_bytes(value, map)?;
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                map_canonical_bytes(value, map)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn compact_canonical_bytes(value: &mut Value) -> TestResult {
    let bytes = serde_json::from_value::<Vec<u8>>(value.take())?;
    *value = match String::from_utf8(bytes) {
        Ok(text) => Value::String(text),
        Err(error) => json!({"hex": plakous::model::hex_lower(error.as_bytes())}),
    };
    Ok(())
}

fn compact_fixture(mut matrix: Value) -> TestResult<Value> {
    for row in matrix.as_object_mut().unwrap().values_mut() {
        let config = &mut row["configuration"];
        if config["selected"]["behavior"] == config["effective"]["behavior"] {
            config["selected"]
                .as_object_mut()
                .unwrap()
                .remove("behavior");
        }
    }
    map_canonical_bytes(&mut matrix, compact_canonical_bytes)?;
    let rows = matrix.as_object_mut().unwrap();
    // Name each block for its first route in lexical order, so editing a
    // block without changing its sharing does not move it in the fixture.
    let names = rows.keys().cloned().collect::<BTreeSet<_>>();
    let mut fixture = json!({});
    for field in INTERNED_FIELDS {
        let mut table = serde_json::Map::new();
        let mut keys_by_block = BTreeMap::<_, String>::new();
        for name in &names {
            let row = rows.get_mut(name).unwrap();
            let block = row[field].take();
            let key = match keys_by_block.entry(serde_json::to_vec(&block)?) {
                Entry::Occupied(entry) => entry.get().clone(),
                Entry::Vacant(entry) => {
                    if table.insert(name.clone(), block).is_some() {
                        return Err(
                            format!("different {field} fixture blocks claim key {name}").into()
                        );
                    }
                    entry.insert(name.clone());
                    name.clone()
                }
            };
            row[field] = Value::String(key);
        }
        fixture[field] = Value::Object(table);
    }
    fixture["rows"] = matrix;
    Ok(fixture)
}

fn expand_fixture(mut fixture: Value) -> TestResult<Value> {
    let mut matrix = fixture["rows"].take();
    let rows = matrix
        .as_object_mut()
        .ok_or("missing binding fixture rows")?;
    for field in INTERNED_FIELDS {
        let table = fixture[field]
            .as_object()
            .ok_or_else(|| format!("missing binding fixture {field} table"))?;
        let mut unused: BTreeSet<_> = table.keys().map(String::as_str).collect();
        for (name, row) in &mut *rows {
            let key = row[field]
                .as_str()
                .ok_or_else(|| format!("{name}: missing or invalid {field} fixture reference"))?;
            let block = table
                .get(key)
                .ok_or_else(|| format!("{name}: unknown {field} fixture key {key}"))?;
            unused.remove(key);
            row[field] = block.clone();
        }
        if !unused.is_empty() {
            return Err(format!("unused {field} fixture keys: {unused:?}").into());
        }
    }
    // Compare the full facts, including selected behavior for every row. An
    // omitted value pins equality to the fixture's effective behavior.
    for row in rows.values_mut() {
        let config = &mut row["configuration"];
        if config["selected"].get("behavior").is_none() {
            config["selected"]["behavior"] = config["effective"]["behavior"].clone();
        }
    }
    map_canonical_bytes(&mut matrix, |value| {
        let bytes = if let Some(text) = value.as_str() {
            text.as_bytes().to_vec()
        } else {
            let hex = value["hex"].as_str().ok_or("expected UTF-8 or hex pin")?;
            let (pairs, trailing) = hex.as_bytes().as_chunks::<2>();
            if !trailing.is_empty() {
                return Err("odd-length hex pin".into());
            }
            pairs
                .iter()
                .map(|pair| Ok(u8::from_str_radix(std::str::from_utf8(pair)?, 16)?))
                .collect::<TestResult<Vec<_>>>()?
        };
        *value = json!(bytes);
        Ok(())
    })?;
    Ok(matrix)
}

fn normalized_facts(mut facts: Value, workspace: &Path) -> TestResult<Value> {
    normalize_strings(&mut facts["profile"], workspace);
    normalize_strings(&mut facts["context_sources"], workspace);
    let configuration: CompiledProviderConfiguration =
        serde_json::from_value(facts["configuration"].clone())?;
    let profile: HarnessProfileSpec = serde_json::from_value(facts["profile"].clone())?;
    let sources: Vec<ResolvedContextSourceFact> =
        serde_json::from_value(facts["context_sources"].clone())?;
    let descriptor = configuration.effective().descriptor().clone();
    let authority = plakous::model::RuntimeModelAuthority::new(
        &descriptor,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"fixture-account"])?,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"fixture-generation"])?,
        1,
        Timestamp::new(1, 0)?,
        None,
    )?;
    let model = super::UncalledModel {
        descriptor,
        authority,
    };
    let plan = plakous::agent::compile_explicit_profile(
        &profile,
        &model,
        &configuration,
        serde_json::from_value(facts["registry_digest"].clone())?,
        serde_json::from_value(facts["canonical_names"].clone())?,
        Digest::sha256(
            DigestDomain::ExecutionEnvironment,
            1,
            &[b"binding-contract-synthetic-workspace"],
        )?,
        sources,
    )?;
    facts["plan_digest"] = serde_json::to_value(plan.plan_digest())?;
    facts["model_facts"] = serde_json::to_value(plan.model_facts())?;
    facts["selected_id"] = serde_json::to_value(configuration.selected_identity())?;
    facts["effective_id"] = serde_json::to_value(configuration.effective_identity())?;
    readable_schemas(&mut facts);
    Ok(facts)
}

fn native_facts(
    route: &RouteSpec,
    extensions: &crate::tools::ProductToolExtensions,
) -> TestResult<Value> {
    let provider = route.provider.native().unwrap();
    let selected = selected_coding_provider_configuration(
        provider,
        ModelId::new(route.model)?,
        ReasoningEffort::High,
        provider.enrolled_tool_mode(route.model),
        route.composition,
    )?;
    let configured = standard_model(
        &selected,
        if provider == plakous::subscriptions::NativeProvider::Claude {
            "messages-family"
        } else {
            "responses-family"
        },
    )?;
    let registry = prompts::registry_for(route, extensions);
    let extras = extensions.names().cloned().collect::<Vec<_>>();
    let direct = route.composition == CompositionSelection::Direct;
    let fence = if route.occupancy == Occupancy::Child {
        SessionToolFence::OccupancyReadOnly
    } else {
        SessionToolFence::Full
    };
    let node = node_session(
        plakous::tools::JobConfig::default(),
        configured,
        &registry,
        extensions,
        if direct { &extras } else { &[] },
        fence,
        prompts::route_runtime(route),
    )?;
    let profile = selected_harness_profile(
        provider,
        &ModelId::new(route.model)?,
        ReasoningEffort::High,
        false,
        route.code_mode,
        &extras,
        extensions.extends_beyond_skills(),
        route.composition,
        &node.surface,
        plakous::prompts::identity::PromptWorkspace::new(
            Path::new("/work/repo"),
            Path::new("/work/repo"),
            "fixture-platform",
        ),
    )?
    .unwrap();
    let projection = prompts::compile_projection(
        route,
        extensions,
        super::projection_target(&node.configuration, "binding contract")?,
    )?;
    Ok(
        json!({"configuration":node.configuration, "registry_digest":registry.registry_identity_digest()?,
        "registry":registry.definitions().iter().filter_map(|definition|registry.identity(definition.name())).map(|identity|json!({"name":identity.name(),"definition":identity.definition_digest(),"schema":identity.schema_digest(),"executor":identity.executor_id().as_str(),"binding":identity.executor_binding_identity().digest(),"nesting":identity.nested_eligibility()})).collect::<Vec<_>>(),
        "canonical_names":registry.definitions().iter().map(|definition|definition.name().as_str()).collect::<Vec<_>>(),
        "projection_digest":projection.digest(), "profile":profile, "context_sources":[]}),
    )
}

fn diff(expected: &Value, actual: &Value, path: &str, output: &mut Vec<String>) {
    if expected == actual || output.len() >= 20 {
        return;
    }
    if path.ends_with(".canonical_bytes") {
        let before = expected.to_string();
        let after = actual.to_string();
        let shared = before
            .chars()
            .zip(after.chars())
            .take_while(|(a, b)| a == b)
            .count();
        let start = shared.saturating_sub(60);
        let preview = |value: &str| value.chars().skip(start).take(240).collect::<String>();
        let prefix = if start == 0 { "" } else { "…" };
        output.push(format!(
            "{path}: {prefix}{} -> {prefix}{}",
            preview(&before),
            preview(&after)
        ));
        return;
    }
    match (expected, actual) {
        (Value::Object(before), Value::Object(after)) => {
            let keys = before.keys().chain(after.keys()).collect::<BTreeSet<_>>();
            for key in keys {
                diff(
                    before.get(key).unwrap_or(&Value::Null),
                    after.get(key).unwrap_or(&Value::Null),
                    &format!("{path}.{key}"),
                    output,
                );
            }
        }
        (Value::Array(before), Value::Array(after)) => {
            if before.len() != after.len() {
                output.push(format!(
                    "{path}.length: {} -> {}",
                    before.len(),
                    after.len()
                ));
            }
            for (index, (before, after)) in before.iter().zip(after).enumerate() {
                diff(before, after, &format!("{path}[{index}]"), output);
            }
        }
        _ => output.push(format!(
            "{path}: {} -> {}",
            expected.to_string().chars().take(240).collect::<String>(),
            actual.to_string().chars().take(240).collect::<String>()
        )),
    }
}

#[tokio::test]
async fn canonical_route_bindings_and_plans_match_the_reviewed_matrix() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = std::fs::canonicalize(directory.path())?;
    let tools = prompts::occupancy_tools(&workspace);
    let mut output = serde_json::Map::new();
    for mut route in prompts::routes() {
        // Prompt renders include forward-looking costume rows; binding evidence
        // must compile an enrolled model, not substitute admission for a render.
        if let Some(provider) = route.provider.native()
            && !provider.enrolls(route.model)
        {
            route.model = match provider {
                plakous::subscriptions::NativeProvider::Codex if route.code_mode => "gpt-5.6-sol",
                plakous::subscriptions::NativeProvider::Codex => "gpt-5.6-luna",
                plakous::subscriptions::NativeProvider::Claude => "claude-opus-5-5",
                _ => "grok-4.6",
            };
        }
        if matches!(route.provider, ProviderKind::Platform(_)) {
            let occupancies: &[Occupancy] = if route.composition == CompositionSelection::PySdk {
                &[Occupancy::SingleAgent]
            } else {
                &[Occupancy::Root, Occupancy::Child]
            };
            for &occupancy in occupancies {
                route.occupancy = occupancy;
                let extensions = prompts::extras_for(&route, &tools);
                let ProviderKind::Platform(provider) = route.provider else {
                    unreachable!();
                };
                let facts = crate::platform::tests::binding_matrix_facts(
                    provider,
                    &route,
                    &extensions,
                    &workspace,
                )
                .await?;
                let key = format!(
                    "{}.{}",
                    route.file_stem,
                    match occupancy {
                        Occupancy::Root => "root",
                        Occupancy::Child => "leaf",
                        _ => "single",
                    }
                );
                output.insert(key, normalized_facts(facts, &workspace)?);
            }
            continue;
        }
        if !(matches!(route.occupancy, Occupancy::Root | Occupancy::Child)
            || route.composition == CompositionSelection::PySdk
                && route.occupancy == Occupancy::SingleAgent)
        {
            continue;
        }
        let extensions = prompts::extras_for(&route, &tools);
        let facts = match route.provider {
            ProviderKind::OpenRouter { wire } => {
                crate::openrouter::binding_matrix_facts(&route, wire, &extensions, &workspace)?
            }
            _ => native_facts(&route, &extensions)?,
        };
        output.insert(
            route.file_stem.clone(),
            normalized_facts(facts, &workspace)?,
        );
    }
    let mut actual = Value::Object(output);
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/binding-contracts.json");
    if std::env::var("PLAK_UPDATE_BINDING_CONTRACTS").is_ok_and(|value| value == "1") {
        let fixture = compact_fixture(actual.clone())?;
        std::fs::write(&path, serde_json::to_vec_pretty(&fixture)?)?;
    }
    let raw = std::fs::read(&path)?;
    let mut expected = expand_fixture(serde_json::from_slice(&raw)?)?;
    // Value parsing can discard duplicate keys, so check the original bytes.
    assert!(
        serde_json::to_vec_pretty(&compact_fixture(expected.clone())?)? == raw,
        "binding fixture {} is not canonical (duplicate or noncanonical keys or formatting); \
         regenerate with PLAK_UPDATE_BINDING_CONTRACTS=1",
        path.display()
    );
    // Expand behavior defaults for field-level comparison, but render a
    // canonical byte field as one value rather than one diff per shifted byte.
    map_canonical_bytes(&mut expected, compact_canonical_bytes)?;
    map_canonical_bytes(&mut actual, compact_canonical_bytes)?;
    let mut changes = Vec::new();
    let routes = expected
        .as_object()
        .unwrap()
        .keys()
        .chain(actual.as_object().unwrap().keys())
        .collect::<BTreeSet<_>>();
    for route in routes {
        let mut row = Vec::new();
        diff(
            &expected[route],
            &actual[route],
            &format!("$.{route}"),
            &mut row,
        );
        changes.extend(row);
    }
    assert!(
        changes.is_empty(),
        "binding contract changed in {}; review the field-level diff and prove predecessor \
         exact reopen before repinning with PLAK_UPDATE_BINDING_CONTRACTS=1:\n{}",
        path.display(),
        changes.join("\n")
    );
    Ok(())
}
