//! The same offline API production builder in predecessor and candidate tests.
use super::*;

pub(crate) async fn previous_fixture_runtime(
    extensions: &crate::tools::ProductToolExtensions,
    workspace: &std::path::Path,
    authority: &str,
    opening: bool,
    child: bool,
    composition: CompositionSelection,
) -> CliResult<plakous::sessions::SessionRuntimeBinding> {
    let provider = PlatformProvider::OpenAi;
    let workers = StableFixtureWorkers;
    if composition != CompositionSelection::Direct {
        fixture_artifact()?;
    }
    let spec = ApiRuntimeSpec {
        runner_services: None,
        job_config: plakous::tools::JobConfig::default(),
        account: provider.account().parse()?,
        sidecars: &workers,
        product_home: None,
        literal_model: if composition == CompositionSelection::NativePtc {
            "gpt-5.6-sol"
        } else {
            "gpt-5.6-luna"
        },
        effort: Some(ReasoningEffort::High),
        composition,
        wire: provider.protocol(),
        workspace_root: workspace,
        cwd: workspace,
        plan_state_authority: authority,
        opening_durable_session: opening,
        tool_fence: if child {
            crate::agent::SessionToolFence::OccupancyReadOnly
        } else {
            crate::agent::SessionToolFence::Full
        },
        scheduled_compaction: false,
        max_model_rounds: crate::agent::NATIVE_RUN_MAX_MODEL_ROUNDS,
        topology_extensions: extensions,
        telemetry: None,
        spawned_child: child,
    };
    configured_fixture_runtime(&spec, &Credentials("binding-fixture-key")).await
}

const ARTIFACT: &str = "/tmp/plakous-binding-fixture-v1/never-invoked-worker";
const ARTIFACT_BYTES: &[u8] = b"qualification-only artifact; never execute";

pub(crate) struct StableFixtureWorkers;
impl plakous::tools::SidecarLocator for StableFixtureWorkers {
    fn javascript_host(&self) -> Result<std::path::PathBuf, String> {
        Ok(ARTIFACT.into())
    }
    fn python_host(&self) -> Result<std::path::PathBuf, String> {
        Ok(ARTIFACT.into())
    }
}

/// Explicit immutable fixture prefix, not a current-build sidecar or user workspace.
/// Its synthetic path participates in the predecessor's actual artifact identity.
pub(crate) fn fixture_artifact() -> CliResult<std::path::PathBuf> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let path = std::path::Path::new(ARTIFACT);
    let parent = path.parent().unwrap();
    match std::fs::create_dir(parent) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(CliFailure::new(error.to_string())),
    }
    if parent
        .canonicalize()
        .map_err(|error| CliFailure::new(error.to_string()))?
        != parent
    {
        return Err(CliFailure::new(
            "fixture artifact prefix must already be canonical",
        ));
    }
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(mut file) => file
            .write_all(ARTIFACT_BYTES)
            .map_err(|error| CliFailure::new(error.to_string()))?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(CliFailure::new(error.to_string())),
    }
    if path
        .canonicalize()
        .map_err(|error| CliFailure::new(error.to_string()))?
        != path
        || std::fs::read(path).map_err(|error| CliFailure::new(error.to_string()))?
            != ARTIFACT_BYTES
    {
        return Err(CliFailure::new(
            "fixture artifact conflicts with recorded bytes",
        ));
    }
    Ok(path.into())
}

pub(crate) async fn configured_fixture_runtime(
    spec: &ApiRuntimeSpec<'_>,
    credentials: &dyn PlatformCredentials,
) -> CliResult<plakous::sessions::SessionRuntimeBinding> {
    construct_platform_runtime(PlatformProvider::OpenAi, credentials, spec, None).await
}
