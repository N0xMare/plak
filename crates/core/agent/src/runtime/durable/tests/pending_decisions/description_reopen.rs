use super::*;

fn base_registry() -> TestResult<FrozenToolRegistry> {
    // The two canonical files definitions that changed since the sealed build.
    let descriptions: BTreeMap<String, String> =
        serde_json::from_str(include_str!("base_files_descriptions.json"))?;
    let current = lineage_registry()?;
    let mut builder = ToolRegistryBuilder::default();
    let executor: Arc<dyn plakous_tools::ToolExecutor> =
        Arc::new(plakous_test_support::ScriptedTool::new([]));
    for definition in current.definitions() {
        let identity = current
            .identity(definition.name())
            .ok_or("missing identity")?;
        let mut definition = definition.clone();
        if let Some(description) = descriptions.get(definition.name().as_str()) {
            definition = ToolDefinition::new(
                definition.name().clone(),
                definition.semantic_version(),
                description,
                definition.input().clone(),
                definition.output().clone(),
                definition.concurrency(),
                definition.effect(),
            )?;
        }
        let registration = ToolRegistration::new(
            identity.tool_id().clone(),
            identity.executor_id().clone(),
            identity.executor_binding_identity(),
            definition,
            executor.clone(),
        );
        builder.register(match identity.nested_eligibility() {
            plakous_tools::NestedToolEligibility::NestedEligible => registration.nested_eligible(),
            plakous_tools::NestedToolEligibility::DirectOnly => registration.direct_only(),
        })?;
    }
    Ok(builder.freeze())
}

fn base_builder(
    workspace: Arc<WorkspacePolicy>,
    model: &Arc<ArtifactModel>,
    registry: FrozenToolRegistry,
) -> TestResult<AgentBuilder> {
    let mut builder = v2_direct_patch_decision_builder(workspace, model, registry.clone())?;
    let projection = builder
        .tool_projection
        .as_ref()
        .ok_or("missing projection")?;
    let descriptions: BTreeMap<String, String> =
        serde_json::from_str(include_str!("base_files_descriptions.json"))?;
    let entries = projection
        .entries()
        .iter()
        .map(|entry| {
            if entry.exposed_name().as_str() != "apply_patch" {
                return Ok(entry.clone());
            }
            let plakous_model::ProjectedToolPresentationV2::CustomUtf8 { input, format } =
                entry.presentation()
            else {
                return Err("expected custom patch".into());
            };
            Ok(plakous_tools::internal::projection_exposure_custom(
                entry.canonical_operations().clone(),
                entry.exposed_name().as_str(),
                &descriptions["patch_envelope"],
                *input,
                format.clone(),
                entry.defaults().clone(),
                entry.lowering(),
                entry.output_schema().clone(),
                entry.raising(),
                entry.target().clone(),
            )?)
        })
        .collect::<TestResult<Vec<_>>>()?;
    builder.tool_projection = Some(plakous_tools::internal::projection_map(
        registry,
        projection.target().clone(),
        entries,
    )?);
    Ok(builder)
}

#[tokio::test]
async fn base_sealed_file_descriptors_refuse_new_descriptions_without_rewriting_session()
-> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = neutral_composition_test_descriptor()?;
    let authority = artifact_authority(&descriptor, 1)?;
    let model = Arc::new(ArtifactModel::new(
        descriptor,
        authority,
        0,
        0xc0de_7300,
        false,
    ));
    let registry = base_registry()?;
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let original = sessions
        .create(
            "create:base-files",
            base_builder(workspace.clone(), &model, registry.clone())?,
        )
        .await?;
    original.shutdown().await?;
    let checkpoint = original.checkpoint();
    let address =
        DurableSessionAddress::new(original.session_id(), original.branch_id(), checkpoint.id());
    let head = store.load_branch(address.session()).await?.head();
    let record_count = lock_mutex(&store.feed_records).len();
    let result = sessions
        .open(
            "open:new-files",
            address,
            v2_direct_patch_decision_builder(workspace.clone(), &model, lineage_registry()?)?,
        )
        .await;
    let error = result.err().ok_or("description drift silently reopened")?;
    // The compiled plan already binds the changed registry, so it refuses
    // before the later projection-digest comparison can adopt any new text.
    assert_eq!(
        error.message(),
        "builder harness plan disagrees with durable semantics"
    );
    assert_eq!(store.load_branch(address.session()).await?.head(), head);
    assert_eq!(lock_mutex(&store.feed_records).len(), record_count);
    let DurableRuntimeOpen::Opened(reopened) = sessions
        .open(
            "open:base-files",
            address,
            base_builder(workspace, &model, registry)?,
        )
        .await?
    else {
        return Err("sealed session no longer opens exactly".into());
    };
    assert_eq!(reopened.checkpoint().id(), checkpoint.id());
    assert_eq!(
        reopened.checkpoint().manifest().tools(),
        checkpoint.manifest().tools()
    );
    reopened.shutdown().await?;
    assert_eq!(model.calls.load(Ordering::Acquire), 0);
    Ok(())
}
