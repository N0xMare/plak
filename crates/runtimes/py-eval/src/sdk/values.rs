//! Plain-data SDK values: typed execution values become the named tuples the
//! stubs declare, and named-state values cross as strict JSON. No value the
//! SDK hands a cell is a class instance or a host-backed object.

use monty_types::{
    MontyObject, ObjectRef,
    unstable::{self, MontyNode},
};
use plakous_tools::{
    AnswerValue, FileChangeAction, FileChangesValue, FileReadValue, ImageValue, ListedEntryKind,
    ListingValue, PageValue, SearchHitsValue,
};
use serde_json::{Map, Value};

/// Deepest JSON nesting a named-state value may have.
const MAX_STATE_DEPTH: usize = 128;

fn text(value: &str) -> MontyObject {
    MontyObject::string(value)
}

fn int(value: u64) -> MontyObject {
    MontyObject::int(i64::try_from(value).unwrap_or(i64::MAX))
}

fn optional_int(value: Option<i64>) -> MontyObject {
    value.map_or_else(MontyObject::none, MontyObject::int)
}

fn tuple(name: &str, fields: &[&str], values: Vec<MontyObject>) -> MontyObject {
    MontyObject::named_tuple(name, fields.iter().copied(), values)
}

/// `Ref(id)`.
pub(super) fn reference(id: &str) -> MontyObject {
    tuple("Ref", &["id"], vec![text(id)])
}

/// `Omission(what, count, bytes, full)`.
pub(super) fn omission(what: &str, count: u64, bytes: u64, full: Option<&str>) -> MontyObject {
    tuple(
        "Omission",
        &["what", "count", "bytes", "full"],
        vec![
            text(what),
            int(count),
            int(bytes),
            full.map_or_else(MontyObject::none, reference),
        ],
    )
}

/// The fields of one `File`.
pub(super) struct FileFields<'a> {
    pub(super) path: &'a str,
    pub(super) text: &'a str,
    pub(super) first_line: u64,
    pub(super) lines: u64,
    pub(super) eof: bool,
    pub(super) revision: &'a str,
    pub(super) image: Option<MontyObject>,
    pub(super) omitted: Vec<MontyObject>,
}

/// `File(...)`.
pub(super) fn file(fields: FileFields<'_>) -> MontyObject {
    tuple(
        "File",
        &[
            "path",
            "text",
            "first_line",
            "lines",
            "eof",
            "revision",
            "image",
            "omitted",
        ],
        vec![
            text(fields.path),
            text(fields.text),
            int(fields.first_line),
            int(fields.lines),
            MontyObject::bool(fields.eof),
            text(fields.revision),
            fields.image.unwrap_or_else(MontyObject::none),
            MontyObject::list(fields.omitted),
        ],
    )
}

/// The `File` of one read page; `path` names what the cell asked for.
pub(super) fn file_read(value: &FileReadValue, path: &str) -> MontyObject {
    let revision = value
        .revision
        .map(plakous_tools::WorkspaceContentIdentity::revision_token)
        .unwrap_or_default();
    let mut omitted = Vec::new();
    if value.lines_cut > 0 {
        omitted.push(omission("lines", value.lines_cut, 0, None));
    }
    file(FileFields {
        path,
        text: &value.text,
        first_line: value.first_line,
        lines: value.lines,
        eof: value.eof,
        revision: &revision,
        image: None,
        omitted,
    })
}

/// `Image(id, path, media_type, bytes, width, height)`.
pub(super) fn image(id: &str, value: &ImageValue) -> MontyObject {
    let (width, height) = value.dimensions.map_or((None, None), |(width, height)| {
        (Some(i64::from(width)), Some(i64::from(height)))
    });
    tuple(
        "Image",
        &["id", "path", "media_type", "bytes", "width", "height"],
        vec![
            text(id),
            text(&value.path),
            text(&value.media_type),
            int(value.bytes.len() as u64),
            optional_int(width),
            optional_int(height),
        ],
    )
}

/// `Answer(text, sources)` with `Source(url, title)` items.
pub(super) fn answer(value: &AnswerValue) -> MontyObject {
    let sources = value.sources.iter().map(|source| {
        tuple(
            "Source",
            &["url", "title"],
            vec![
                text(&source.url),
                source.title.as_deref().map_or_else(MontyObject::none, text),
            ],
        )
    });
    tuple(
        "Answer",
        &["text", "sources"],
        vec![text(&value.text), MontyObject::list(sources)],
    )
}

/// `Page(...)`; `image` is the issued `Image` and `full` the `Ref` of the
/// whole text, when there are ones.
pub(super) fn page(
    value: &PageValue,
    image: Option<MontyObject>,
    full: Option<&str>,
) -> MontyObject {
    let mut omitted = Vec::new();
    if value.omitted_bytes > 0 {
        omitted.push(omission("output", 0, value.omitted_bytes, full));
    }
    tuple(
        "Page",
        &[
            "url",
            "final_url",
            "status",
            "content_type",
            "text",
            "image",
            "truncated",
            "omitted",
        ],
        vec![
            text(&value.url),
            text(&value.final_url),
            int(u64::from(value.status)),
            value
                .content_type
                .as_deref()
                .map_or_else(MontyObject::none, text),
            text(&value.text),
            image.unwrap_or_else(MontyObject::none),
            MontyObject::bool(value.truncated),
            MontyObject::list(omitted),
        ],
    )
}

/// `Hits(hits, omitted)`.
pub(super) fn hits(value: &SearchHitsValue) -> MontyObject {
    let hits = value
        .hits
        .iter()
        .map(|hit| {
            tuple(
                "Hit",
                &["path", "line", "text"],
                vec![text(&hit.path), int(hit.line), text(&hit.text)],
            )
        })
        .collect::<Vec<_>>();
    let mut omitted = Vec::new();
    if value.hits_cut > 0 || value.incomplete {
        omitted.push(omission("matches", value.hits_cut, 0, None));
    }
    tuple(
        "Hits",
        &["hits", "omitted"],
        vec![MontyObject::list(hits), MontyObject::list(omitted)],
    )
}

/// `Listing(entries, omitted)`.
pub(super) fn listing(value: &ListingValue) -> MontyObject {
    let entries = value
        .entries
        .iter()
        .map(|entry| {
            let kind = match entry.kind {
                ListedEntryKind::File => "file",
                ListedEntryKind::Directory => "dir",
                ListedEntryKind::Link => "link",
                ListedEntryKind::Other => "other",
            };
            tuple(
                "Entry",
                &["path", "kind", "size"],
                vec![text(&entry.path), text(kind), int(entry.size)],
            )
        })
        .collect::<Vec<_>>();
    let mut omitted = Vec::new();
    if value.entries_cut > 0 || value.incomplete {
        omitted.push(omission("entries", value.entries_cut, 0, None));
    }
    tuple(
        "Listing",
        &["entries", "omitted"],
        vec![MontyObject::list(entries), MontyObject::list(omitted)],
    )
}

/// `list[fs.Change]`.
pub(super) fn changes(value: &FileChangesValue) -> MontyObject {
    MontyObject::list(value.changes.iter().map(|change| {
        let action = match change.action {
            FileChangeAction::Add => "add",
            FileChangeAction::Update => "update",
            FileChangeAction::Delete => "delete",
            FileChangeAction::Move => "move",
        };
        tuple(
            "Change",
            &["path", "action", "revision"],
            vec![
                text(&change.path),
                text(action),
                change.revision.map_or_else(MontyObject::none, |revision| {
                    text(&revision.revision_token())
                }),
            ],
        )
    }))
}

/// `proc.Result(status, exit_code, output, omitted)`.
pub(super) fn process_result(
    status: &str,
    exit_code: Option<i32>,
    output: &str,
    omitted: Vec<MontyObject>,
) -> MontyObject {
    tuple(
        "Result",
        &["status", "exit_code", "output", "omitted"],
        vec![
            text(status),
            optional_int(exit_code.map(i64::from)),
            text(output),
            MontyObject::list(omitted),
        ],
    )
}

/// `proc.Job(id)`.
pub(super) fn job(id: &str) -> MontyObject {
    tuple("Job", &["id"], vec![text(id)])
}

/// `proc.Status(job, status, exit_code, output, omitted)`.
pub(super) fn process_status(
    job_id: &str,
    status: &str,
    exit_code: Option<i32>,
    output: &str,
    omitted: Vec<MontyObject>,
) -> MontyObject {
    tuple(
        "Status",
        &["job", "status", "exit_code", "output", "omitted"],
        vec![
            job(job_id),
            text(status),
            optional_int(exit_code.map(i64::from)),
            text(output),
            MontyObject::list(omitted),
        ],
    )
}

/// `state.Slot(name, bytes, tainted)`.
pub(super) fn slot(name: &str, bytes: u64, tainted: bool) -> MontyObject {
    tuple(
        "Slot",
        &["name", "bytes", "tainted"],
        vec![text(name), int(bytes), MontyObject::bool(tainted)],
    )
}

/// Converts a cell value to a named-state JSON value: `None`, `bool`, `int`
/// within 64 bits, finite `float`, `str`, and lists and `str`-keyed dicts of
/// them. Anything else is refused.
pub(super) fn state_json(value: ObjectRef<'_>) -> Result<Value, String> {
    state_json_within(value, 0)
}

fn state_json_within(value: ObjectRef<'_>, depth: usize) -> Result<Value, String> {
    if depth > MAX_STATE_DEPTH {
        return Err(format!(
            "state value nests deeper than {MAX_STATE_DEPTH} levels"
        ));
    }
    match unstable::node(value) {
        MontyNode::None => Ok(Value::Null),
        MontyNode::Bool(flag) => Ok(Value::Bool(*flag)),
        MontyNode::Int(number) => Ok(Value::from(*number)),
        MontyNode::Float(number) if number.is_finite() => Ok(Value::from(*number)),
        MontyNode::String(text) => Ok(Value::String(text.clone())),
        MontyNode::List(ids) => ids
            .iter()
            .map(|id| state_json_within(unstable::child(value, *id), depth + 1))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        MontyNode::Dict(pairs) => {
            let mut map = Map::new();
            for (key, item) in pairs {
                let MontyNode::String(name) = unstable::node(unstable::child(value, *key)) else {
                    return Err("state dict keys must be str".to_owned());
                };
                let item = state_json_within(unstable::child(value, *item), depth + 1)?;
                map.insert(name.clone(), item);
            }
            Ok(Value::Object(map))
        }
        _ => Err(format!(
            "state values must be JSON (None, bool, int, float, str, list, dict), not {}",
            value.type_name()
        )),
    }
}
