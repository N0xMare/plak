use std::path::Path;
use std::sync::Arc;

use plakous::prompts::identity::{PromptWorkspace, product_environment_sentence};
use plakous::runner::protocol::VersionTriple;
use plakous::runner::{RunnerIdentity, RunnerService, RunnerServices};
use plakous::tools::runner::{RunnerBackend, RunnerCall, RunnerCallFuture, RunnerLink};
use plakous::tools::{
    ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope, RunnerId,
};

use super::node_backend_descriptor;

struct NoCalls;

impl RunnerLink for NoCalls {
    fn call(&self, _call: RunnerCall) -> RunnerCallFuture {
        panic!("naming the backend OS must not call the runner")
    }
}

/// A node placed on a runner of another OS tells the model the runner's OS,
/// not the OS of the host that renders the prompt; a local node names this
/// process's OS.
#[tokio::test]
async fn the_prompt_names_the_os_of_the_backend_the_tools_run_on() {
    let workspace = tempfile::tempdir().unwrap();
    assert_eq!(
        node_backend_descriptor(None),
        plakous::tools::ExecutionBackendDescriptor::local_runner()
    );
    let service = RunnerService::new(
        ExecutionEnvironment::host(
            ExecutionEnvironmentScope::Run(uuid::Uuid::new_v4().try_into().unwrap()),
            workspace.path(),
            ".",
            ExecutionEnvironmentConfig::default(),
        )
        .unwrap(),
        RunnerIdentity {
            runner_id: RunnerId::new("remote").unwrap(),
            version: VersionTriple::current("runner-os", "debug"),
            boot_id: "boot".into(),
            binary_id: None,
        },
    )
    .unwrap();
    for (os, filesystem) in [
        ("linux", "LinuxDescriptorRelativeNoFollow"),
        ("macos", "HostPathPolicyOnly"),
    ] {
        let mut descriptor = service.hello().descriptor.clone();
        descriptor["operating_system"] = os.into();
        descriptor["containment"]["filesystem"] = filesystem.into();
        let backend = Arc::new(RunnerBackend::new(descriptor, Arc::new(NoCalls)).unwrap());
        let services = RunnerServices::new(backend, tokio::runtime::Handle::current());
        let backend = node_backend_descriptor(Some(&services));
        assert_eq!(backend.operating_system(), os);
        assert_eq!(backend.runner_id().map(RunnerId::as_str), Some("remote"));
        let sentence = product_environment_sentence(PromptWorkspace::new(
            Path::new("/w"),
            Path::new("/w"),
            backend.operating_system(),
        ));
        assert!(sentence.ends_with(&format!("OS: {os}.")), "{sentence}");
    }
}
