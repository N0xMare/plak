//! A background job's command and output tail as a face shows them. The
//! runtime displays a job's command escaped and cut, and keeps a tail cut
//! at a byte bound; both are the runtime's process notices to the model as
//! well, so they are redacted here, for display only (spec 04 §5).

/// A job's displayed command, redacted as the raw command it shows. The
/// runtime displays a command with each newline and carriage return escaped
/// (`\n`, `\r`) and cut at 1 KiB; escaped, a secret that starts a line
/// (a heredoc's token) follows a letter and misses the redactor's word
/// boundary. The escapes are undone for the redactor and made again after
/// it, which gives back the displayed text wherever nothing was redacted; a
/// private key the cut left without its END is redacted to the end.
pub(crate) fn redacted_job_command(command: &str) -> String {
    let raw = command.replace("\\r", "\r").replace("\\n", "\n");
    plakous::tools::preview_redaction::text(&raw)
        .replace('\r', "\\r")
        .replace('\n', "\\n")
}

/// A job's retained output tail, redacted for display. The runtime starts
/// every tail at a line, so the redactor sees whole lines and no secret
/// whose prefix a cut removed; a tail it cut inside its only line it marks
/// ([`plakous::tools::JOB_TAIL_FRAGMENT`]), and that fragment is not shown,
/// only the mark. A private key whose BEGIN was cut off is redacted from the
/// start of the tail to its END.
pub(crate) fn redacted_job_tail(output: &str) -> String {
    let fragment = plakous::tools::JOB_TAIL_FRAGMENT;
    let output = if output.starts_with(fragment) {
        fragment
    } else {
        output
    };
    let mut text = plakous::tools::preview_redaction::text(output);
    // The redactor took every whole key and every key without its END, so
    // an END left is a key's whose BEGIN is gone.
    if let Some(end) = last_key_end(&text) {
        text.replace_range(..end, plakous::tools::preview_redaction::MARKER);
    }
    text
}

/// Where the last `-----END [WORD ]PRIVATE KEY-----` marker in `text` ends.
fn last_key_end(text: &str) -> Option<usize> {
    const KEY: &str = "PRIVATE KEY-----";
    const END: &str = "-----END ";
    let mut search = text.len();
    while let Some(at) = text[..search].rfind(KEY) {
        let before = &text[..at];
        let worded = before
            .strip_suffix(' ')
            .map(|word| word.trim_end_matches(|ch: char| ch.is_ascii_uppercase()));
        if before.ends_with(END) || worded.is_some_and(|rest| rest.ends_with(END)) {
            return Some(at + KEY.len());
        }
        search = at;
    }
    None
}
