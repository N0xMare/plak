use super::*;

#[test]
fn write_switches_require_matching_reads_at_host_startup() -> Result<(), Box<dyn std::error::Error>>
{
    let (_root, paths) = crate::home::paths::PlakPaths::isolated_test_home();
    for (write, read) in [
        ("credentials_write", "accounts_read"),
        ("config_write", "config_read"),
    ] {
        std::fs::write(paths.config_file(), format!("[web]\n{write} = true\n"))?;
        let config = super::super::ProductConfig::load_config_file(&paths.config_file())?;
        assert!(validate_switches(&config).is_err());
        #[cfg(feature = "web")]
        assert!(super::super::ProductConfig::console_switches(&paths).is_err());
        std::fs::write(
            paths.config_file(),
            format!("[web]\n{write} = true\n{read} = true\n"),
        )?;
        let config = super::super::ProductConfig::load_config_file(&paths.config_file())?;
        validate_switches(&config)?;
        #[cfg(feature = "web")]
        assert!(super::super::ProductConfig::console_switches(&paths).is_ok());
    }
    Ok(())
}

/// An origin is exactly its own serialization; anything else is refused.
#[test]
fn an_origin_is_exact() {
    assert_eq!(
        origin_authority("https://host.example.ts.net").unwrap(),
        "host.example.ts.net"
    );
    assert_eq!(
        origin_authority("http://127.0.0.1:8123").unwrap(),
        "127.0.0.1:8123"
    );
    for refused in [
        "https://host.example.ts.net/",
        "https://host.example.ts.net/app",
        "https://host.example.ts.net:443",
        "https://Host.example.ts.net",
        "https://user@host.example.ts.net",
        "https://host.example.ts.net?x=1",
        "http://host.example.ts.net",
        "host.example.ts.net",
    ] {
        assert!(origin_authority(refused).is_err(), "{refused}");
    }
}

/// `allow` takes printable logins, each once.
#[test]
fn allow_takes_distinct_printable_logins() {
    let file = toml::Value::Array(vec!["a@example.com".into(), "b@example.com".into()]);
    assert_eq!(
        assigned_logins(FieldValue::Toml(&file)).unwrap(),
        ["a@example.com", "b@example.com"]
    );
    assert_eq!(
        assigned_logins(FieldValue::Cli("a@example.com,b@example.com")).unwrap(),
        ["a@example.com", "b@example.com"]
    );
    let twice = toml::Value::Array(vec!["a@example.com".into(), "a@example.com".into()]);
    assert!(
        assigned_logins(FieldValue::Toml(&twice))
            .unwrap_err()
            .contains("twice")
    );
    let spaced = toml::Value::Array(vec!["a b".into()]);
    assert!(assigned_logins(FieldValue::Toml(&spaced)).is_err());
    let text = toml::Value::String("a@example.com".into());
    assert!(
        assigned_logins(FieldValue::Toml(&text))
            .unwrap_err()
            .contains("must be an array")
    );
}
