//! Literal process value bounds shared by every process request.

use super::{
    ExecutionBackendError, MAX_EXECUTION_VALUE_BYTES, MAX_LITERAL_PROCESS_ARGUMENTS, Path,
    invalid_request, resource_error,
};

pub(in crate::environment::backend) fn validate_absolute_program(
    executable: &Path,
    message: &'static str,
) -> Result<(), ExecutionBackendError> {
    if !executable.is_absolute() {
        return Err(invalid_request(message));
    }
    let value = executable
        .to_str()
        .ok_or_else(|| invalid_request("joined process executable is not portable UTF-8"))?;
    validate_process_value(value)
}

pub(in crate::environment::backend) fn validate_process_values(
    values: &[String],
) -> Result<(), ExecutionBackendError> {
    if values.len() > MAX_LITERAL_PROCESS_ARGUMENTS {
        return Err(resource_error(
            "joined process argument count exceeds its bound",
        ));
    }
    values
        .iter()
        .try_for_each(|value| validate_process_value(value))
}

pub(in crate::environment::backend) fn validate_process_value(
    value: &str,
) -> Result<(), ExecutionBackendError> {
    if value.len() > MAX_EXECUTION_VALUE_BYTES || value.as_bytes().contains(&0) {
        return Err(invalid_request("joined process literal value is invalid"));
    }
    Ok(())
}
