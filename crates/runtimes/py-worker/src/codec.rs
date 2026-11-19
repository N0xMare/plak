use std::io::{self, Write};

use micro_monty::{HostError, HostErrorKind, HostValue, Unconverted};
use plakous_cell::{BindingInput, CellValue, MAX_CELL_VALUE_DEPTH, json_depth};
use serde_json::Value;

pub(super) const MAX_ARGUMENT_BYTES: usize = 1024 * 1024;
const ARGUMENT_TOO_LARGE: &str = "Python tool argument exceeds 1 MiB";

pub(super) fn arguments(
    input: BindingInput,
    mut args: Vec<HostValue>,
    kwargs: Vec<(String, HostValue)>,
) -> Result<CellValue, HostError> {
    let argument = if args.is_empty() {
        HostValue::Dict(kwargs)
    } else if args.len() == 1 && kwargs.is_empty() {
        args.pop().unwrap_or(HostValue::None)
    } else {
        return Err(argument_shape());
    };
    let value = match argument {
        HostValue::Dict(items) if input == BindingInput::String => {
            let one: Option<[(String, HostValue); 1]> = items.try_into().ok();
            match one {
                Some([(key, HostValue::Str(value))]) if key == "arguments" => Value::String(value),
                _ => return Err(argument_shape()),
            }
        }
        HostValue::Dict(items) => argument_json(HostValue::Dict(items))?,
        // The private freeform callback also accepts the raw string generated
        // by the wrapper; the public tool takes `arguments=` or a dict.
        HostValue::Str(value) if input == BindingInput::String => Value::String(value),
        HostValue::Unconverted(reason) => return Err(argument_error(&reason)),
        _ => return Err(argument_shape()),
    };
    if json_depth(&value) > MAX_CELL_VALUE_DEPTH {
        return Err(argument_error(&Unconverted::TooDeep));
    }
    let mut count = Budget(MAX_ARGUMENT_BYTES);
    if serde_json::to_writer(&mut count, &value).is_err() {
        return Err(HostError::new(
            HostErrorKind::ValueError,
            ARGUMENT_TOO_LARGE,
        ));
    }
    Ok(CellValue::from(value))
}

struct Budget(usize);

impl Write for Budget {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self
            .0
            .checked_sub(bytes.len())
            .ok_or_else(|| io::Error::other(ARGUMENT_TOO_LARGE))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn argument_shape() -> HostError {
    HostError::new(
        HostErrorKind::TypeError,
        "pass tool arguments as keywords or one dictionary",
    )
}

fn argument_error(reason: &Unconverted) -> HostError {
    let message = match reason {
        Unconverted::TooLarge { .. } => ARGUMENT_TOO_LARGE.to_owned(),
        Unconverted::TooDeep => {
            format!("Python tool argument nests deeper than {MAX_CELL_VALUE_DEPTH} levels")
        }
        Unconverted::KeyNotStr => "Python tool argument dict keys must be str".into(),
        _ => "Python tool argument has no JSON form".into(),
    };
    HostError::new(HostErrorKind::ValueError, message)
}

fn argument_json(value: HostValue) -> Result<Value, HostError> {
    Ok(match value {
        HostValue::None => Value::Null,
        HostValue::Bool(value) => Value::Bool(value),
        HostValue::Int(value) => Value::from(value),
        HostValue::BigInt(value) => Value::from(value.parse::<u64>().map_err(|_| {
            HostError::new(
                HostErrorKind::ValueError,
                "Python tool argument integer is outside the JSON integer range",
            )
        })?),
        HostValue::Float(value) if value.is_finite() => Value::from(value),
        HostValue::Str(value) => Value::String(value),
        HostValue::List(items) | HostValue::Tuple(items) => Value::Array(
            items
                .into_iter()
                .map(argument_json)
                .collect::<Result<_, _>>()?,
        ),
        HostValue::Dict(items) => Value::Object(
            items
                .into_iter()
                .map(|(key, item)| Ok((key, argument_json(item)?)))
                .collect::<Result<_, HostError>>()?,
        ),
        HostValue::Unconverted(reason) => return Err(argument_error(&reason)),
        _ => {
            return Err(HostError::new(
                HostErrorKind::ValueError,
                "Python tool argument has no JSON form",
            ));
        }
    })
}

pub(super) fn outcome(value: Result<CellValue, String>) -> Result<HostValue, HostError> {
    match value {
        Ok(value) => result_value(value, 0),
        Err(message) => Err(tool_error(message)),
    }
}

pub(super) fn tool_error(message: String) -> HostError {
    const MAX_DIAGNOSTIC_BYTES: usize = 16 * 1024;
    let message = if message.len() > MAX_DIAGNOSTIC_BYTES {
        let end = message.floor_char_boundary(MAX_DIAGNOSTIC_BYTES);
        format!("{}\n[diagnostic truncated]", &message[..end])
    } else {
        message
    };
    HostError {
        class: Some("ToolError".into()),
        ..HostError::new(HostErrorKind::RuntimeError, message)
    }
}

fn result_value(value: CellValue, depth: usize) -> Result<HostValue, HostError> {
    if depth >= MAX_CELL_VALUE_DEPTH
        && matches!(
            value,
            CellValue::List(_) | CellValue::Dict(_) | CellValue::Record { .. }
        )
    {
        return Err(tool_error(format!(
            "nested result nests deeper than {MAX_CELL_VALUE_DEPTH} levels"
        )));
    }
    let fields = |entries: Vec<(String, CellValue)>| {
        entries
            .into_iter()
            .map(|(key, item)| Ok((key, result_value(item, depth + 1)?)))
            .collect::<Result<_, HostError>>()
    };
    Ok(match value {
        CellValue::Null => HostValue::None,
        CellValue::Bool(value) => HostValue::Bool(value),
        CellValue::Int(value) => HostValue::Int(value),
        CellValue::BigInt(value) => HostValue::BigInt(value),
        CellValue::Float(value) => HostValue::Float(value),
        CellValue::Str(value) => HostValue::Str(value),
        CellValue::Bytes(value) => HostValue::Bytes(value),
        CellValue::List(items) => HostValue::List(
            items
                .into_iter()
                .map(|item| result_value(item, depth + 1))
                .collect::<Result<_, _>>()?,
        ),
        CellValue::Dict(items) => HostValue::Dict(fields(items)?),
        CellValue::Record {
            name,
            fields: items,
        } => HostValue::Record {
            name,
            fields: fields(items)?,
        },
    })
}

#[cfg(test)]
mod tests;
