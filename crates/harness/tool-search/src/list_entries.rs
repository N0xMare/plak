//! The SDK-only `list_entries` capability: a bounded, deterministic,
//! `.gitignore`-aware listing with a glob and a depth.

use std::fmt::Write as _;
use std::sync::Arc;

use plakous_model::{ToolConcurrency, ToolEffect};
use plakous_tools::{
    ExecutionValue, ListedEntry, ListedEntryKind, ListingValue, RegisteredToolId, ToolContext,
    ToolExecutor, ToolExecutorBindingIdentity, ToolExecutorId, ToolFuture, ToolInvocation,
    ToolOutcome, ToolRegistration, ToolRuntimeError, ToolRuntimeErrorKind, decode_arguments,
    standard_definition,
};
use serde::Deserialize;
use serde_json::json;

/// Canonical name of the SDK-only listing.
pub const LIST_ENTRIES_NAME: &str = "list_entries";
/// Registered tool and executor id of the SDK-only listing.
pub const LIST_ENTRIES_REGISTERED_TOOL_ID: &str = "plakous.list-entries.v1";

/// Entries one listing returns.
const LIST_ENTRIES_MAX: usize = 2_000;
/// Entries one walk visits before it stops and reports itself incomplete.
const LIST_ENTRIES_VISITS: usize = 10_000;
/// The deepest listing a call may ask for.
const LIST_ENTRIES_MAX_DEPTH: u64 = 16;

/// The SDK-only `list_entries` registration. It is nested-eligible and
/// belongs to no standard pack; the composition that presents it registers it.
///
/// # Errors
///
/// Returns an invalid-definition error if the registration cannot be constructed.
pub fn list_entries_registration() -> Result<ToolRegistration, ToolRuntimeError> {
    let definition = standard_definition(
        LIST_ENTRIES_NAME,
        "List entries under a workspace directory, honouring .gitignore and skipping hidden names, to a depth, optionally filtered by a glob over the relative path.",
        plakous_tools::object_schema(
            json!({
                "path": {"type": "string"},
                "glob": {"type": "string"},
                "depth": {"type": "integer", "minimum": 1, "maximum": LIST_ENTRIES_MAX_DEPTH}
            }),
            &["path", "depth"],
        ),
        ToolConcurrency::Parallel,
        ToolEffect::ReadOnly,
    )?;
    Ok(ToolRegistration::new(
        RegisteredToolId::new(LIST_ENTRIES_REGISTERED_TOOL_ID)?,
        ToolExecutorId::new(LIST_ENTRIES_REGISTERED_TOOL_ID)?,
        ToolExecutorBindingIdentity::new(1, &[b"list-entries", b"gitignore-walk"])?,
        definition,
        Arc::new(ListEntriesTool),
    )
    .nested_eligible())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListEntriesArgs {
    path: String,
    glob: Option<String>,
    depth: u64,
}

struct ListEntriesTool;

impl ToolExecutor for ListEntriesTool {
    fn execute(self: Arc<Self>, invocation: ToolInvocation, context: ToolContext) -> ToolFuture {
        Box::pin(async move {
            context.check_cancelled()?;
            let args: ListEntriesArgs = decode_arguments(&invocation)?;
            list_entries(&context, &args)
        })
    }
}

fn invalid(message: &'static str) -> ToolRuntimeError {
    ToolRuntimeError::new(ToolRuntimeErrorKind::InvalidArguments, message)
}

fn list_entries(
    context: &ToolContext,
    args: &ListEntriesArgs,
) -> Result<ToolOutcome, ToolRuntimeError> {
    if !(1..=LIST_ENTRIES_MAX_DEPTH).contains(&args.depth) {
        return Err(invalid("list depth is out of range"));
    }
    let root = context
        .workspace()
        .resolve_existing(std::path::Path::new(&args.path))?;
    if !root.is_dir() {
        return ToolOutcome::failure_text("list target is not a directory");
    }
    let glob = args
        .glob
        .as_deref()
        .map(|glob| {
            globset::Glob::new(glob)
                .map(|glob| glob.compile_matcher())
                .map_err(|_| invalid("list glob is invalid"))
        })
        .transpose()?;
    let walker = ignore::WalkBuilder::new(&root)
        .max_depth(usize::try_from(args.depth).ok())
        .follow_links(false)
        .hidden(true)
        .require_git(false)
        .sort_by_file_name(Ord::cmp)
        .build();
    let mut entries = Vec::new();
    let mut entries_cut = 0_u64;
    let mut incomplete = false;
    for (visited, entry) in walker.enumerate() {
        if visited >= LIST_ENTRIES_VISITS {
            incomplete = true;
            break;
        }
        let Ok(entry) = entry else {
            incomplete = true;
            continue;
        };
        if entry.depth() == 0 {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(&root) else {
            continue;
        };
        if glob.as_ref().is_some_and(|glob| !glob.is_match(relative)) {
            continue;
        }
        if entries.len() == LIST_ENTRIES_MAX {
            entries_cut += 1;
            continue;
        }
        let file_type = entry.file_type();
        let kind = match file_type {
            Some(kind) if kind.is_symlink() => ListedEntryKind::Link,
            Some(kind) if kind.is_dir() => ListedEntryKind::Directory,
            Some(kind) if kind.is_file() => ListedEntryKind::File,
            _ => ListedEntryKind::Other,
        };
        let size = if kind == ListedEntryKind::File {
            entry.metadata().map_or(0, |metadata| metadata.len())
        } else {
            0
        };
        entries.push(ListedEntry {
            path: relative.to_string_lossy().into_owned(),
            kind,
            size,
        });
    }
    let mut text = String::new();
    if entries_cut != 0 || incomplete {
        let _ = writeln!(
            text,
            "[listed {} entries; {entries_cut} more omitted; incomplete={incomplete}]",
            entries.len()
        );
    }
    for entry in &entries {
        let _ = match entry.kind {
            ListedEntryKind::Directory => writeln!(text, "{}/", entry.path),
            ListedEntryKind::File => writeln!(text, "{}\t{}", entry.path, entry.size),
            ListedEntryKind::Link => writeln!(text, "{}@", entry.path),
            ListedEntryKind::Other => writeln!(text, "{}", entry.path),
        };
    }
    if text.is_empty() {
        text.push_str("(no entries)");
    }
    Ok(
        ToolOutcome::success_text(text)?.with_execution_value(ExecutionValue::Listing(
            ListingValue {
                entries,
                entries_cut,
                incomplete,
            },
        )),
    )
}
