//! Check that every retained revision reaches the same shape as a fresh store.

use std::path::Path;

use super::Schema;

/// Exercise every upgrade source in the supplied registry in an empty test directory.
pub fn check_revision_paths(
    schema: &Schema,
    directory: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let fresh = schema.open_writer(&directory.join("fresh.sqlite3"), |_| Ok(()))?;
    let shape = schema.read_shape(&fresh)?;
    let fingerprint = schema.recorded_fingerprint(&fresh)?;
    if fingerprint != schema.fingerprint() {
        return Err(format!(
            "{} fresh store did not record its current fingerprint",
            schema.stem
        )
        .into());
    }
    for (length, (revision, _)) in schema.steps().enumerate() {
        let prior = Schema {
            revisions: &schema.revisions[..length],
            ..*schema
        };
        let path = directory.join(format!("source-{revision}.sqlite3"));
        drop(prior.open_writer(&path, |_| Ok(()))?);
        schema
            .lock_upgrade(&path)?
            .ok_or("expected an upgrade from an older revision")?
            .backup()?
            .commit(|_, _| {})?;
        let upgraded = schema.open_writer(&path, |_| Ok(()))?;
        if schema.read_shape(&upgraded)? != shape
            || schema.recorded_fingerprint(&upgraded)? != fingerprint
        {
            return Err(format!(
                "{} revision {revision} does not upgrade to the fresh schema",
                schema.stem
            )
            .into());
        }
    }
    Ok(())
}
