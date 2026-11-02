use super::{
    Arc, Deserialize, MAX_STANDARD_SEARCH_FILES, RepositoryQuery, RepositoryQueryLimits,
    STANDARD_SEARCH_FILE_BYTES, STANDARD_SEARCH_RESULT_BYTES, STANDARD_SEARCH_TREE_ENTRIES,
    ToolContext, ToolDefinition, ToolEffect, ToolExecutor, ToolFuture, ToolInvocation, ToolOutcome,
    ToolRuntimeError, backend_runtime_error, decode_arguments, deserialize_optional_semantic_u64,
    json, object_schema, operation_control, query_root, standard_definition,
};
use std::path::PathBuf;

pub(super) struct ListEntriesTool;

impl ListEntriesTool {
    pub(super) fn new() -> Result<(ToolDefinition, Self), ToolRuntimeError> {
        Ok((
            standard_definition(
                plakous_tools::LIST_ENTRIES_CAPABILITY_NAME,
                "List repository files and directories using workspace-root ignore rules. depth defaults to 1; glob is relative to path. Symlinks are not followed.",
                object_schema(
                    json!({
                        "path":{"type":"string"}, "glob":{"type":"string"},
                        "depth":{"type":"integer","minimum":1,"maximum":128}
                    }),
                    &["path"],
                ),
                plakous_model::ToolConcurrency::Parallel,
                ToolEffect::ReadOnly,
            )?,
            Self,
        ))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    path: PathBuf,
    glob: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_semantic_u64")]
    depth: Option<u64>,
}

impl ToolExecutor for ListEntriesTool {
    fn execute(self: Arc<Self>, invocation: ToolInvocation, context: ToolContext) -> ToolFuture {
        Box::pin(async move {
            context.check_cancelled()?;
            let args: ListArgs = decode_arguments(&invocation)?;
            let limits = RepositoryQueryLimits::new(
                MAX_STANDARD_SEARCH_FILES,
                STANDARD_SEARCH_TREE_ENTRIES,
                STANDARD_SEARCH_FILE_BYTES,
                STANDARD_SEARCH_FILE_BYTES,
                super::MAX_STANDARD_SEARCH_MATCHES,
                1,
                STANDARD_SEARCH_RESULT_BYTES - 1024,
            )
            .map_err(backend_runtime_error)?;
            let query = RepositoryQuery::list(
                query_root(&context, &args.path)?,
                args.glob,
                usize::try_from(args.depth.unwrap_or(1)).unwrap_or(usize::MAX),
                limits,
            )
            .map_err(backend_runtime_error)?;
            let result = context
                .environment()
                .query_repository(query, operation_control(&context))
                .await
                .map_err(backend_runtime_error)?;
            let entries: Vec<_> = result
                .matches()
                .iter()
                .map(|entry| {
                    json!({
                        "path":entry.path().to_string_lossy(),
                        "kind":if entry.is_directory() { "directory" } else { "file" }
                    })
                })
                .collect();
            let outcome = ToolOutcome::success_text(
                json!({
                    "entries":entries, "incomplete":result.incomplete(),
                    "entries_skipped":result.entries_skipped(),
                    "stop_reason":result.stop_reason().map(|reason| format!("{reason:?}"))
                })
                .to_string(),
            )?;
            if !invocation.want_value() {
                return Ok(outcome);
            }
            let mut listed = args.path.display().to_string();
            listed.shrink_to_fit();
            let count = super::bounded_value_count::<plakous_tools::ListedEntry>(
                listed.capacity(),
                result
                    .matches()
                    .iter()
                    .map(|entry| entry.path().as_os_str().len()),
            );
            let entries = result
                .matches()
                .iter()
                .take(count)
                .map(|entry| plakous_tools::ListedEntry {
                    path: entry.path().to_string_lossy().into_owned(),
                    kind: if entry.is_directory() {
                        plakous_tools::ListedEntryKind::Directory
                    } else {
                        plakous_tools::ListedEntryKind::File
                    },
                    size: entry.entry_size(),
                })
                .collect();
            outcome.with_value(plakous_tools::ExecutionValue::Listing(
                plakous_tools::ListingValue {
                    path: listed,
                    entries,
                    entries_cut: result.matches().len().saturating_sub(count) as u64,
                    incomplete: result.incomplete(),
                },
            ))
        })
    }
}
