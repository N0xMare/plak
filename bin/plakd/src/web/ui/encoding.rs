//! Picks a precompressed sibling without changing the logical asset's URL or MIME type.

use hyper::header::{ACCEPT_ENCODING, HeaderMap};

use super::{File, Ui};

#[derive(Default)]
struct Preferences {
    br: Option<u16>,
    gzip: Option<u16>,
    identity: Option<u16>,
    any: Option<u16>,
}

impl Preferences {
    fn parse(headers: &HeaderMap) -> Self {
        let mut preferences = Self::default();
        for value in headers
            .get_all(ACCEPT_ENCODING)
            .iter()
            .filter_map(|value| value.to_str().ok())
        {
            for choice in value.split(',') {
                let (coding, quality) =
                    choice
                        .split_once(';')
                        .map_or((choice, Some(1000)), |(coding, parameter)| {
                            let quality = parameter
                                .trim()
                                .split_once('=')
                                .filter(|(name, _)| name.trim().eq_ignore_ascii_case("q"))
                                .and_then(|(_, value)| quality(value.trim()));
                            (coding, quality)
                        });
                let coding = coding.trim();
                let field = if coding.eq_ignore_ascii_case("br") {
                    &mut preferences.br
                } else if coding.eq_ignore_ascii_case("gzip") {
                    &mut preferences.gzip
                } else if coding.eq_ignore_ascii_case("identity") {
                    &mut preferences.identity
                } else if coding == "*" {
                    &mut preferences.any
                } else {
                    continue;
                };
                if quality.is_some() {
                    *field = quality;
                }
            }
        }
        preferences
    }
}

fn quality(value: &str) -> Option<u16> {
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    if fraction.len() > 3 || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let fraction = fraction
        .bytes()
        .zip([100, 10, 1])
        .map(|(digit, scale)| u16::from(digit - b'0') * scale)
        .sum();
    match whole {
        "0" => Some(fraction),
        "1" if fraction == 0 => Some(1000),
        _ => None,
    }
}

pub(super) fn choose<'a>(
    ui: &'a Ui,
    identity: &'a File,
    logical: &str,
    headers: &HeaderMap,
) -> Option<(&'a File, Option<&'static str>)> {
    let preference = Preferences::parse(headers);
    let quality_of = |explicit: Option<u16>| explicit.or(preference.any).unwrap_or(0);
    let mut best = (preference.identity.unwrap_or_default(), (identity, None));
    // Try Brotli first in a tie. Implicit identity is the fallback, unless
    // a wildcard or explicit identity weight excluded it.
    for (coding, suffix, weight) in [
        ("br", ".br", preference.br),
        ("gzip", ".gz", preference.gzip),
    ] {
        let q = quality_of(weight);
        if q > 0
            && (q > best.0 || (q == best.0 && best.1.1.is_none()))
            && let Some(file) = ui.file(&format!("{logical}{suffix}"))
        {
            best = (q, (file, Some(coding)));
        }
    }
    if best.1.1.is_some()
        || preference
            .identity
            .unwrap_or_else(|| if preference.any == Some(0) { 0 } else { 1000 })
            > 0
    {
        Some(best.1)
    } else {
        None
    }
}
