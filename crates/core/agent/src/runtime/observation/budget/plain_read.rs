//! A plain-text file read: the selected text, then one metadata line
//! `offset=<n> lines=<n> eof=<bool> next_offset=<n|null>` with optional
//! `clipped_lines=<n>`, `lossy=true`, `workspace_generation=<n>`,
//! `final_newline=false` and `observation_truncated=true` fields.

/// One plain read split into its selected text and the metadata it keeps.
pub(super) struct PlainRead<'a> {
    /// Selected text, without the separator newline `final_newline=false` marks.
    text: &'a str,
    offset: u64,
    /// `clipped_lines`, `lossy` and `workspace_generation`, verbatim.
    kept: Vec<&'a str>,
}

impl<'a> PlainRead<'a> {
    /// Recognises a body whose last line is a read metadata line.
    pub(super) fn parse(body: &'a str) -> Option<Self> {
        let body = body.strip_suffix('\n')?;
        let start = body.rfind('\n').map_or(0, |index| index + 1);
        let (text, metadata) = body.split_at(start);
        let mut fields = metadata.split(' ');
        let offset = number(fields.next()?.strip_prefix("offset=")?)?;
        number(fields.next()?.strip_prefix("lines=")?)?;
        if !matches!(fields.next()?.strip_prefix("eof=")?, "true" | "false") {
            return None;
        }
        let next = fields.next()?.strip_prefix("next_offset=")?;
        if next != "null" {
            number(next)?;
        }
        let mut kept = Vec::new();
        let mut unterminated = false;
        for field in fields {
            match field.split_once('=')? {
                ("clipped_lines" | "workspace_generation", value) => {
                    number(value)?;
                    kept.push(field);
                }
                ("lossy", "true") => kept.push(field),
                ("final_newline", "false") => unterminated = true,
                ("observation_truncated", "true") => {}
                _ => return None,
            }
        }
        let text = if unterminated {
            text.strip_suffix('\n')?
        } else {
            text
        };
        Some(Self { text, offset, kept })
    }

    /// Keeps whole lines from the head within `remaining` bytes and rewrites
    /// the metadata to describe them: `next_offset` continues after the last
    /// line shown, and `observation_truncated=true` marks the cut. When no
    /// line fits, `withheld` (the capacity guidance) precedes the metadata
    /// line, so the page is not simply re-read at the same size.
    pub(super) fn cut(&self, remaining: usize, withheld: &str) -> String {
        let total = self.text.split_inclusive('\n').count() as u64;
        let room = remaining.saturating_sub(self.metadata(total).len());
        let mut end = 0;
        let mut shown = 0_u64;
        for line in self.text.split_inclusive('\n') {
            if !line.ends_with('\n') || end + line.len() > room {
                break;
            }
            end += line.len();
            shown += 1;
        }
        let mut cut = if shown == 0 {
            format!("{withheld}\n")
        } else {
            self.text[..end].to_owned()
        };
        cut.push_str(&self.metadata(shown));
        cut
    }

    fn metadata(&self, shown: u64) -> String {
        let mut line = format!(
            "offset={} lines={shown} eof=false next_offset={}",
            self.offset,
            self.offset.saturating_add(shown)
        );
        for field in &self.kept {
            line.push(' ');
            line.push_str(field);
        }
        line.push_str(" observation_truncated=true\n");
        line
    }
}

fn number(value: &str) -> Option<u64> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}
