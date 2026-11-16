use super::{ReadRefusal, invalid};
use plakd_wire_types::console::TemplateChange;
use std::collections::BTreeSet;
use toml_edit::{DocumentMut, Item, Value};

pub(super) fn profile(
    existing: Option<&str>,
    body: &plakd_wire_types::console::ProfileBody,
) -> Result<String, ReadRefusal> {
    let mut document = document(existing)?;
    put(
        document.as_table_mut(),
        "root",
        Some(Value::from(body.root.as_str())),
    );
    put(
        document.as_table_mut(),
        "working_set",
        Some(array(&body.working_set)),
    );
    Ok(document.to_string())
}

pub(super) fn template(
    existing: Option<&str>,
    changes: &[TemplateChange],
) -> Result<String, ReadRefusal> {
    if changes.is_empty() || changes.len() > 64 {
        return Err(invalid("changes"));
    }
    let mut document = document(existing)?;
    let mut seen = BTreeSet::new();
    for change in changes {
        if let TemplateChange::Env { name, value } = change {
            if !seen.insert(format!("env.{name}")) || name.len() > 256 {
                return Err(invalid("env"));
            }
            let table = document
                .entry("env")
                .or_insert(Item::Table(toml_edit::Table::new()))
                .as_table_like_mut()
                .ok_or_else(|| invalid("env"))?;
            put(
                table,
                name,
                value.as_ref().map(|value| Value::from(value.expose())),
            );
            continue;
        }
        let (key, value) = field(change)?;
        if !seen.insert(key.into()) {
            return Err(invalid("changes"));
        }
        put(document.as_table_mut(), key, value);
    }
    Ok(document.to_string())
}

fn field(change: &TemplateChange) -> Result<(&'static str, Option<Value>), ReadRefusal> {
    use TemplateChange as C;
    let value = match change {
        C::Account { value } => {
            super::super::accounts::exact(value)?;
            ("account", Some(Value::from(value.as_str())))
        }
        C::Model { value } => ("model", Some(Value::from(value.as_str()))),
        C::Effort { value } => ("effort", value.as_deref().map(Value::from)),
        C::FastMode { value } => ("fast_mode", Some(Value::from(*value))),
        C::Composition { value } => (
            "composition",
            value.map(|value| Value::from(value.as_str())),
        ),
        C::Protocol { value } => ("protocol", value.map(|value| Value::from(value.as_str()))),
        C::Description { value } => ("description", Some(Value::from(value.as_str()))),
        C::Access { value } => (
            "access",
            Some(Value::from(match value {
                plakd_wire_types::console::TemplateAccess::Full => "full",
                plakd_wire_types::console::TemplateAccess::ReadExecute => "read-execute",
            })),
        ),
        C::Tools { value } => ("tools", Some(array(value))),
        C::Spawns { value } => ("spawns", Some(array(value))),
        C::MayAdopt { value } => ("may_adopt", Some(Value::from(*value))),
        C::Delegation { value } => ("delegation", Some(Value::from(value.as_str()))),
        C::CostTier { value } => (
            "cost_tier",
            value.map(|value| Value::from(i64::from(value))),
        ),
        C::MaxTurns { value } => (
            "max_turns",
            value.map(|value| Value::from(i64::from(value))),
        ),
        C::ShowModel { value } => ("show_model", Some(Value::from(*value))),
        C::OnMint { value } => (
            "on_mint",
            value.as_ref().map(|value| Value::from(value.expose())),
        ),
        C::Env { .. } => return Err(invalid("env")),
    };
    Ok(value)
}

fn document(existing: Option<&str>) -> Result<DocumentMut, ReadRefusal> {
    existing
        .unwrap_or_default()
        .parse()
        .map_err(|_| invalid("body"))
}

fn array(values: &[String]) -> Value {
    Value::Array(values.iter().map(String::as_str).collect())
}

fn put(table: &mut dyn toml_edit::TableLike, key: &str, value: Option<Value>) {
    match value {
        Some(mut value) => {
            if let Some(existing) = table.get(key).and_then(Item::as_value) {
                *value.decor_mut() = existing.decor().clone();
            }
            table.insert(key, Item::Value(value));
        }
        None => {
            table.remove(key);
        }
    }
}
