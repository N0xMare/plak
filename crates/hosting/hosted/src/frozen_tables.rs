//! The frozen canonical `[tools.<name>]` and `[mcp_servers.<id>]` tables a
//! node binds, keyed `tools.<name>` / `mcp_servers.<id>`: a child's from its
//! epoch snapshot, the root's from its template's reached tables. Every
//! reader parses them here.

use std::collections::BTreeMap;

use plakous::tools::web::FetchUrlLimits;
use serde::Deserialize;
use serde::de::DeserializeOwned;

/// One frozen table as `T`; `None` when it does not parse as one.
pub(crate) fn parse<T: DeserializeOwned>(rendered: &str) -> Option<T> {
    toml::from_str(rendered).ok()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RenderedFetchUrl {
    max_bytes: u32,
    timeout_seconds: u16,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RenderedToolTable<T> {
    tools: BTreeMap<String, T>,
}

/// The node's `fetch_url` bounds: its frozen `[tools.fetch_url]` table, or
/// the defaults when it reaches none.
///
/// # Errors
///
/// Refuses a table that does not parse or whose values are out of range.
pub(crate) fn fetch_url_limits(
    sections: &BTreeMap<String, String>,
) -> Result<FetchUrlLimits, String> {
    let Some(rendered) = sections.get("tools.fetch_url") else {
        return Ok(FetchUrlLimits::default());
    };
    parse::<RenderedToolTable<RenderedFetchUrl>>(rendered)
        .and_then(|mut table| {
            table
                .tools
                .remove("fetch_url")
                .filter(|_| table.tools.is_empty())
        })
        .and_then(|table| FetchUrlLimits::new(table.max_bytes, table.timeout_seconds))
        .ok_or_else(|| String::from("fetch_url has an invalid frozen [tools.fetch_url] table"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sections(rendered: &str) -> BTreeMap<String, String> {
        BTreeMap::from([("tools.fetch_url".to_owned(), rendered.to_owned())])
    }

    #[test]
    fn fetch_url_limits_come_from_the_frozen_table_or_the_defaults() {
        assert_eq!(
            fetch_url_limits(&BTreeMap::new()),
            Ok(FetchUrlLimits::default())
        );
        assert_eq!(
            fetch_url_limits(&sections(
                "[tools.fetch_url]\nmax_bytes = 4096\ntimeout_seconds = 7\n"
            )),
            Ok(FetchUrlLimits::new(4096, 7).unwrap_or_default())
        );
        for invalid in [
            "[tools.fetch_url]\nmax_bytes = 0\ntimeout_seconds = 7\n",
            "[tools.fetch_url]\nmax_bytes = 4096\ntimeout_seconds = 121\n",
            "[tools.fetch_url]\nmax_bytes = 4096\n",
            "[tools.fetch_url]\nmax_bytes = 4096\ntimeout_seconds = 7\nextra = 1\n",
            "[tools.web_search]\nmax_uses = 5\n",
            "not toml",
        ] {
            assert_eq!(
                fetch_url_limits(&sections(invalid)),
                Err(String::from(
                    "fetch_url has an invalid frozen [tools.fetch_url] table"
                )),
                "{invalid}"
            );
        }
    }
}
