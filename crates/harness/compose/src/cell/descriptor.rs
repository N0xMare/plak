use plakous_model::Digest;
use plakous_tools::ToolProjectionMap;

use super::{
    CodexMinimalLocalCodeModeDescriptorV1, CodexNativeLocalCodeModeDescriptorV1,
    ProductJavaScriptCompositionDescriptorV1, PythonPtcDescriptorV1,
};

/// One compiled cell presentation, independent of its interpreter attachment.
///
/// Each variant holds the exact compiler output; wrapping it does not change
/// its descriptor identity, projections, or model-visible text.
/// This enum is exhaustive; adding a presentation is a breaking API change.
///
/// Inner compiler output fields remain private even when constructing a variant:
///
/// ```compile_fail,E0451
/// use plakous_compose::{CellDescriptor, PythonPtcDescriptorV1};
/// fn forge(descriptor: PythonPtcDescriptorV1) -> CellDescriptor {
///     CellDescriptor::PythonPtcV1(PythonPtcDescriptorV1 {
///         identity: descriptor.identity(),
///         ..descriptor
///     })
/// }
/// ```
#[derive(Clone)]
pub enum CellDescriptor {
    /// Frozen Codex seven-tool reference presentation.
    CodexNativeLocalCodeModeV1(CodexNativeLocalCodeModeDescriptorV1),
    /// Codex minimal presentation with explicit nested extensions.
    CodexMinimalLocalCodeModeV1(CodexMinimalLocalCodeModeDescriptorV1),
    /// Product JavaScript cells.
    ProductJavaScriptV1(ProductJavaScriptCompositionDescriptorV1),
    /// Product Python coroutine cells.
    PythonPtcV1(PythonPtcDescriptorV1),
}

impl CellDescriptor {
    /// Complete v3 descriptor identity bound by the presentation compiler.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        match self {
            Self::CodexNativeLocalCodeModeV1(descriptor) => descriptor.identity(),
            Self::CodexMinimalLocalCodeModeV1(descriptor) => descriptor.identity(),
            Self::ProductJavaScriptV1(descriptor) => descriptor.identity(),
            Self::PythonPtcV1(descriptor) => descriptor.identity(),
        }
    }

    /// Exact provider-visible outer tools.
    #[must_use]
    pub const fn outer_projection(&self) -> &ToolProjectionMap {
        match self {
            Self::CodexNativeLocalCodeModeV1(descriptor) => descriptor.outer_projection(),
            Self::CodexMinimalLocalCodeModeV1(descriptor) => descriptor.outer_projection(),
            Self::ProductJavaScriptV1(descriptor) => descriptor.outer_projection(),
            Self::PythonPtcV1(descriptor) => descriptor.outer_projection(),
        }
    }

    /// Exact executor-bound nested catalog.
    #[must_use]
    pub const fn nested_projection(&self) -> &ToolProjectionMap {
        match self {
            Self::CodexNativeLocalCodeModeV1(descriptor) => descriptor.nested_projection(),
            Self::CodexMinimalLocalCodeModeV1(descriptor) => descriptor.nested_projection(),
            Self::ProductJavaScriptV1(descriptor) => descriptor.nested_projection(),
            Self::PythonPtcV1(descriptor) => descriptor.nested_projection(),
        }
    }
}

macro_rules! cell_descriptor_from {
    ($descriptor:ty, $variant:ident) => {
        impl From<$descriptor> for CellDescriptor {
            fn from(descriptor: $descriptor) -> Self {
                Self::$variant(descriptor)
            }
        }
        impl From<&$descriptor> for CellDescriptor {
            fn from(descriptor: &$descriptor) -> Self {
                Self::$variant(descriptor.clone())
            }
        }
    };
}

cell_descriptor_from!(
    CodexNativeLocalCodeModeDescriptorV1,
    CodexNativeLocalCodeModeV1
);
cell_descriptor_from!(
    CodexMinimalLocalCodeModeDescriptorV1,
    CodexMinimalLocalCodeModeV1
);
cell_descriptor_from!(
    ProductJavaScriptCompositionDescriptorV1,
    ProductJavaScriptV1
);
cell_descriptor_from!(PythonPtcDescriptorV1, PythonPtcV1);
