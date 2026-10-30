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

pub use text::{
    PLAKOUS_CODEX_APPLY_PATCH_FREEFORM_DESCRIPTION, PLAKOUS_GROK_READ_DESCRIPTION,
    PLAKOUS_GROK_SEARCH_REPLACE_DESCRIPTION, PLAKOUS_GROK_WRITE_DESCRIPTION,
    PLAKOUS_TOOL_APPLY_PATCH_DESCRIPTION, PLAKOUS_TOOL_APPLY_PATCH_REQUIRES_INPUT,
    PLAKOUS_TOOL_CREATE_PATCH_NONEMPTY, PLAKOUS_TOOL_DELETE_PATCH_NONEMPTY,
    PLAKOUS_TOOL_FILE_CHANGES_FORMAT, PLAKOUS_TOOL_IMAGE_TOO_LARGE,
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
use plakous_tools::{APPLY_PATCH_CAPABILITY_NAME, READ_FILE_CAPABILITY_NAME};
use plakous_tools::{
    CanonicalProcessOperationV2, FrozenToolRegistry, RegisteredToolId, ToolExecutorBindingIdentity,
    ToolExecutorId, ToolProjectionLoweringV2, ToolRegistration,
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
const PI_EDIT_DESCRIPTION: &str = "Edit a single file using exact text replacement. Every edits[].oldText must match a unique, non-overlapping region of the original file. If two changes affect the same block or nearby lines, merge them into one edit instead of emitting overlapping edits. Do not include large unchanged regions just to connect distant changes. The edits apply together or not at all.";
const PI_WRITE_DESCRIPTION: &str = "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates parent directories.";
const CLAUDE_READ_DESCRIPTION: &str = "Reads a file from the local filesystem.\n\n- Reads up to 2000 lines by default.\n- If you already know which part of the file to read, only read that part. This is important for larger files/context efficiency.\n- `offset` is the 1-indexed first line (omit to start at line 1). `limit` is how many lines to return (default 2000, max 2000).\n- The result is cat -n text: a 6-character file line number, a tab, then the line. Numbers are the file's real line numbers (offset 10 starts at line 10).\n- If truncated, a last line `... more lines; next_offset=N` means call again with that offset.\n- Lines longer than 2000 characters are clipped.\n- Text files only: binary files, images and PDFs are refused (there is no `pages` parameter).\n- `file_path` is an absolute path under the workspace root, a workspace-relative path without `..`, or exactly a path a Bash result or completion notice named on its `full output:` line; other paths are refused, as is a `full output:` path issued by an earlier process (before a restart or reload) or another agent.\n- A read of an output file whose writing failed ends with `[output file incomplete: write failed after N bytes]`; the file stops there.\n- An empty file succeeds with an empty body, missing file/directory returns an error.";
const CLAUDE_EDIT_DESCRIPTION: &str = "Performs exact string replacement in a file.\n\n- `old_string` must be non-empty and match the file exactly, including indentation, and be unique — the edit fails otherwise.\n- `replace_all: true` replaces every occurrence instead.\n- Use Write to create a new file.";
const CLAUDE_WRITE_DESCRIPTION: &str = "Writes a file to the local filesystem.\n\n- Creates the file, or overwrites it when it exists. Missing parent directories are created.\n- Prefer Edit for partial changes; read an existing file before overwriting it.";

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
#[derive(Clone, Copy, Debug, Default)]
pub struct FilesToolPack;

impl FilesToolPack {
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
            standard_registration("read-file", ReadFileTool::new())?,
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
            CodingPresentation::Codex => vec![codex_patch_envelope_entry()?],
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
        "bf7713d70627a8f5bae6fcc666712b1abb873a7a45fa705cc909ec9cf2cbb53c",
    ),
    PromptPin::new(
        "files.write_unread",
        PLAKOUS_TOOL_WRITE_UNREAD,
        "25c55aadb1dfd9d16eaa25393a9ae3169bba5d743d89395ffe14b19d9d3f8e49",
    ),
    PromptPin::new(
        "files.write_stale",
        PLAKOUS_TOOL_WRITE_STALE,
        "54fad0c80ee7e20e1ebfee6a254a6e667352e1328a89a34cb26b7c326eb3c35d",
    ),
    PromptPin::new(
        "files.changes_format",
        PLAKOUS_TOOL_FILE_CHANGES_FORMAT,
        "0dced8c198dfdcd62fd2cab8a513ddc6efdd2a0ecf4389097d5f51f7f0d07c86",
    ),
    PromptPin::new(
        "files.read_binary",
        PLAKOUS_TOOL_READ_BINARY,
        "622c5d46b0083489e29bb3dce008f3c89d1604a5be8398713dff8f2d047c11d3",
    ),
    PromptPin::new(
        "files.read_file",
        PLAKOUS_TOOL_READ_FILE_DESCRIPTION,
        "f95b2f3663ceed6a005e0b3d37899fff9c0d18d2b300b1d121677c879d27bdd9",
    ),
    PromptPin::new(
        "files.apply_patch",
        PLAKOUS_TOOL_APPLY_PATCH_DESCRIPTION,
        "ff1efc37d9bb186332d8d5ea81fde8a4b87bf80eb4d4ce413667617129cffb0c",
    ),
    PromptPin::new(
        "files.inspect_image",
        PLAKOUS_TOOL_INSPECT_IMAGE_DESCRIPTION,
        "ae28b85d0568f2003cce69f9f1599fc5845a58f5c825138495002d5f783ae7f2",
    ),
    PromptPin::new(
        "files.codex.apply_patch",
        PLAKOUS_CODEX_APPLY_PATCH_FREEFORM_DESCRIPTION,
        "0b70e9f33c3f169a4189336e2af8180b566a736274703d48fd75d04ba3dc0af9",
    ),
    PromptPin::new(
        "files.grok.read_file",
        PLAKOUS_GROK_READ_DESCRIPTION,
        "78bd813467dc5832c582fd2e249ff0ea15457b6541b82dbe8b5397ff221ae181",
    ),
    PromptPin::new(
        "files.grok.search_replace",
        PLAKOUS_GROK_SEARCH_REPLACE_DESCRIPTION,
        "eb2fc3d2bb93a3fb2022c78bb646b7f7c9a4f44455abbece931ed2c7041e76b9",
    ),
    PromptPin::new(
        "files.grok.write",
        PLAKOUS_GROK_WRITE_DESCRIPTION,
        "702c05152d0d708e25c97b1f89528dfa5afa20b58882e6a4d77ee4fc7550b34b",
    ),
    PromptPin::new(
        "files.pi.read",
        PI_READ_DESCRIPTION,
        "18782c69b7f8d7cd197a8128d81f7f6d4640e47ac2aa8fe381f4e6698e959bce",
    ),
    PromptPin::new(
        "files.pi.edit",
        PI_EDIT_DESCRIPTION,
        "409f9e32af7d38aac3a09b4f4303953cf9821bdb02ab81c2eca43c68ef4e536d",
    ),
    PromptPin::new(
        "files.pi.write",
        PI_WRITE_DESCRIPTION,
        "c7d18538f7549a0cea0b1f47ae7309066be2a56c8217530eba35efdb45e94971",
    ),
    PromptPin::new(
        "files.claude.Read",
        CLAUDE_READ_DESCRIPTION,
        "d406130f15c7bbd6908b7e74d4e564b063babbce65e4aa6c35602f186e377b13",
    ),
    PromptPin::new(
        "files.claude.Edit",
        CLAUDE_EDIT_DESCRIPTION,
        "9ec37949f13550d1f740faca439dea0d378bad2d61eb64ce6d5ec64c9e14960e",
    ),
    PromptPin::new(
        "files.claude.Write",
        CLAUDE_WRITE_DESCRIPTION,
        "8af8c3181d6045cc674cdbb772ff58478b7119c6eba56023746cf068927a54ea",
    ),
];
