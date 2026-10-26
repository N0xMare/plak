//! Canonical telemetry database and SQLite-sidecar filenames for maintenance.

pub(crate) fn version(name: &str) -> Option<i64> {
    let database = ["-wal", "-shm", "-journal"]
        .into_iter()
        .find_map(|suffix| name.strip_suffix(suffix))
        .unwrap_or(name);
    if database == "telemetry.sqlite3" {
        return Some(0);
    }
    let version = database
        .strip_prefix("telemetry-v")?
        .strip_suffix(".sqlite3")?;
    if version.is_empty()
        || !version.bytes().all(|byte| byte.is_ascii_digit())
        || (version.len() > 1 && version.starts_with('0'))
    {
        return None;
    }
    version.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::version;

    #[test]
    fn canonical_databases_and_sidecars_share_one_version_grammar() {
        for (database, expected) in [
            ("telemetry.sqlite3", 0),
            ("telemetry-v0.sqlite3", 0),
            ("telemetry-v5.sqlite3", 5),
            ("telemetry-v20.sqlite3", 20),
            ("telemetry-v999.sqlite3", 999),
        ] {
            for suffix in ["", "-wal", "-shm", "-journal"] {
                assert_eq!(version(&format!("{database}{suffix}")), Some(expected));
            }
        }
        for name in [
            "telemetry-v+5.sqlite3",
            "telemetry-v05.sqlite3",
            "telemetry-v-5.sqlite3",
            "telemetry-v.sqlite3",
            "telemetry-vbad.sqlite3",
            "telemetry-v9223372036854775808.sqlite3",
            "telemetry-v5.sqlite3-wal-shm",
            "notes.txt",
        ] {
            assert_eq!(version(name), None, "{name}");
        }
    }
}
