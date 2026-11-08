//! Enforced console document policy; report-only headers cannot satisfy it.

use std::collections::BTreeMap;

pub(super) fn console_policy(header: &str) -> bool {
    let mut directives = BTreeMap::new();
    for directive in header.split(';') {
        let mut words = directive.split_ascii_whitespace();
        let Some(name) = words.next() else {
            continue;
        };
        let values: Vec<_> = words.collect();
        if values.iter().any(|value| {
            value.eq_ignore_ascii_case("'unsafe-inline'")
                || value.eq_ignore_ascii_case("'unsafe-eval'")
        }) {
            return false;
        }
        // CSP uses the first duplicate. Refuse ambiguity instead of checking
        // a later restrictive directive that the browser will ignore.
        if directives
            .insert(name.to_ascii_lowercase(), values)
            .is_some()
        {
            return false;
        }
    }
    for (name, expected) in [
        ("connect-src", "'self'"),
        ("frame-ancestors", "'none'"),
        ("object-src", "'none'"),
        ("base-uri", "'none'"),
        ("require-trusted-types-for", "'script'"),
    ] {
        if !matches!(directives.get(name), Some(values) if values.len() == 1 && values[0].eq_ignore_ascii_case(expected))
        {
            return false;
        }
    }
    let scripts = directives
        .get("script-src")
        .or_else(|| directives.get("default-src"));
    if !scripts.is_some_and(|values| script_sources(values)) {
        return false;
    }
    if directives
        .get("script-src-elem")
        .is_some_and(|values| !script_sources(values))
    {
        return false;
    }
    directives
        .get("script-src-attr")
        .is_none_or(|values| values.as_slice() == ["'none'"])
}

fn script_sources(values: &[&str]) -> bool {
    !values.is_empty()
        && values.iter().all(|value| {
            matches!(*value, "'none'" | "'self'")
                || ["'sha256-", "'sha384-", "'sha512-"]
                    .iter()
                    .any(|prefix| value.starts_with(prefix) && value.ends_with('\''))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    const STRICT: &str = "default-src 'self'; script-src 'self'; connect-src 'self'; require-trusted-types-for 'script'; frame-ancestors 'none'; object-src 'none'; base-uri 'none'";

    #[test]
    fn console_csp_rejects_each_isolated_unsafe_or_widened_source() {
        for invalid in [
            format!("{STRICT}; style-src 'unsafe-inline'"),
            format!("{STRICT}; script-src-attr 'unsafe-hashes'"),
            format!("{STRICT}; script-src-attr 'self'"),
            STRICT.replace("connect-src 'self'", "connect-src 'self' https:"),
            STRICT.replace("object-src 'none'", "object-src 'none' 'self'"),
        ] {
            assert!(!console_policy(&invalid), "{invalid}");
        }
    }

    #[test]
    fn static_console_scripts_allow_only_self_none_and_hashes() {
        for source in ["'strict-dynamic'", "'nonce-example'", "'wasm-unsafe-eval'"] {
            assert!(!console_policy(&STRICT.replace(
                "script-src 'self'",
                &format!("script-src 'self' {source}")
            )));
        }
        for source in [
            "'none'",
            "'sha256-example'",
            "'sha384-example'",
            "'sha512-example'",
        ] {
            assert!(console_policy(
                &STRICT.replace("script-src 'self'", &format!("script-src {source}"))
            ));
        }
    }

    #[test]
    fn enforced_console_policy_requires_every_boundary_without_unsafe_escape_hatches() {
        assert!(console_policy(STRICT));
        for invalid in [
            STRICT.replace("connect-src 'self'", "connect-src https:"),
            STRICT.replace("require-trusted-types-for 'script';", ""),
            STRICT.replace("base-uri 'none'", "base-uri *"),
            STRICT.replace("object-src 'none'", "object-src 'self'"),
            STRICT.replace("frame-ancestors 'none'", "frame-ancestors 'self'"),
            format!("script-src 'unsafe-inline'; {STRICT}"),
            STRICT.replace("script-src 'self'", "script-src 'unsafe-eval'"),
            STRICT.replace("script-src 'self'", "script-src *"),
            format!("{STRICT}; script-src-elem https:"),
            format!("connect-src *; {STRICT}"),
        ] {
            assert!(!console_policy(&invalid), "{invalid}");
        }
    }
}
