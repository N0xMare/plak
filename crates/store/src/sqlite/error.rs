//! SQLite and I/O failures as store errors.

#![allow(
    clippy::wildcard_imports,
    reason = "a split of the sqlite module that shares its vocabulary"
)]

use super::*;

pub(super) const fn startup_error_category(error: SessionStoreError) -> &'static str {
    match error {
        SessionStoreError::BackendBusy => "backend_busy",
        SessionStoreError::BackendUnavailable => "backend_unavailable",
        SessionStoreError::UnsupportedSchema => "unsupported_schema",
        SessionStoreError::UnsupportedCodec => "unsupported_codec",
        SessionStoreError::Corrupt(_) => "corrupt",
        SessionStoreError::Invalid(_) => "invalid",
        SessionStoreError::Capacity(_) => "capacity",
        _ => "semantic",
    }
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "map_err supplies the owned bundle failure"
)]
pub(super) fn bundle_error(e: CheckpointBundleError) -> SessionStoreError {
    if e == CheckpointBundleError::UnsupportedRevision {
        SessionStoreError::UnsupportedCodec
    } else {
        SessionStoreError::corrupt()
    }
}

pub(super) fn map_sqlite(error: rusqlite::Error) -> SessionStoreError {
    // The public error stays content-free. Keep the SQLite code in the local
    // diagnostic so an immediate FULL is distinguishable from a busy wait.
    if let rusqlite::Error::SqliteFailure(failure, _) = &error {
        tracing::warn!(
            target: "plakous_store::sqlite_operation",
            sqlite_code = sqlite_failure_category(failure.code),
            sqlite_extended_code = failure.extended_code,
            "SQLite store operation failed"
        );
    } else {
        tracing::warn!(
            target: "plakous_store::sqlite_operation",
            error_category = rusqlite_error_category(&error),
            "SQLite store operation failed"
        );
    }
    let mapped = match &error {
        rusqlite::Error::SqliteFailure(f, _) if f.code == ErrorCode::DiskFull => {
            SessionStoreError::StorageFull
        }
        rusqlite::Error::SqliteFailure(f, _)
            if matches!(f.code, ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) =>
        {
            SessionStoreError::BackendBusy
        }
        rusqlite::Error::SqliteFailure(f, _)
            if matches!(f.code, ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase) =>
        {
            SessionStoreError::corrupt()
        }
        // Every lookup that may find nothing reads through `optional()`; a
        // missing row elsewhere is a store invariant breach, not an outage.
        rusqlite::Error::QueryReturnedNoRows
        | rusqlite::Error::InvalidColumnType(..)
        | rusqlite::Error::IntegralValueOutOfRange(..)
        | rusqlite::Error::FromSqlConversionFailure(..) => SessionStoreError::corrupt(),
        _ => SessionStoreError::BackendUnavailable,
    };
    drop(error);
    mapped
}

/// A content-free category for errors raised by rusqlite rather than SQLite.
const fn rusqlite_error_category(error: &rusqlite::Error) -> &'static str {
    match error {
        rusqlite::Error::QueryReturnedNoRows => "no_rows",
        rusqlite::Error::InvalidQuery
        | rusqlite::Error::InvalidParameterName(_)
        | rusqlite::Error::InvalidParameterCount(..)
        | rusqlite::Error::InvalidColumnIndex(_)
        | rusqlite::Error::InvalidColumnName(_)
        | rusqlite::Error::ExecuteReturnedResults
        | rusqlite::Error::QueryReturnedMoreThanOneRow
        | rusqlite::Error::MultipleStatement => "invalid_query",
        rusqlite::Error::InvalidColumnType(..)
        | rusqlite::Error::IntegralValueOutOfRange(..)
        | rusqlite::Error::FromSqlConversionFailure(..) => "invalid_value",
        _ => "other_rusqlite",
    }
}

pub(super) const fn sqlite_failure_category(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::DiskFull => "disk_full",
        ErrorCode::DatabaseBusy => "database_busy",
        ErrorCode::DatabaseLocked => "database_locked",
        ErrorCode::SystemIoFailure => "io_failure",
        ErrorCode::CannotOpen => "cannot_open",
        ErrorCode::ReadOnly => "read_only",
        ErrorCode::DatabaseCorrupt => "database_corrupt",
        ErrorCode::NotADatabase => "not_a_database",
        _ => "other_sqlite_failure",
    }
}

pub(super) fn map_io(error: std::io::Error) -> SessionStoreError {
    let full = error.raw_os_error().is_some_and(|code| {
        code == rustix::io::Errno::NOSPC.raw_os_error()
            || code == rustix::io::Errno::DQUOT.raw_os_error()
    });
    drop(error);
    if full {
        SessionStoreError::StorageFull
    } else {
        SessionStoreError::BackendUnavailable
    }
}
