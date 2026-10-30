use super::*;

#[test]
fn terminal_blocks_keep_lines_and_tabs_but_drop_every_escape() {
    let hostile = "a\u{1b}[31mred\u{1b}]52;c;cGF5bG9hZA==\u{7}\tb\r\nc\rd\u{9b}2J\u{202e}e\n";
    assert_eq!(
        safe_terminal_block(hostile),
        "a�[31mred�]52;c;cGF5bG9hZA==�\tb\nc�d�2J�e\n"
    );
    assert_eq!(
        safe_terminal_block("plain 日本 text\n"),
        "plain 日本 text\n"
    );
}

#[test]
fn destination_policy_preserves_pipes_and_bounds_after_terminal_projection() {
    let hostile = "normal 日本\n\u{1b}]52;c;payload\u{7}\u{9b}2J\u{202e}";
    assert_eq!(terminal_block(hostile, TerminalMode::Piped, None), hostile);
    assert_eq!(
        terminal_block(hostile, TerminalMode::Interactive, None),
        "normal 日本\n�]52;c;payload��2J�"
    );

    let controls = "\u{1b}".repeat(64 * 1024);
    let projected = terminal_block(&controls, TerminalMode::Interactive, Some(64 * 1024));
    assert_eq!(projected.len(), 64 * 1024 - 1);
    assert!(projected.chars().all(|character| character == '�'));
    assert!(projected.is_char_boundary(projected.len()));
}

#[test]
fn table_cells_strip_terminal_spoofing_and_preserve_pipe_framing() {
    let mut table = Table::new(["A", "B"]);
    table
        .push(["safe\u{1b}]52;c;payload\u{7}", "x\ty\n\u{202e}z"])
        .unwrap();
    let mut output = Vec::new();
    table.write(&mut output, TerminalMode::Piped).unwrap();
    let output = String::from_utf8(output).unwrap();
    assert!(!output.contains('\u{1b}'));
    assert!(!output.contains('\u{7}'));
    assert!(!output.contains('\u{202e}'));
    assert_eq!(output.lines().count(), 2);
    assert_eq!(output.matches('\t').count(), 2);
}

#[test]
fn aligned_tables_measure_terminal_cells_not_unicode_scalars() {
    let mut table = Table::new(["VALUE", "NEXT"]);
    table.push(["界", "one"]).unwrap();
    table.push(["e\u{301}", "two"]).unwrap();
    let mut output = Vec::new();
    table.write(&mut output, TerminalMode::Interactive).unwrap();
    let output = String::from_utf8(output).unwrap();
    let starts = output
        .lines()
        .filter_map(|line| {
            line.find("one")
                .or_else(|| line.find("two"))
                .map(|index| line[..index].width())
        })
        .collect::<Vec<_>>();
    assert_eq!(starts.len(), 2);
    assert_eq!(starts[0], starts[1]);
}

#[test]
fn tables_align_on_terminals_and_delimit_when_piped() {
    let mut table = Table::new(["ACCOUNT", "STATUS"]);
    table.push(["Codex", "ready"]).unwrap();
    table.push(["OpenRouter", "missing"]).unwrap();

    let mut terminal = Vec::new();
    table
        .write(&mut terminal, TerminalMode::Interactive)
        .unwrap();
    assert_eq!(
        String::from_utf8(terminal).unwrap(),
        "ACCOUNT     STATUS\nCodex       ready\nOpenRouter  missing\n"
    );

    let mut pipe = Vec::new();
    table.write(&mut pipe, TerminalMode::Piped).unwrap();
    assert_eq!(
        String::from_utf8(pipe).unwrap(),
        "ACCOUNT\tSTATUS\nCodex\tready\nOpenRouter\tmissing\n"
    );
}

#[test]
fn provider_labels_are_consistent() {
    assert_eq!(
        ProviderLabel::new("supergrok").to_string(),
        "Grok Subscription"
    );
    assert_eq!(ProviderLabel::new("xai").to_string(), "xAI API");
    assert_eq!(
        ProviderLabel::new("openrouter").to_string(),
        "OpenRouter API"
    );
    assert_eq!(ProviderLabel::new("custom").to_string(), "custom");
}
