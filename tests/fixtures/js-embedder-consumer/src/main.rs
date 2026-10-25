//! The composition-only embedding workflow, with either cell worker.
//!
//! A fresh external consumer can select JavaScript or Python composition
//! using this manifest alone. The `composition` feature reaches neither
//! interpreter's worker graph nor any git dependency.
//!
//! It also shows that the composition surface is fully nameable without the
//! subscription, credential, and HTTP machinery that used to come with it.

use std::sync::Arc;

use plakous::agent::{
    AgentBuilder, AgentError, LocalCompositionAttachment, LocalCompositionAttachmentBuilder,
    LocalCompositionContainment, LocalCompositionEngineArtifact, LocalCompositionLanguage,
    LocalCompositionProfile, ProductExtensionEvidence, ProductExtensionLimits,
    attach_local_composition,
};
use plakous::construction::{
    CustomProviderConfigurationRequest, bind_custom_model_authority,
    compile_local_composition_plan, provider_model_projection_identity,
};
use plakous::model::{
    CompiledProviderConfiguration, ContextContract, Digest, ResolvedModelDescriptor,
    RuntimeModelAuthority, RuntimeModelAuthorityError, Timestamp,
};
use plakous::sessions::{AgentSessions, SessionBackend};
use plakous::tools::{
    ProductJavaScriptCompositionDescriptorV1, ProductJavaScriptCompositionV1,
    PythonPtcDescriptorV1, ToolRegistryBuilder, WorkspacePolicy,
};

/// Names the composition-only surface for both JavaScript and Python cell workers.
fn composition_surface_is_nameable() {
    fn select_sidecar_profile(
        descriptor: &ProductJavaScriptCompositionDescriptorV1,
    ) -> Result<LocalCompositionProfile, AgentError> {
        LocalCompositionProfile::product_javascript_v1(descriptor)
    }
    fn verify_host(path: &std::path::Path) -> Result<LocalCompositionEngineArtifact, AgentError> {
        LocalCompositionEngineArtifact::quickjs_worker_v1(path)
    }
    fn select_python_profile(
        descriptor: &PythonPtcDescriptorV1,
    ) -> Result<LocalCompositionProfile, AgentError> {
        LocalCompositionProfile::python_ptc_v1(descriptor)
    }
    fn verify_python_host(
        path: &std::path::Path,
    ) -> Result<LocalCompositionEngineArtifact, AgentError> {
        LocalCompositionEngineArtifact::monty_worker_v1(path)
    }
    fn qualify_cell(
        profile: &LocalCompositionProfile,
        artifact: &LocalCompositionEngineArtifact,
        issuer: Digest,
        qualification: Digest,
        configuration: &CompiledProviderConfiguration,
    ) -> Result<ProductExtensionEvidence, AgentError> {
        ProductExtensionEvidence::cell(profile, artifact, issuer, qualification, configuration)
    }
    fn close_binding(
        profile: LocalCompositionProfile,
        artifact: LocalCompositionEngineArtifact,
        evidence: ProductExtensionEvidence,
    ) -> Result<LocalCompositionAttachment, AgentError> {
        LocalCompositionAttachment::builder(profile)
            .engine_artifact(artifact)
            .coding_presentation_kind(plakous::tools::CodingPresentation::Pi)
            .product_extension_evidence(evidence)
            .build()
    }
    fn dormant(builder: AgentBuilder) -> Result<AgentBuilder, AgentError> {
        attach_local_composition(builder, None)
    }

    fn hold<T: ?Sized>(_: &T) {}
    hold(&std::any::type_name::<LocalCompositionAttachmentBuilder>());
    hold(&std::any::type_name::<LocalCompositionContainment>());
    hold(&std::any::type_name::<LocalCompositionLanguage>());
    hold(&std::any::type_name::<ProductExtensionEvidence>());
    hold(&std::any::type_name::<ProductExtensionLimits>());
    hold(&std::any::type_name::<
        ProductJavaScriptCompositionDescriptorV1,
    >());
    hold(&select_sidecar_profile);
    hold(&verify_host);
    hold(&select_python_profile);
    hold(&verify_python_host);
    hold(&qualify_cell);
    hold(&close_binding);
    hold(&dormant);
}

/// A caller-owned error type the authority helper reports through.
#[derive(Debug)]
struct ConsumerError(String);

impl std::fmt::Display for ConsumerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ConsumerError {}

impl From<RuntimeModelAuthorityError> for ConsumerError {
    fn from(error: RuntimeModelAuthorityError) -> Self {
        Self(error.to_string())
    }
}

/// Binds a descriptor and its authority in one step, as an embedder would.
fn bind_authority(
    descriptor: ResolvedModelDescriptor,
    binding: Digest,
    checked_at: Timestamp,
    contract: fn(Digest) -> Result<ContextContract, ConsumerError>,
) -> Result<(ResolvedModelDescriptor, RuntimeModelAuthority), ConsumerError> {
    bind_custom_model_authority(descriptor, binding, binding, 1, checked_at, None, contract)
}

/// Names the construction helpers and the durable session backend.
fn construction_surface_is_nameable() {
    fn hold<T: ?Sized>(_: &T) {}
    hold(&std::any::type_name::<CustomProviderConfigurationRequest<'_>>());
    hold(&std::any::type_name::<AgentSessions>());
    hold(&std::any::type_name::<ToolRegistryBuilder>());
    hold(&std::any::type_name::<WorkspacePolicy>());
    hold(&std::any::type_name::<Arc<SessionBackend>>());
    hold(&bind_authority);
    hold(&compile_local_composition_plan);
    hold(&provider_model_projection_identity);
    fn compile_javascript(
        registry: plakous::tools::FrozenToolRegistry,
        target: plakous::tools::ProviderModelProjectionIdentityV2,
        nested: Vec<plakous::model::ToolName>,
    ) -> Result<ProductJavaScriptCompositionDescriptorV1, Box<dyn std::error::Error>> {
        Ok(ProductJavaScriptCompositionV1::compile_for_kind(
            plakous::tools::JobConfig::default(),
            registry,
            target,
            plakous::tools::CodingPresentation::Pi,
            nested,
        )?)
    }
    hold(&compile_javascript);
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    assert_eq!(plakous::VERSION, "0.0.2");
    composition_surface_is_nameable();
    construction_surface_is_nameable();
    // The durable backend is part of this workflow and needs no patch table.
    let directory = tempfile::tempdir()?;
    let _sessions = AgentSessions::new(SessionBackend::sqlite(
        directory.path().join("js-embedder.sqlite3"),
    )?)?;
    println!("Composition-only embedding surface builds with no patch table");
    Ok(())
}
