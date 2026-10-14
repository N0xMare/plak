use plakous::agent::{
    AgentBuilder, AgentError, LocalCompositionAttachment, LocalCompositionAttachmentBuilder,
    LocalCompositionEngineArtifact, LocalCompositionLanguage, LocalCompositionProfile,
    attach_local_composition,
};
use plakous::tools::CodexNativeLocalCodeModeDescriptorV1;

pub fn surface_is_nameable() {
    fn select_profile(
        descriptor: &CodexNativeLocalCodeModeDescriptorV1,
    ) -> Result<LocalCompositionProfile, AgentError> {
        LocalCompositionProfile::codex_native_local_code_mode_host_v1(descriptor)
    }
    fn verify_artifact(
        path: &std::path::Path,
    ) -> Result<LocalCompositionEngineArtifact, AgentError> {
        LocalCompositionEngineArtifact::rquickjs_sidecar_v1(path)
    }
    fn close_binding(
        profile: LocalCompositionProfile,
        artifact: LocalCompositionEngineArtifact,
    ) -> Result<LocalCompositionAttachment, AgentError> {
        LocalCompositionAttachment::builder(profile)
            .engine_artifact(artifact)
            .build()
    }
    fn dormant(builder: AgentBuilder) -> Result<AgentBuilder, AgentError> {
        attach_local_composition(builder, None)
    }

    fn hold<T: ?Sized>(_: &T) {}
    hold(&std::any::type_name::<LocalCompositionAttachment>());
    hold(&std::any::type_name::<LocalCompositionAttachmentBuilder>());
    hold(&std::any::type_name::<LocalCompositionEngineArtifact>());
    hold(&std::any::type_name::<LocalCompositionLanguage>());
    hold(&std::any::type_name::<LocalCompositionProfile>());
    hold(&select_profile);
    hold(&verify_artifact);
    hold(&close_binding);
    hold(&dormant);
}
