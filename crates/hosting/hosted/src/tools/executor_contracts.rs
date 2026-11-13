//! Release executor revisions own an explicit, append-only accepted input set.
//! `PLAK_UPDATE_EXECUTOR_CONTRACTS=1` adds reviewed current schemas without
//! removing predecessors. External MCP server schemas are not release-owned.

use super::*;
use crate::agent::rendered_prompts::suite;
use plakous::model::{Digest, DigestDomain, ToolInputContract};
use plakous::tools::web::*;
use plakous::tools::{OperationContext, ToolExecutorBindingIdentity};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;
type Contracts = BTreeMap<String, Value>;

struct NoNetwork;

impl RetrievalPort for NoNetwork {
    fn kinds(&self) -> &[RetrievalKind] {
        &[
            RetrievalKind::WebSearch,
            RetrievalKind::PageFetch,
            RetrievalKind::PostSearch,
        ]
    }

    fn retrieve(&self, _: RetrievalRequest, _: OperationContext) -> RetrievalFuture {
        panic!("executor contracts must not perform retrieval")
    }
}

impl ImageGenPort for NoNetwork {
    fn max_edit_images(&self) -> usize {
        3
    }
    fn takes_size(&self) -> bool {
        true
    }
    fn generate(&self, _: ImageGenRequest, _: OperationContext) -> ImageGenFuture {
        panic!("executor contracts must not generate images")
    }
}

fn process_binding() -> TestResult<ToolExecutorBindingIdentity> {
    Ok(ToolExecutorBindingIdentity::new(
        1,
        &[
            b"process-session-leftover-jobs",
            b"posix-sh-non-login-and-login-v1",
        ],
    )?)
}

fn collect(registry: &FrozenToolRegistry, contracts: &mut Contracts) -> TestResult {
    let expected_process = process_binding()?;
    for definition in registry.definitions() {
        let identity = registry
            .identity(definition.name())
            .ok_or("missing executor")?;
        let id = identity.executor_id().as_str();
        if id == "plakous.process-session-executor.v3" {
            assert_eq!(
                identity.executor_binding_identity(),
                expected_process,
                "{id}: binding_identity must name the fixed POSIX-shell semantic contract, not host enrollment metadata"
            );
        }
        let binding = identity.executor_binding_identity().digest().to_string();
        let key = format!("{id}/{binding}");
        let schema = match definition.input() {
            ToolInputContract::Structured(input) => {
                serde_json::from_slice(input.document().canonical_bytes())?
            }
            input => serde_json::to_value(input)?,
        };
        let row = contracts.entry(key).or_insert_with(|| {
            json!({
                "executor_id": id, "binding_identity": binding, "accepted": {}
            })
        });
        row["accepted"][identity.schema_digest().to_string()] = schema;
    }
    Ok(())
}

fn current() -> TestResult<Contracts> {
    let directory = tempfile::tempdir()?;
    let tools = suite::occupancy_tools(directory.path());
    let mut contracts = Contracts::new();
    for route in suite::routes() {
        let extensions = suite::extras_for(&route, &tools);
        // Use one explicit plan authority: production varies it per session,
        // but not the release's update-plan executor revision or input language.
        let registry = crate::agent::build::coding_tool_registry_with_extensions(
            "executor-contract",
            route.composition,
            true,
            &extensions,
        )?;
        collect(&registry, &mut contracts)?;
    }
    for presentation in [
        CodingPresentation::Codex,
        CodingPresentation::Claude,
        CodingPresentation::Grok,
        CodingPresentation::Pi,
    ] {
        let registrations = [WebTool::WebSearch, WebTool::WebFetch, WebTool::XSearch]
            .into_iter()
            .filter_map(|tool| WebToolPack::new(presentation, [tool], Arc::new(NoNetwork)).ok())
            .map(|pack| pack.registrations())
            .collect::<Result<Vec<_>, _>>()?;
        let mut builder = ToolRegistryBuilder::default();
        for registration in registrations.into_iter().flatten() {
            builder.register(registration)?;
        }
        collect(&builder.freeze(), &mut contracts)?;
    }
    let mut builder = ToolRegistryBuilder::default();
    for registration in fetch_url::registrations(FetchUrlLimits::default(), true)?
        .into_iter()
        .chain(ImageGenPack::new(Arc::new(NoNetwork)).registrations()?)
    {
        builder.register(registration)?;
    }
    collect(&builder.freeze(), &mut contracts)?;
    Ok(contracts)
}

/// Conservative structural inclusion: every old accepted object must remain
/// valid. Unknown keyword changes fail closed instead of guessing implication.
fn subset(old: &Value, new: &Value, path: &str) -> Result<(), String> {
    if old == new {
        return Ok(());
    }
    let (Some(old), Some(new)) = (old.as_object(), new.as_object()) else {
        return Err(path.to_owned());
    };
    if old.get("additionalProperties") != Some(&Value::Bool(false))
        && new
            .get("properties")
            .and_then(Value::as_object)
            .is_some_and(|properties| {
                properties.keys().any(|name| {
                    old.get("properties")
                        .and_then(|prior| prior.get(name))
                        .is_none()
                })
            })
    {
        return Err(format!("{path}.properties"));
    }
    for (key, value) in old {
        let field = format!("{path}.{key}");
        match key.as_str() {
            "description" | "$schema" | "title" => {}
            "properties" => {
                for (name, property) in value.as_object().ok_or_else(|| field.clone())? {
                    let current = new
                        .get(key)
                        .and_then(|v| v.get(name))
                        .ok_or_else(|| format!("{field}.{name}"))?;
                    subset(property, current, &format!("{field}.{name}"))?;
                }
            }
            "required" => {
                let prior = value.as_array().ok_or_else(|| field.clone())?;
                if new
                    .get(key)
                    .and_then(Value::as_array)
                    .is_some_and(|now| now.iter().any(|item| !prior.contains(item)))
                {
                    return Err(field);
                }
            }
            "additionalProperties" => {
                if value != &Value::Bool(false) && new.get(key) != Some(value) {
                    return Err(field);
                }
            }
            "items" => subset(value, new.get(key).ok_or_else(|| field.clone())?, &field)?,
            _ if new.get(key) != Some(value) => return Err(field),
            _ => {}
        }
    }
    for (key, value) in new {
        if !(old.contains_key(key)
            || matches!(
                key.as_str(),
                "description" | "title" | "$schema" | "properties"
            )
            || key == "required" && value.as_array().is_some_and(Vec::is_empty))
        {
            return Err(format!("{path}.{key}"));
        }
    }
    Ok(())
}

fn changed_field(old: &Value, new: &Value, path: &str) -> String {
    if let (Some(old), Some(new)) = (old.as_object(), new.as_object()) {
        for key in old.keys().chain(new.keys()) {
            if old.get(key) != new.get(key) {
                let field = format!("{path}.{key}");
                return match (old.get(key), new.get(key)) {
                    (Some(old), Some(new)) => changed_field(old, new, &field),
                    _ => field,
                };
            }
        }
    }
    path.to_owned()
}

fn schema_digest(schema: &Value) -> TestResult<String> {
    if schema.get("Utf8").is_some() {
        let ToolInputContract::Utf8(input) = serde_json::from_value(schema.clone())? else {
            return Err("expected UTF-8 contract".into());
        };
        Ok(Digest::sha256(
            DigestDomain::ToolDefinition,
            1,
            &[
                b"plakous-tools/utf8-input-v1",
                &input.max_bytes().to_be_bytes(),
            ],
        )?
        .to_string())
    } else {
        let ToolInputContract::Structured(input) = ToolInputContract::structured(schema.clone())?
        else {
            return Err("expected structured input".into());
        };
        Ok(input.digest().to_string())
    }
}

#[test]
fn release_executor_accepted_inputs_match_the_reviewed_contracts() -> TestResult {
    let current = current()?;
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/executor-contracts.json");
    if std::env::var("PLAK_UPDATE_EXECUTOR_CONTRACTS").is_ok_and(|value| value == "1") {
        let mut updated = if path.exists() {
            serde_json::from_slice::<Contracts>(&std::fs::read(&path)?)?
        } else {
            current.clone()
        };
        for (executor, row) in &current {
            let accepted = updated
                .entry(executor.clone())
                .or_insert_with(|| row.clone());
            for (digest, schema) in row["accepted"]
                .as_object()
                .ok_or("current accepted inputs")?
            {
                accepted["accepted"][digest] = schema.clone();
                // The same send executor also serves the frozen pre-recovery schema.
                if row["executor_id"] == "plak.topology.send_agent_message.executor.v1" {
                    let mut previous = schema.clone();
                    previous["properties"]
                        .as_object_mut()
                        .ok_or("send properties")?
                        .remove("acknowledge_recovery");
                    let ToolInputContract::Structured(input) =
                        ToolInputContract::structured(previous.clone())?
                    else {
                        unreachable!()
                    };
                    accepted["accepted"][input.digest().to_string()] = previous;
                }
            }
        }
        std::fs::write(&path, serde_json::to_vec_pretty(&updated)?)?;
    }
    let accepted: Contracts = serde_json::from_slice(&std::fs::read(path)?)?;
    assert_eq!(
        current.keys().collect::<Vec<_>>(),
        accepted.keys().collect::<Vec<_>>(),
        "release executor id/binding changed; review the revision cut before pinning"
    );
    for (executor, row) in &current {
        for field in ["executor_id", "binding_identity"] {
            assert_eq!(row[field], accepted[executor][field], "{executor}: {field}");
        }
        let pinned = &accepted[executor]["accepted"];
        for (digest, schema) in row["accepted"].as_object().ok_or("accepted inputs")? {
            let fields = pinned
                .as_object()
                .ok_or("pinned inputs")?
                .values()
                .map(|previous| changed_field(previous, schema, "$"))
                .collect::<Vec<_>>();
            assert_eq!(
                pinned.get(digest),
                Some(schema),
                "{executor}: unreviewed accepted input schema {digest}, changed fields {fields:?}; retain predecessor schemas or bump the executor revision"
            );
        }
        for (digest, previous) in pinned.as_object().ok_or("pinned inputs")? {
            assert_eq!(
                *digest,
                schema_digest(previous)?,
                "{executor}: accepted schema_digest does not match its schema"
            );
            if row["accepted"].get(digest).is_some() {
                continue;
            }
            let errors = row["accepted"]
                .as_object()
                .ok_or("current inputs")?
                .values()
                .map(|schema| subset(previous, schema, "$"))
                .collect::<Vec<_>>();
            assert!(
                errors.iter().any(Result::is_ok),
                "{executor}: predecessor schema {digest} is no longer accepted without a revision cut: {errors:?}"
            );
        }
    }
    Ok(())
}

#[test]
fn executor_schema_inclusion_rejects_narrowed_fields() {
    let old = json!({"type":"object", "properties":{"message":{"type":"string"}}, "required":["message"], "additionalProperties":false});
    let mut new = old.clone();
    new["properties"]["acknowledge_recovery"] = json!({"type":"string"});
    assert_eq!(subset(&old, &new, "$"), Ok(()));
    assert_eq!(
        changed_field(&old, &new, "$"),
        "$.properties.acknowledge_recovery"
    );
    for (field, value, expected) in [
        ("properties", json!({}), "$.properties.message"),
        (
            "required",
            json!(["message", "acknowledge_recovery"]),
            "$.required",
        ),
        (
            "properties",
            json!({"message":{"type":"integer"}}),
            "$.properties.message.type",
        ),
    ] {
        let mut changed = new.clone();
        changed[field] = value;
        assert_eq!(subset(&old, &changed, "$"), Err(expected.to_owned()));
    }
    let mut permissive = old.clone();
    permissive["additionalProperties"] = json!(true);
    assert_eq!(
        subset(&permissive, &old, "$"),
        Err("$.additionalProperties".to_owned())
    );
    // Constraining a formerly arbitrary additional property narrows the input.
    let mut broader = new.clone();
    broader["additionalProperties"] = json!(true);
    assert_eq!(
        subset(&permissive, &broader, "$"),
        Err("$.properties".to_owned())
    );
    assert_eq!(
        subset(&json!({"type":"object"}), &broader, "$"),
        Err("$.properties".to_owned())
    );
}
