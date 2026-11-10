//! One workspace-path rule and one patch preflight.
//!
//! Bounded reads, exact-content patching and image inspection, plus every
//! provider lowering of them: `Write`, `write` and `search_replace` are
//! lowerings of `apply_patch`, not separate capabilities (spec 02 §6.3), so
//! they live here with the preflight that makes them exact, and the Codex
//! freeform patch envelope with the executor it shares.

#![forbid(unsafe_code)]
#![allow(
    clippy::doc_markdown,
    reason = "doc comments name exact provider tool identifiers"
)]

mod codex_patch;
mod edit_files;
mod image;
mod mutation;
mod patch;
mod read;
mod text;
mod visual;

pub use text::{
    PLAKOUS_CODEX_APPLY_PATCH_FREEFORM_DESCRIPTION, PLAKOUS_GROK_READ_CELL_DESCRIPTION,
    PLAKOUS_GROK_READ_DESCRIPTION, PLAKOUS_GROK_SEARCH_REPLACE_DESCRIPTION,
    PLAKOUS_GROK_WRITE_DESCRIPTION, PLAKOUS_TOOL_APPLY_PATCH_DESCRIPTION,
    PLAKOUS_TOOL_APPLY_PATCH_REQUIRES_INPUT, PLAKOUS_TOOL_CREATE_PATCH_NONEMPTY,
    PLAKOUS_TOOL_DELETE_PATCH_NONEMPTY, PLAKOUS_TOOL_IMAGE_TOO_LARGE,
    PLAKOUS_TOOL_INSPECT_IMAGE_DESCRIPTION, PLAKOUS_TOOL_PATCH_HUNK_CONFLICT,
    PLAKOUS_TOOL_PATCH_KIND_CHANGED, PLAKOUS_TOOL_PATCH_MALFORMED,
    PLAKOUS_TOOL_PATCH_TARGET_NOT_UTF8, PLAKOUS_TOOL_PATCH_TARGET_TOO_LARGE,
    PLAKOUS_TOOL_PATCH_WORKSPACE_CONFLICT, PLAKOUS_TOOL_READ_FILE_DESCRIPTION,
    PLAKOUS_TOOL_READ_OFFSET_PAST_EOF, PLAKOUS_TOOL_SEARCH_REPLACE_AMBIGUOUS,
    PLAKOUS_TOOL_SEARCH_REPLACE_NOT_FOUND, PLAKOUS_TOOL_SEARCH_REPLACE_NOT_UTF8,
    PLAKOUS_TOOL_WRITE_STALE, PLAKOUS_TOOL_WRITE_UNREAD,
};

use crate::codex_patch::{
    CODEX_APPLY_PATCH_GRAMMAR_V1, CODEX_PATCH_ENVELOPE_CANONICAL_NAME,
    CODEX_PATCH_ENVELOPE_MAX_BYTES, CODEX_PATCH_ENVELOPE_REGISTERED_TOOL_ID,
    codex_patch_envelope_definition,
};
use plakous_model::{ProjectedCustomToolFormatV2, Utf8TextContractV1};
use plakous_tools::CodingPresentation;
use plakous_tools::PromptPin;
use plakous_tools::object_schema;
use plakous_tools::{
    APPLY_PATCH_CAPABILITY_NAME, READ_FILE_CAPABILITY_NAME, VIEW_IMAGE_CAPABILITY_NAME,
};
use plakous_tools::{
    CanonicalProcessOperationV2, FrozenToolRegistry, RegisteredToolId, ToolExecutorBindingIdentity,
    ToolExecutorId, ToolProjectionLoweringV2, ToolProjectionRaisingV2, ToolRegistration,
};
use plakous_tools::{MenuBinding, MenuEntry, MenuShape, ToolPack, tool_name};
use plakous_tools::{ToolContext, ToolExecutor, ToolRuntimeError, ToolRuntimeErrorKind};
use plakous_tools::{
    standard_executor_id, standard_registration, standard_registration_with_executor,
};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use text::PLAKOUS_TOOL_READ_BINARY;

use image::InspectImageTool;
use patch::ApplyPatchTool;
use read::ReadFileTool;
#[doc(hidden)]
pub use read::skill_root::SkillFileIdentity;

fn relative_workspace_path(
    context: &ToolContext,
    path: impl AsRef<Path>,
) -> Result<PathBuf, ToolRuntimeError> {
    context.workspace().relative_input(path)
}

// ---------------------------------------------------------------------------
// The files pack.
// ---------------------------------------------------------------------------

// Adapted from pi 1.1.0's file tool descriptions (MIT); see the README License section.
const PI_READ_DESCRIPTION: &str = "Read the contents of a text file; images and binary files are refused. Output is truncated to 2000 lines or 50 KiB (50 * 1024 bytes), whichever is hit first, and lines longer than 2000 characters are clipped. Use offset/limit for large files. When you need the full file, continue with offset until complete. path is relative to the workspace or absolute under it, or exactly a path a bash result or completion notice named on its `full output:` line.";
const PI_EDIT_DESCRIPTION: &str = "Edit a single file using exact text replacement. Every edits[].oldText must match a unique, non-overlapping region of the original file. If two changes affect the same block or nearby lines, merge them into one edit instead of emitting overlapping edits. Do not include large unchanged regions just to connect distant changes. The edits apply together or not at all. Success returns changed_files=N followed by A/M/D and each JSON-quoted workspace-relative changed path in lexical order; unchanged files are omitted.";
const PI_WRITE_DESCRIPTION: &str = "Write content to a file. Creates missing files and parent directories. Overwriting an existing file requires an explicit text read in this session attachment and unchanged content, not just unchanged mtime. Read again after each change to that file or resume/reload. Success returns changed_files=N followed by A/M/D and each JSON-quoted workspace-relative changed path in lexical order; unchanged files are omitted.";
const CLAUDE_READ_DESCRIPTION: &str = "Reads a file from the local filesystem.\n\n- Reads up to 2000 lines by default.\n- If you already know which part of the file to read, only read that part. This is important for larger files/context efficiency.\n- `offset` is the 1-indexed first line (omit to start at line 1). `limit` is how many lines to return (default 2000, max 2000).\n- The result is cat -n text: a 6-character file line number, a tab, then the line. Numbers are the file's real line numbers (offset 10 starts at line 10).\n- If truncated, a last line `... more lines; next_offset=N` means call again with that offset.\n- Lines longer than 2000 characters are clipped.\n- PNG, JPEG, GIF and WebP are returned as an image when this route's model accepts image input. Other binary files are refused.\n- A model that does not accept images is refused with the route and model named.\n- `file_path` is an absolute path under the workspace root, a workspace-relative path without `..`, or exactly a path a Bash result or completion notice named on its `full output:` line; other paths are refused, as is a `full output:` path issued by an earlier process (before a restart or reload) or another agent.\n- A read of an output file whose writing failed ends with `[output file incomplete: write failed after N bytes]`; the file stops there.\n- An empty file succeeds with an empty body, missing file/directory returns an error.";
const CLAUDE_EDIT_DESCRIPTION: &str = "Performs exact string replacement in a file.\n\n- `old_string` must be non-empty and match the file exactly, including indentation, and be unique — the edit fails otherwise.\n- `replace_all: true` replaces every occurrence instead.\n- Use Write to create a new file.\n- Success returns changed_files=N followed by A/M/D and each JSON-quoted workspace-relative changed path in lexical order; unchanged files are omitted.";
const CLAUDE_WRITE_DESCRIPTION: &str = "Writes a file to the local filesystem.\n\n- Creates missing files and parent directories.\n- Overwriting an existing file requires an explicit text read in this session attachment and unchanged content, not just unchanged mtime. Read again after each change to that file or resume/reload.\n- Prefer Edit for partial changes.\n- Success returns changed_files=N followed by A/M/D and each JSON-quoted workspace-relative changed path in lexical order; unchanged files are omitted.";
const VIEW_IMAGE_DESCRIPTION: &str = "View a local image file from the filesystem when visual inspection is needed. Use this for images already available on disk. path is an absolute path under the workspace root or a workspace-relative path without `..`; other paths are refused. PNG, JPEG, GIF and WebP are returned as an image. A model that does not accept image input is refused with the route and model named.";

/// The Codex freeform patch envelope: the same apply-patch executor under the
/// registration a Codex costume expects. It is the files pack's registration
/// because it is the files pack's executor.
fn codex_patch_envelope_registration(
    apply_executor_id: &ToolExecutorId,
    apply_executor: Arc<dyn ToolExecutor>,
) -> Result<ToolRegistration, ToolRuntimeError> {
    Ok(ToolRegistration::new(
        RegisteredToolId::new(CODEX_PATCH_ENVELOPE_REGISTERED_TOOL_ID)?,
        apply_executor_id.clone(),
        ToolExecutorBindingIdentity::new(
            4,
            &[
                b"standard-coding-tool",
                apply_executor_id.as_str().as_bytes(),
            ],
        )?,
        codex_patch_envelope_definition()?,
        apply_executor,
    )
    .nested_eligible())
}

/// One workspace-path rule and one patch preflight: bounded reads, exact-content
/// patching, and image inspection, plus every provider lowering of them.
///
/// `Write`, `write`, and `search_replace` are lowerings of `apply_patch`, not
/// separate capabilities (spec 02 §6.3), so they live here with the preflight
/// that makes them exact.
///
/// The default pack reads only the workspace (and issued job output files).
/// [`FilesToolPack::with_skill_roots`] adds read-only skill roots.
#[derive(Clone, Default)]
pub struct FilesToolPack {
    skill_roots: Arc<[PathBuf]>,
    skill_runner: Option<plakous_tools::internal::runner::RunnerServices>,
}

impl std::fmt::Debug for FilesToolPack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FilesToolPack")
            .field("skill_roots", &self.skill_roots)
            .field("remote_skills", &self.skill_runner.is_some())
            .finish()
    }
}

impl FilesToolPack {
    /// A pack whose read also admits regular files under these user skill
    /// roots outside the workspace (spec 04 §2.1). Each root is canonicalized now; a root that does not
    /// exist as a directory now is dropped and stays unreadable for this pack.
    /// Reads under a root are read-only, follow no link out of their root and
    /// keep the workspace read's bounds and refusals.
    #[must_use]
    pub fn with_skill_roots(roots: impl IntoIterator<Item = PathBuf>) -> Self {
        let mut canonical = Vec::new();
        for root in roots {
            if let Ok(path) = std::fs::canonicalize(&root)
                && path.is_dir()
                && !canonical.contains(&path)
            {
                canonical.push(path);
            }
        }
        Self {
            skill_roots: canonical.into(),
            skill_runner: None,
        }
    }

    /// Uses canonical user skill roots discovered on this runner. These paths
    /// are never resolved or read on the host; the runner rechecks its own root
    /// containment for every read.
    #[must_use]
    pub fn with_runner_skill_roots(
        roots: impl IntoIterator<Item = PathBuf>,
        runner: plakous_tools::internal::runner::RunnerServices,
    ) -> Self {
        Self {
            skill_roots: roots.into_iter().collect(),
            skill_runner: Some(runner),
        }
    }

    /// The runner extension's bounded, read-only user-skill file operation.
    ///
    /// # Errors
    /// Refuses paths outside `root`, escaping links and non-regular files.
    #[doc(hidden)]
    pub fn read_skill_slice(
        root: &Path,
        path: &Path,
        offset: u64,
    ) -> Result<(Vec<u8>, bool, SkillFileIdentity), ToolRuntimeError> {
        read::skill_root::read_slice(root, path, offset)
    }

    /// Registers transactional file operations for an explicitly enrolled SDK catalog.
    /// Existing files packs do not enroll this capability.
    ///
    /// # Errors
    ///
    /// Returns a definition or executor-identity error.
    pub fn edit_files_registration() -> Result<ToolRegistration, ToolRuntimeError> {
        standard_registration("edit-files", edit_files::EditFilesTool::new())
    }
}

impl ToolPack for FilesToolPack {
    fn id(&self) -> &'static str {
        "plakous.files.v3"
    }

    /// Three canonical capabilities and the Codex freeform patch envelope,
    /// which is the same executor under a provider-familiar registration.
    fn registrations(&self) -> Result<Vec<ToolRegistration>, ToolRuntimeError> {
        let (apply_definition, apply_tool) = ApplyPatchTool::new()?;
        let apply_executor: Arc<dyn ToolExecutor> = Arc::new(apply_tool);
        let apply_executor_id = standard_executor_id("apply-patch")?;
        Ok(vec![
            standard_registration(
                "read-file",
                ReadFileTool::new(Arc::clone(&self.skill_roots), self.skill_runner.clone()),
            )?,
            standard_registration("view-image", visual::ViewImageTool::new())?,
            standard_registration_with_executor(
                "apply-patch",
                apply_definition,
                Arc::clone(&apply_executor),
                &apply_executor_id,
            )?,
            standard_registration("inspect-image", InspectImageTool::new())?,
            codex_patch_envelope_registration(&apply_executor_id, apply_executor)?,
        ])
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the four vocabularies of one pack stay in one menu"
    )]
    fn menu(&self, presentation: CodingPresentation) -> Result<Vec<MenuEntry>, ToolRuntimeError> {
        let read_file = tool_name(
            READ_FILE_CAPABILITY_NAME,
            "canonical coding capability name is invalid",
        )?;
        let apply_patch = tool_name(
            APPLY_PATCH_CAPABILITY_NAME,
            "canonical coding capability name is invalid",
        )?;
        Ok(match presentation {
            CodingPresentation::Pi => pi_menu(&read_file, &apply_patch),
            CodingPresentation::Codex => {
                let view_image = tool_name(
                    VIEW_IMAGE_CAPABILITY_NAME,
                    "canonical coding capability name is invalid",
                )?;
                vec![
                    codex_patch_envelope_entry()?,
                    MenuEntry::new(
                        "view_image",
                        vec![MenuBinding::by_effect(view_image)],
                        MenuShape::Structured {
                            description: Arc::from(VIEW_IMAGE_DESCRIPTION),
                            schema: visual::view_image_schema(),
                        },
                        ToolProjectionLoweringV2::CanonicalPassthrough,
                    )
                    .with_raising(ToolProjectionRaisingV2::ModelContentIdentityV1),
                ]
            }
            CodingPresentation::Grok => vec![
                MenuEntry::new(
                    "read_file",
                    vec![MenuBinding::by_effect(read_file)],
                    MenuShape::Structured {
                        description: Arc::from(PLAKOUS_GROK_READ_DESCRIPTION),
                        schema: object_schema(
                            json!({
                                "target_file": {"type": "string"},
                                "offset": {"type": "integer", "minimum": 1},
                                "limit": {"type": "integer", "minimum": 1, "maximum": 2000}
                            }),
                            &["target_file"],
                        ),
                    },
                    ToolProjectionLoweringV2::GrokReadFile,
                ),
                MenuEntry::new(
                    "search_replace",
                    vec![MenuBinding::by_effect(apply_patch.clone())],
                    MenuShape::Structured {
                        description: Arc::from(PLAKOUS_GROK_SEARCH_REPLACE_DESCRIPTION),
                        schema: object_schema(
                            json!({
                                "file_path": {"type": "string"},
                                "old_string": {"type": "string"},
                                "new_string": {"type": "string"},
                                "replace_all": {"type": "boolean"}
                            }),
                            &["file_path", "old_string", "new_string"],
                        ),
                    },
                    ToolProjectionLoweringV2::GrokSearchReplace,
                ),
                MenuEntry::new(
                    "write",
                    vec![MenuBinding::by_effect(apply_patch)],
                    MenuShape::Structured {
                        description: Arc::from(PLAKOUS_GROK_WRITE_DESCRIPTION),
                        schema: object_schema(
                            json!({
                                "file_path": {"type": "string"},
                                "content": {"type": "string"}
                            }),
                            &["file_path", "content"],
                        ),
                    },
                    ToolProjectionLoweringV2::GrokWrite,
                ),
            ],
            CodingPresentation::Claude => vec![
                MenuEntry::new(
                    "Read",
                    vec![MenuBinding::by_effect(read_file)],
                    MenuShape::Structured {
                        description: Arc::from(CLAUDE_READ_DESCRIPTION),
                        schema: object_schema(
                            json!({
                                "file_path": {"type": "string"},
                                "offset": {"type": "integer", "minimum": 1},
                                "limit": {"type": "integer", "minimum": 1, "maximum": 2000}
                            }),
                            &["file_path"],
                        ),
                    },
                    ToolProjectionLoweringV2::ClaudeReadFile,
                ),
                MenuEntry::new(
                    "Edit",
                    vec![MenuBinding::by_effect(apply_patch.clone())],
                    MenuShape::Structured {
                        description: Arc::from(CLAUDE_EDIT_DESCRIPTION),
                        schema: object_schema(
                            json!({
                                "file_path": {"type": "string"},
                                "old_string": {"type": "string"},
                                "new_string": {"type": "string"},
                                "replace_all": {"type": "boolean"}
                            }),
                            &["file_path", "old_string", "new_string"],
                        ),
                    },
                    ToolProjectionLoweringV2::ClaudeEdit,
                ),
                MenuEntry::new(
                    "Write",
                    vec![MenuBinding::by_effect(apply_patch)],
                    MenuShape::Structured {
                        description: Arc::from(CLAUDE_WRITE_DESCRIPTION),
                        schema: object_schema(
                            json!({
                                "file_path": {"type": "string"},
                                "content": {"type": "string"}
                            }),
                            &["file_path", "content"],
                        ),
                    },
                    ToolProjectionLoweringV2::ClaudeWrite,
                ),
            ],
        })
    }

    fn verify(
        &self,
        presentation: CodingPresentation,
        registry: &FrozenToolRegistry,
    ) -> Result<(), ToolRuntimeError> {
        if presentation != CodingPresentation::Codex {
            return Ok(());
        }
        let name = tool_name(
            CODEX_PATCH_ENVELOPE_CANONICAL_NAME,
            "Codex patch envelope name is invalid",
        )?;
        let identity = registry.identity(&name).ok_or_else(|| {
            ToolRuntimeError::new(
                ToolRuntimeErrorKind::Registration,
                "Codex patch envelope capability is not registered",
            )
        })?;
        if identity.tool_id().as_str() != CODEX_PATCH_ENVELOPE_REGISTERED_TOOL_ID {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::Registration,
                "Codex patch envelope identity is not enrolled",
            ));
        }
        Ok(())
    }

    fn pins(&self) -> &'static [PromptPin] {
        FILES_PROMPT_PINS
    }
}

/// Pi 1.1.0's `read`, `edit` and `write`, with pi's schemas and property
/// descriptions; `read` is text only, as pi 1.1.0's is.
fn pi_menu(
    read_file: &plakous_model::ToolName,
    apply_patch: &plakous_model::ToolName,
) -> Vec<MenuEntry> {
    let path = |verb: &str| {
        json!({
            "type": "string",
            "description": format!("Path to the file to {verb} (relative or absolute)")
        })
    };
    vec![
        MenuEntry::new(
            "read",
            vec![MenuBinding::by_effect(read_file.clone())],
            MenuShape::Structured {
                description: Arc::from(PI_READ_DESCRIPTION),
                schema: object_schema(
                    json!({
                        "path": path("read"),
                        "offset": {
                            "type": "integer",
                            "minimum": 1,
                            "description": "Line number to start reading from (1-indexed)"
                        },
                        "limit": {
                            "type": "integer",
                            "minimum": 1,
                            "description": "Maximum number of lines to read"
                        }
                    }),
                    &["path"],
                ),
            },
            ToolProjectionLoweringV2::PiRead,
        ),
        MenuEntry::new(
            "edit",
            vec![MenuBinding::by_effect(apply_patch.clone())],
            MenuShape::Structured {
                description: Arc::from(PI_EDIT_DESCRIPTION),
                schema: object_schema(
                    json!({
                        "path": path("edit"),
                        "edits": {
                            "type": "array",
                            "minItems": 1,
                            "description": "One or more targeted replacements. Each edit is matched against the original file, not incrementally. Do not include overlapping or nested edits. If two changes touch the same block or nearby lines, merge them into one edit instead.",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "oldText": {
                                        "type": "string",
                                        "description": "Exact text for one targeted replacement. It must be unique in the original file and must not overlap with any other edits[].oldText in the same call."
                                    },
                                    "newText": {
                                        "type": "string",
                                        "description": "Replacement text for this targeted edit."
                                    }
                                },
                                "required": ["oldText", "newText"],
                                "additionalProperties": false
                            }
                        }
                    }),
                    &["path", "edits"],
                ),
            },
            ToolProjectionLoweringV2::PiEdit,
        ),
        MenuEntry::new(
            "write",
            vec![MenuBinding::by_effect(apply_patch.clone())],
            MenuShape::Structured {
                description: Arc::from(PI_WRITE_DESCRIPTION),
                schema: object_schema(
                    json!({
                        "path": path("write"),
                        "content": {"type": "string", "description": "Content to write to the file"}
                    }),
                    &["path", "content"],
                ),
            },
            ToolProjectionLoweringV2::PiWrite,
        ),
    ]
}

/// The Codex freeform `apply_patch` envelope, shared by the Direct menu and the
/// Codex Code Mode outer surface.
/// The Codex freeform `apply_patch` menu entry, for the compositions that
/// re-raise it inside a cell.
///
/// # Errors
///
/// Returns an invalid-definition error when a built-in bound cannot be frozen.
pub fn codex_patch_envelope_entry() -> Result<MenuEntry, ToolRuntimeError> {
    let envelope = tool_name(
        CODEX_PATCH_ENVELOPE_CANONICAL_NAME,
        "Codex patch envelope name is invalid",
    )?;
    Ok(MenuEntry::new(
        "apply_patch",
        vec![MenuBinding::exact(
            CanonicalProcessOperationV2::Start,
            envelope,
        )],
        MenuShape::CustomUtf8 {
            description: Arc::from(PLAKOUS_CODEX_APPLY_PATCH_FREEFORM_DESCRIPTION),
            input: Utf8TextContractV1::new(CODEX_PATCH_ENVELOPE_MAX_BYTES).map_err(|error| {
                ToolRuntimeError::from_sensitive_source(
                    ToolRuntimeErrorKind::InvalidDefinition,
                    "Codex patch envelope UTF-8 contract is invalid",
                    &error,
                )
            })?,
            format: ProjectedCustomToolFormatV2::lark(CODEX_APPLY_PATCH_GRAMMAR_V1).map_err(
                |error| {
                    ToolRuntimeError::from_sensitive_source(
                        ToolRuntimeErrorKind::InvalidDefinition,
                        "Codex patch envelope grammar is invalid",
                        &error,
                    )
                },
            )?,
        },
        ToolProjectionLoweringV2::CodexApplyPatch,
    ))
}

/// Every model-visible string the files pack owns.
static FILES_PROMPT_PINS: &[PromptPin] = &[
    PromptPin::new(
        "files.sdk.edit_files",
        text::PLAKOUS_TOOL_EDIT_FILES_DESCRIPTION,
        "dcee2a43728f9bcb657cbb4df27ad6b25b0c7bdc390d7acbcef4761528d6b800",
    ),
    PromptPin::new(
        "files.write_unread",
        PLAKOUS_TOOL_WRITE_UNREAD,
        "6f381d28c4af7b70662d3eda2eedc8248ab58ce29e633f6d5c1193de735bd2d6",
    ),
    PromptPin::new(
        "files.write_stale",
        PLAKOUS_TOOL_WRITE_STALE,
        "54fad0c80ee7e20e1ebfee6a254a6e667352e1328a89a34cb26b7c326eb3c35d",
    ),
    PromptPin::new(
        "files.write_binary",
        text::PLAKOUS_TOOL_WRITE_BINARY,
        "8c67f8b56fa9ef3c6e14ff47384f514bae1f807c719685785dde21d04d4373ec",
    ),
    PromptPin::new(
        "files.write_too_large",
        text::PLAKOUS_TOOL_WRITE_TOO_LARGE,
        "64d48655f955e61a3c6a545e932c8e4af794145877758bb8e71b29151ef581a4",
    ),
    PromptPin::new(
        "files.write_directory",
        text::PLAKOUS_TOOL_WRITE_DIRECTORY,
        "beaa3b432c1811942b4722491464658d859ab9fcfecbcf74dd84e5b95ebf75cf",
    ),
    PromptPin::new(
        "files.write_parent_not_directory",
        text::PLAKOUS_TOOL_WRITE_PARENT_NOT_DIRECTORY,
        "b25322321a815bdc22c5c95cc2657818e641a470a308e34b4583f1c2d9b45dad",
    ),
    PromptPin::new(
        "files.read_binary",
        PLAKOUS_TOOL_READ_BINARY,
        "622c5d46b0083489e29bb3dce008f3c89d1604a5be8398713dff8f2d047c11d3",
    ),
    PromptPin::new(
        "files.read_file",
        PLAKOUS_TOOL_READ_FILE_DESCRIPTION,
        "9de10e016d6add1cd350fa8e8d40287c6f18d796778bcd35b72c5fa0f2985906",
    ),
    PromptPin::new(
        "files.apply_patch",
        PLAKOUS_TOOL_APPLY_PATCH_DESCRIPTION,
        "b11431ed839144bb094d750542881fdc4b1527cf85d6d41bede82041ee58a70a",
    ),
    PromptPin::new(
        "files.inspect_image",
        PLAKOUS_TOOL_INSPECT_IMAGE_DESCRIPTION,
        "ae28b85d0568f2003cce69f9f1599fc5845a58f5c825138495002d5f783ae7f2",
    ),
    PromptPin::new(
        "files.codex.apply_patch",
        PLAKOUS_CODEX_APPLY_PATCH_FREEFORM_DESCRIPTION,
        "0c5947d5946e9c81534c17cc8b8798c36532c545639c7245b6cb6d1fdd2ba605",
    ),
    PromptPin::new(
        "files.grok.read_file",
        PLAKOUS_GROK_READ_DESCRIPTION,
        "d68418264094b82ce787a03e6772c13924cdc4857d332481fd53436e1dacee5e",
    ),
    PromptPin::new(
        "files.grok.read_file.cell",
        PLAKOUS_GROK_READ_CELL_DESCRIPTION,
        "f74959e098fe21179a684b49f651c606072ef48d89f1b3b113ae525f77723e7a",
    ),
    PromptPin::new(
        "files.grok.search_replace",
        PLAKOUS_GROK_SEARCH_REPLACE_DESCRIPTION,
        "8b972d4f156e7265360579d25dd499b7ebbf4656b3a4515a5f71906a16e6457c",
    ),
    PromptPin::new(
        "files.grok.write",
        PLAKOUS_GROK_WRITE_DESCRIPTION,
        "192f70146da8b70c667e19d42a79c35c20883db243864eaba3b5970ea3918c5d",
    ),
    PromptPin::new(
        "files.pi.read",
        PI_READ_DESCRIPTION,
        "18782c69b7f8d7cd197a8128d81f7f6d4640e47ac2aa8fe381f4e6698e959bce",
    ),
    PromptPin::new(
        "files.pi.edit",
        PI_EDIT_DESCRIPTION,
        "981680af66987fad9bee7feefb62d56c4793c895f2e618561d19bdd3173e899b",
    ),
    PromptPin::new(
        "files.pi.write",
        PI_WRITE_DESCRIPTION,
        "8dded374dff3a1852971fac0a1493f91b04dea942d1fb21521a6ea7620aed093",
    ),
    PromptPin::new(
        "files.claude.Read",
        CLAUDE_READ_DESCRIPTION,
        "1e1631b8491a6dbb1613ced7a6512092f0ca252d3c1bdc2b6a8b3e5eea4d980f",
    ),
    PromptPin::new(
        "files.codex.view_image",
        VIEW_IMAGE_DESCRIPTION,
        "9f28a4e8ad63309a430c2979ab77f1d38886c3ff22c591230a06cc874dad45d3",
    ),
    PromptPin::new(
        "files.claude.Edit",
        CLAUDE_EDIT_DESCRIPTION,
        "0bd837f20a34eb230218acf595d6ce6cf6693dc8c4a4a1eada05df70ca2d42f8",
    ),
    PromptPin::new(
        "files.claude.Write",
        CLAUDE_WRITE_DESCRIPTION,
        "da2d0dd9555cc899559b2acb7026767964661a1c57cae321aee22ab252e35ef4",
    ),
];
