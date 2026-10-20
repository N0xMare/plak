//! Runs one validated SDK call: each I/O function becomes one or more
//! canonical nested children through the driver, or a named-state operation,
//! and its typed execution value becomes the plain-data return value.

use std::sync::Arc;

use monty_types::{ExcType, MontyException, MontyObject};
use plakous_agent::internal::NamedStateError;
use plakous_tools::ExecutionValue;
use serde_json::{Value, json};

use super::{
    args::{HostCall, ReadTarget},
    evidence::Evidence,
    process,
    scope::{Ack, CellScope, NestedError, NestedResult},
    values::{self, FileFields},
};

/// File extensions `ctx.read` reads through `inspect_image`.
const IMAGE_EXTENSIONS: [&str; 5] = ["png", "jpg", "jpeg", "gif", "webp"];

/// What one host call resolves its future to. `ack` acknowledges the one
/// nested result the cell receives directly, after it reaches the cell.
pub(super) struct HostReply {
    pub(super) value: Result<MontyObject, MontyException>,
    pub(super) ack: Option<Ack>,
}

impl HostReply {
    fn done(value: Result<MontyObject, MontyException>) -> Self {
        Self { value, ack: None }
    }
}

pub(super) fn runtime_error(message: impl Into<String>) -> MontyException {
    MontyException::new(ExcType::RuntimeError, Some(message.into()))
}

pub(super) fn unavailable(id: &str) -> MontyException {
    runtime_error(format!("reference not available: {id}"))
}

pub(super) fn nested_error(error: NestedError) -> MontyException {
    match error {
        NestedError::Invalid(message) => MontyException::new(ExcType::ValueError, Some(message)),
        NestedError::Failed(message) => runtime_error(message),
        NestedError::Stopped => runtime_error("the cell is stopping"),
    }
}

pub(super) fn id_error(error: getrandom::Error) -> MontyException {
    runtime_error(format!("could not issue an id: {error}"))
}

/// The typed value of a committed child; a child without one is a definite
/// failure whose canonical text is its diagnostic.
pub(super) fn typed<'a>(
    result: &'a NestedResult,
    tool: &str,
) -> Result<&'a ExecutionValue, MontyException> {
    result.execution.as_deref().ok_or_else(|| {
        let text = match &result.value {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        runtime_error(crate::sanitize_nested_error(&format!("{tool}: {text}")))
    })
}

fn unexpected(tool: &str) -> MontyException {
    runtime_error(format!("{tool} returned an unexpected result"))
}

/// Executes one call to completion.
pub(super) async fn execute(call: HostCall, scope: Arc<CellScope>) -> HostReply {
    match call {
        HostCall::Read { target, lines } => read(&scope, target, lines).await,
        HostCall::List { path, glob, depth } => {
            let mut input = json!({"path": path, "depth": depth});
            if let Some(glob) = glob {
                input["glob"] = json!(glob);
            }
            single(&scope, "list_entries", input, |value| match value {
                ExecutionValue::Listing(listing) => Some(values::listing(listing)),
                _ => None,
            })
            .await
        }
        HostCall::Grep {
            pattern,
            path,
            glob,
        } => {
            let mut input = json!({"query": pattern, "path": path, "use_regex": true});
            if let Some(glob) = glob {
                input["filename_glob"] = json!(glob);
            }
            single(&scope, "search_repository", input, |value| match value {
                ExecutionValue::SearchHits(hits) => Some(values::hits(hits)),
                _ => None,
            })
            .await
        }
        HostCall::Apply { ops } => {
            single(
                &scope,
                "edit_files",
                json!({"ops": ops}),
                |value| match value {
                    ExecutionValue::FileChanges(changes) => Some(values::changes(changes)),
                    _ => None,
                },
            )
            .await
        }
        HostCall::PlanUpdate { steps } => {
            single(
                &scope,
                "update_plan",
                json!({"plan": steps}),
                |value| match value {
                    ExecutionValue::Plan(plan) => Some(MontyObject::int(
                        i64::try_from(plan.revision).unwrap_or(i64::MAX),
                    )),
                    _ => None,
                },
            )
            .await
        }
        HostCall::StateGet { name, default } => HostReply::done(state_get(&scope, &name, default)),
        HostCall::StatePut { name, value } => {
            let tainted = !scope.untrusted_sources().is_empty();
            HostReply::done(
                scope
                    .state
                    .put(&name, value, tainted)
                    .await
                    .map(|()| MontyObject::none())
                    .map_err(state_error),
            )
        }
        HostCall::StateList => HostReply::done(Ok(MontyObject::list(
            scope
                .state
                .list()
                .iter()
                .map(|slot| values::slot(&slot.name, slot.bytes, slot.tainted)),
        ))),
        HostCall::StateDrop { name } => HostReply::done(
            scope
                .state
                .drop_name(&name)
                .await
                .map(MontyObject::bool)
                .map_err(state_error),
        ),
        other => HostReply::done(process::execute(other, &scope).await),
    }
}

/// One child whose typed value converts directly; its acknowledgement waits
/// for delivery to the cell.
async fn single(
    scope: &CellScope,
    tool: &str,
    input: Value,
    convert: impl FnOnce(&ExecutionValue) -> Option<MontyObject>,
) -> HostReply {
    let result = match scope.call(tool, input).await {
        Ok(result) => result,
        Err(error) => return HostReply::done(Err(nested_error(error))),
    };
    let value =
        typed(&result, tool).and_then(|value| convert(value).ok_or_else(|| unexpected(tool)));
    HostReply {
        value,
        ack: Some(result.ack),
    }
}

fn state_error(error: NamedStateError) -> MontyException {
    match error {
        NamedStateError::Store(_) => runtime_error(error.to_string()),
        other => MontyException::new(ExcType::ValueError, Some(other.to_string())),
    }
}

fn state_get(scope: &CellScope, name: &str, default: Value) -> Result<MontyObject, MontyException> {
    let entry = scope.state.get(name).map_err(state_error)?;
    let value = match entry {
        None => default,
        Some(entry) => {
            if entry.tainted {
                scope.mark_untrusted(format!("state:{name}"));
            }
            entry.value
        }
    };
    crate::codec::json_to_monty(&value).map_err(runtime_error)
}

fn is_image_path(path: &str) -> bool {
    path.rsplit_once('.').is_some_and(|(_, extension)| {
        IMAGE_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str())
    })
}

fn read_input(path: &str, lines: Option<(u64, u64)>) -> Value {
    let mut input = json!({"path": path});
    if let Some((first, last)) = lines {
        input["offset"] = json!(first);
        input["limit"] = json!(last - first + 1);
    }
    input
}

async fn read(scope: &CellScope, target: ReadTarget, lines: Option<(u64, u64)>) -> HostReply {
    match target {
        ReadTarget::Path(path) if is_image_path(&path) => inspect_image(scope, &path).await,
        ReadTarget::Path(path) => read_file(scope, &path, lines).await,
        ReadTarget::Reference(id) => match scope.evidence.resolve(&id) {
            None => HostReply::done(Err(unavailable(&id))),
            Some(Evidence::ProcessOutput(path)) => read_file(scope, &path, lines).await,
            Some(Evidence::CellOutput(text)) => {
                HostReply::done(Ok(cell_output_page(&id, &text, lines)))
            }
        },
    }
}

async fn read_file(scope: &CellScope, path: &str, lines: Option<(u64, u64)>) -> HostReply {
    single(
        scope,
        "read_file",
        read_input(path, lines),
        |value| match value {
            ExecutionValue::FileRead(page) => Some(values::file_read(page, path)),
            _ => None,
        },
    )
    .await
}

async fn inspect_image(scope: &CellScope, path: &str) -> HostReply {
    let result = match scope.call("inspect_image", json!({"path": path})).await {
        Ok(result) => result,
        Err(error) => return HostReply::done(Err(nested_error(error))),
    };
    let value = typed(&result, "inspect_image").and_then(|value| {
        let ExecutionValue::Image(image) = value else {
            return Err(unexpected("inspect_image"));
        };
        let id = super::evidence::new_id().map_err(id_error)?;
        let object = values::image(&id, image);
        scope.issue_image(id, image.clone());
        Ok(values::file(FileFields {
            path,
            text: "",
            first_line: 1,
            lines: 0,
            eof: true,
            revision: "",
            image: Some(object),
            omitted: Vec::new(),
        }))
    });
    HostReply {
        value,
        ack: Some(result.ack),
    }
}

/// A page of a retained cell output, read without a nested call.
fn cell_output_page(id: &str, text: &str, lines: Option<(u64, u64)>) -> MontyObject {
    let all = text.split_inclusive('\n').collect::<Vec<_>>();
    let (first, last) = lines.unwrap_or((1, all.len().max(1) as u64));
    let start = usize::try_from(first - 1)
        .unwrap_or(usize::MAX)
        .min(all.len());
    let end = usize::try_from(last).unwrap_or(usize::MAX).min(all.len());
    let page = all[start..end.max(start)].concat();
    values::file(FileFields {
        path: &format!("Ref(\"{id}\")"),
        text: &page,
        first_line: first,
        lines: (end.max(start) - start) as u64,
        eof: end >= all.len(),
        revision: "",
        image: None,
        omitted: Vec::new(),
    })
}
