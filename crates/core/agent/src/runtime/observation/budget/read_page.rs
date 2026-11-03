//! The head-first cut of a read page whose raising program renders its own
//! notes and continuation.

use plakous_tools::internal::RaisedReadPage;

/// Keeps whole lines from the head of the page's lines within `remaining`
/// bytes; the page then says where to continue after the last line shown.
/// When no line fits, `withheld` (the capacity guidance) takes the lines'
/// place, so the page is not simply re-read at the same size.
///
/// `body` is exactly the text `page` describes.
pub(super) fn cut(body: &str, page: &RaisedReadPage, remaining: usize, withheld: &str) -> String {
    let lines = &body[..page.lines_bytes()];
    let whole = lines.matches('\n').count();
    // A cut shows fewer lines than its page, so it never reads as the whole.
    let most = if lines.ends_with('\n') {
        whole - 1
    } else {
        whole
    };
    let most_shown = u64::try_from(most).unwrap_or(u64::MAX);
    // The notes after `most` lines are the longest any cut can carry: the
    // continuation's numbers only grow with the lines shown.
    let room = remaining.saturating_sub(page.cut("\n", most_shown).len().saturating_sub(1));
    let mut end = 0;
    let mut shown = 0;
    for line in lines.split_inclusive('\n') {
        if shown == most || !line.ends_with('\n') || end + line.len() > room {
            break;
        }
        end += line.len();
        shown += 1;
    }
    let shown = u64::try_from(shown).unwrap_or(u64::MAX);
    if shown == 0 {
        return page.cut(&format!("{withheld}\n"), 0);
    }
    page.cut(&lines[..end], shown)
}
