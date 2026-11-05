use super::{JOB_TAIL_FRAGMENT, job_output_tail, output_tail};

/// A tail starts where a line does: at the output's start, after a line its
/// line bound left out, or, when a cut fell inside a line, at the next one.
#[test]
fn a_job_tail_starts_where_a_line_does() {
    assert_eq!(job_output_tail("first\nsecond\n"), "first\nsecond");
    // The window's start is not the output's: its first line is a fragment.
    assert_eq!(output_tail("ghij0123\nnext\nlast", true), "next\nlast");
    let many = (1..=50)
        .map(|n| format!("line {n}\n"))
        .collect::<Vec<_>>()
        .concat();
    let tail = output_tail(&many, true);
    assert!(
        tail.starts_with("line 11\n") && tail.ends_with("line 50"),
        "{tail}"
    );
    // The byte bound falls inside a line, though nothing was cut before.
    let long = format!("{}\n{}", "x".repeat(5_000), "y".repeat(100));
    assert_eq!(job_output_tail(&long), "y".repeat(100));
}

/// Progress drawn with bare carriage returns normalizes to one line far
/// shorter than its bytes; cut, it is marked a fragment, never shown as a
/// line's start. A tail is its own tail.
#[test]
fn a_tail_cut_inside_its_only_line_is_marked() {
    let progress = (0..1_000)
        .map(|n| format!("{n:>4}% done\r"))
        .collect::<Vec<_>>()
        .concat();
    let raw = &progress[progress.len() - 4_096..];
    let tail = output_tail(raw, true);
    assert!(tail.starts_with(JOB_TAIL_FRAGMENT), "{tail}");
    assert!(tail.len() <= 4_096);
    assert_eq!(job_output_tail(&tail), tail);
    let whole = output_tail("ghij0123\nnext", true);
    assert_eq!(job_output_tail(&whole), whole);
    assert_eq!(output_tail("only", false), "only");
}
