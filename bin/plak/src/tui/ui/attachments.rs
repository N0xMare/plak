//! Images attached to the draft as inline `[image #N]` tokens.
//!
//! A token is ordinary draft text, so caret movement, undo and history see
//! it as text; only `Backspace` at its end and `Delete` at its start remove
//! it as one unit. Reading, decoding and downscaling run on a worker thread:
//! the token is in the draft at once and resolves when the image is ready,
//! or leaves the draft with a note when it fails. On send, the referenced
//! images go in token order and the tokens are renumbered `#1..#n`, so a
//! token in the text names the image at that position of the message.

use std::ops::Range;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};

use plakous_host_protocol::{Draft, DraftImage};

use super::{App, ScrollBlock};
use crate::config::image_attachments;

type Outcome = (u64, Result<DraftImage, String>);

/// The draft's images, keyed by the number its token shows.
pub(crate) struct Attachments {
    entries: Vec<Entry>,
    next_id: u64,
    sender: Sender<Outcome>,
    receiver: Receiver<Outcome>,
}

struct Entry {
    /// Identity of the worker's answer; a number can be reused by a later
    /// draft, an id cannot.
    id: u64,
    number: u32,
    /// `None` while the worker is still reading the image.
    image: Option<DraftImage>,
}

impl Default for Attachments {
    fn default() -> Self {
        let (sender, receiver) = mpsc::channel();
        Self {
            entries: Vec::new(),
            next_id: 0,
            sender,
            receiver,
        }
    }
}

/// The token text of attachment `number`.
pub(super) fn token(number: u32) -> String {
    format!("[image #{number}]")
}

/// Every `[image #N]` in `text`, as char ranges with their numbers.
pub(super) fn token_spans(text: &str) -> Vec<(Range<usize>, u32)> {
    const OPEN: &str = "[image #";
    let mut spans = Vec::new();
    let mut search = 0;
    while let Some(found) = text[search..].find(OPEN) {
        let start = search + found;
        let digits_at = start + OPEN.len();
        let digits = text[digits_at..]
            .bytes()
            .take_while(u8::is_ascii_digit)
            .count();
        search = digits_at;
        if digits == 0 || !text[digits_at + digits..].starts_with(']') {
            continue;
        }
        let Ok(number) = text[digits_at..digits_at + digits].parse::<u32>() else {
            continue;
        };
        let end = digits_at + digits + 1;
        let char_start = text[..start].chars().count();
        let char_end = char_start + text[start..end].chars().count();
        spans.push((char_start..char_end, number));
        search = end;
    }
    spans
}

impl Attachments {
    fn get(&self, number: u32) -> Option<&Entry> {
        self.entries.iter().find(|entry| entry.number == number)
    }

    /// Whether a token in `text` still waits for its image.
    pub(super) fn pending_in(&self, text: &str) -> Option<u32> {
        token_spans(text)
            .into_iter()
            .map(|(_, number)| number)
            .find(|number| self.get(*number).is_some_and(|entry| entry.image.is_none()))
    }

    pub(super) fn any_pending(&self) -> bool {
        self.entries.iter().any(|entry| entry.image.is_none())
    }

    /// The distinct attachment numbers `text` references, in token order.
    fn referenced(&self, text: &str) -> Vec<u32> {
        let mut numbers = Vec::new();
        for (_, number) in token_spans(text) {
            if self.get(number).is_some() && !numbers.contains(&number) {
                numbers.push(number);
            }
        }
        numbers
    }

    /// Drops every image no token in `texts` references.
    fn retain_referenced(&mut self, texts: &[&str]) {
        let referenced: Vec<u32> = texts
            .iter()
            .flat_map(|text| self.referenced(text))
            .collect();
        self.entries
            .retain(|entry| referenced.contains(&entry.number));
    }

    /// Takes the images `text` references, in token order, and the text with
    /// those tokens renumbered `#1..#n`. A text that is only tokens is an
    /// image-only message. Tokens still pending stay literal text.
    pub(super) fn take_draft(&mut self, text: &str) -> Draft {
        let order: Vec<u32> = self
            .referenced(text)
            .into_iter()
            .filter(|number| self.get(*number).is_some_and(|entry| entry.image.is_some()))
            .collect();
        let mut rewritten = String::with_capacity(text.len());
        let mut rest = String::new();
        let mut last = 0;
        let chars: Vec<char> = text.chars().collect();
        for (range, number) in token_spans(text) {
            let Some(position) = order.iter().position(|taken| *taken == number) else {
                continue;
            };
            let before: String = chars[last..range.start].iter().collect();
            rewritten.push_str(&before);
            rest.push_str(&before);
            rewritten.push_str(&token(u32::try_from(position + 1).unwrap_or(u32::MAX)));
            last = range.end;
        }
        let tail: String = chars[last..].iter().collect();
        rewritten.push_str(&tail);
        rest.push_str(&tail);
        let images = order
            .iter()
            .filter_map(|number| {
                let index = self
                    .entries
                    .iter()
                    .position(|entry| entry.number == *number)?;
                self.entries.remove(index).image
            })
            .collect();
        let text = if rest.trim().is_empty() {
            String::new()
        } else {
            rewritten
        };
        Draft::new(text, images)
    }
}

impl App {
    /// Inserts the next `[image #N]` token at the caret now and reads the
    /// image on a worker thread; [`App::poll_attachments`] resolves it.
    pub(super) fn begin_attachment(
        &mut self,
        read: impl FnOnce() -> Result<DraftImage, String> + Send + 'static,
    ) {
        self.mark_dirty();
        let pending_prompt = self.pending_prompt.clone().unwrap_or_default();
        self.attachments
            .retain_referenced(&[self.input.as_str(), pending_prompt.as_str()]);
        if self.attachments.referenced(&self.input).len() >= image_attachments::MAX_IMAGES {
            self.push_block(ScrollBlock::System(format!(
                "image not attached: a message carries at most {} images",
                image_attachments::MAX_IMAGES
            )));
            return;
        }
        let number = self
            .attachments
            .entries
            .iter()
            .map(|entry| entry.number)
            .max()
            .unwrap_or(0)
            + 1;
        let id = self.attachments.next_id;
        self.attachments.next_id += 1;
        let sender = self.attachments.sender.clone();
        let spawned = std::thread::Builder::new()
            .name("plak-attach-image".to_owned())
            .spawn(move || {
                let _ = sender.send((id, read()));
            });
        if let Err(error) = spawned {
            self.push_block(ScrollBlock::System(format!(
                "not attached: could not start the image reader: {error}"
            )));
            return;
        }
        self.attachments.entries.push(Entry {
            id,
            number,
            image: None,
        });
        self.insert_chars(&format!("{} ", token(number)));
        self.after_composer_edit();
    }

    /// Applies every finished read: a ready image resolves its token; a
    /// failure removes the token and says why.
    pub(super) fn poll_attachments(&mut self) {
        loop {
            match self.attachments.receiver.try_recv() {
                Ok(outcome) => self.resolve_attachment(outcome),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return,
            }
        }
    }

    /// Blocks until no read is pending (tests drive the reader this way).
    #[cfg(test)]
    pub(super) fn settle_attachments(&mut self) {
        while self.attachments.any_pending() {
            let outcome = self
                .attachments
                .receiver
                .recv()
                .expect("the sender lives in the app");
            self.resolve_attachment(outcome);
        }
    }

    fn resolve_attachment(&mut self, (id, outcome): Outcome) {
        let Some(index) = self
            .attachments
            .entries
            .iter()
            .position(|entry| entry.id == id)
        else {
            return;
        };
        self.mark_dirty();
        match outcome {
            Ok(image) => self.attachments.entries[index].image = Some(image),
            Err(error) => {
                let number = self.attachments.entries.remove(index).number;
                self.remove_token(number);
                self.push_block(ScrollBlock::System(format!("not attached: {error}")));
            }
        }
    }

    /// Removes every `[image #number]` (with the space typed after it) from
    /// the draft, keeping the caret on the same text.
    fn remove_token(&mut self, number: u32) {
        let spans = token_spans(&self.input);
        let chars: Vec<char> = self.input.chars().collect();
        let mut kept = String::with_capacity(self.input.len());
        let mut cursor = self.input_cursor;
        let mut last = 0;
        for (range, found) in spans {
            if found != number {
                continue;
            }
            let end = if chars.get(range.end) == Some(&' ') {
                range.end + 1
            } else {
                range.end
            };
            kept.extend(&chars[last..range.start]);
            if cursor >= end {
                cursor -= end - range.start;
            } else if cursor > range.start {
                cursor = range.start;
            }
            last = end;
        }
        kept.extend(&chars[last..]);
        self.input = kept;
        self.input_cursor = cursor;
        self.composer_selection.set_caret(&self.input, cursor);
    }

    /// The token span `Backspace` (ending at the caret) or `Delete` (starting
    /// at it) removes as one unit, when the caret touches an attached image.
    pub(super) fn token_at_caret(&self, forward: bool) -> Option<Range<usize>> {
        if self.composer_selection.range(&self.input).is_some() {
            return None;
        }
        token_spans(&self.input)
            .into_iter()
            .find(|(range, number)| {
                let edge = if forward { range.start } else { range.end };
                edge == self.input_cursor && self.attachments.get(*number).is_some()
            })
            .map(|(range, _)| range)
    }

    /// Deletes one token span as an undoable edit.
    pub(super) fn delete_token(&mut self, range: Range<usize>) {
        self.record_undo(super::EditKind::Discrete);
        let kept: String = self
            .input
            .chars()
            .take(range.start)
            .chain(self.input.chars().skip(range.end))
            .collect();
        self.input = kept;
        self.input_cursor = range.start;
        self.composer_selection.set_caret(&self.input, range.start);
        self.composer_wheel_scroll = None;
        self.mark_dirty();
    }

    /// The draft `text` sends: its images in token order.
    pub(super) fn take_draft(&mut self, text: &str) -> Draft {
        let draft = self.attachments.take_draft(text);
        let pending_prompt = self.pending_prompt.clone().unwrap_or_default();
        self.attachments
            .retain_referenced(&[self.input.as_str(), pending_prompt.as_str()]);
        draft
    }

    /// A token in `text` whose image is still being read.
    pub(super) fn pending_attachment_in(&self, text: &str) -> Option<u32> {
        self.attachments.pending_in(text)
    }

    pub(super) fn attachments_pending(&self) -> bool {
        self.attachments.any_pending()
    }

    /// How many images the draft references.
    #[cfg(any(test, feature = "test-hooks"))]
    pub(super) fn attachment_count(&self) -> usize {
        self.attachments.referenced(&self.input).len()
    }

    /// Takes back a returned draft's images as attachments `#1..#k`, ahead
    /// of the ones in the draft now (renumbered after them, in the draft and
    /// in the returned text alike), and returns its text with a token for
    /// every image (an image-only message gets its tokens back).
    pub(super) fn adopt_draft_images(&mut self, draft: Draft) -> String {
        let Draft { mut text, images } = draft;
        let returned = u32::try_from(images.len()).unwrap_or(u32::MAX);
        if returned == 0 {
            return text;
        }
        let shift = |text: &str, attachments: &Attachments| {
            let mut shifted = String::with_capacity(text.len());
            let chars: Vec<char> = text.chars().collect();
            let mut last = 0;
            for (range, number) in token_spans(text) {
                if attachments.get(number).is_none() {
                    continue;
                }
                shifted.extend(&chars[last..range.start]);
                shifted.push_str(&token(number.saturating_add(returned)));
                last = range.end;
            }
            shifted.extend(&chars[last..]);
            shifted
        };
        let input = shift(&self.input, &self.attachments);
        if input != self.input {
            let cursor = self.input_cursor;
            self.input = input;
            self.input_cursor = cursor.min(self.input.chars().count());
            self.composer_selection
                .set_caret(&self.input, self.input_cursor);
        }
        if let Some(prompt) = self.pending_prompt.take() {
            self.pending_prompt = Some(shift(&prompt, &self.attachments));
        }
        for entry in &mut self.attachments.entries {
            entry.number = entry.number.saturating_add(returned);
        }
        let present: Vec<u32> = token_spans(&text).into_iter().map(|(_, n)| n).collect();
        for (number, image) in (1..).zip(images) {
            if !present.contains(&number) {
                if !text.is_empty() && !text.ends_with(char::is_whitespace) {
                    text.push(' ');
                }
                text.push_str(&token(number));
            }
            let id = self.attachments.next_id;
            self.attachments.next_id += 1;
            self.attachments.entries.push(Entry {
                id,
                number,
                image: Some(image),
            });
        }
        text
    }

    /// The draft's attachments in token order within `budget` cells, each
    /// as `[image #N: shot.png 1280x720 412 KB]` (`[image #N: reading…]`
    /// while pending), else a count.
    pub(super) fn attachment_chips(&self, budget: usize) -> String {
        use unicode_width::UnicodeWidthStr as _;
        let numbers = self.attachments.referenced(&self.input);
        if numbers.is_empty() {
            return String::new();
        }
        // The file name goes first when space is short; the dimensions and
        // a downscale note stay.
        let chips = |named: bool| {
            numbers
                .iter()
                .filter_map(|number| self.attachments.get(*number))
                .map(|entry| match &entry.image {
                    None => format!("[image #{}: reading…]", entry.number),
                    Some(image) => {
                        let chip = if named {
                            image_attachments::chip(image)
                        } else {
                            image_attachments::unnamed_chip(image)
                        };
                        format!(
                            "[image #{}: {}",
                            entry.number,
                            chip.strip_prefix("[image: ").unwrap_or(&chip)
                        )
                    }
                })
                .collect::<Vec<_>>()
                .join(" ")
        };
        let count = format!(
            "[{} image{}]",
            numbers.len(),
            if numbers.len() == 1 { "" } else { "s" }
        );
        [chips(true), chips(false), count]
            .into_iter()
            .find(|text| text.width() <= budget)
            .unwrap_or_default()
    }

    /// Char ranges of the draft's tokens with whether each is still pending.
    pub(super) fn attachment_token_ranges(&self) -> Vec<(Range<usize>, bool)> {
        token_spans(&self.input)
            .into_iter()
            .filter_map(|(range, number)| {
                let entry = self.attachments.get(number)?;
                Some((range, entry.image.is_none()))
            })
            .collect()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests may assert with unwrap")]
mod tests {
    use super::*;

    fn image(name: &str) -> DraftImage {
        DraftImage {
            name: name.to_owned(),
            media_type: "image/png".to_owned(),
            bytes: image_attachments::png(4, 3, 8),
            source_dimensions: None,
        }
    }

    fn ready(attachments: &mut Attachments, number: u32, name: &str) {
        attachments.entries.push(Entry {
            id: u64::from(number),
            number,
            image: Some(image(name)),
        });
    }

    #[test]
    fn tokens_are_found_by_char_range_and_malformed_ones_are_text() {
        let text = "é [image #1] and [image #12][image #] [image #x] [image #3";
        let spans = token_spans(text);
        assert_eq!(spans, vec![(2..12, 1), (17..28, 12)]);
    }

    #[test]
    fn a_draft_sends_its_images_in_token_order_renumbered_from_one() {
        let mut attachments = Attachments::default();
        ready(&mut attachments, 2, "b.png");
        ready(&mut attachments, 3, "c.png");
        ready(&mut attachments, 5, "unreferenced.png");
        attachments.entries.push(Entry {
            id: 9,
            number: 4,
            image: None,
        });
        assert_eq!(attachments.pending_in("[image #4]"), Some(4));
        let draft = attachments
            .take_draft("compare [image #3] with [image #2], again [image #3] [image #7]");
        assert_eq!(
            draft.text,
            "compare [image #1] with [image #2], again [image #1] [image #7]"
        );
        let names: Vec<_> = draft
            .images
            .iter()
            .map(|image| image.name.as_str())
            .collect();
        assert_eq!(names, ["c.png", "b.png"]);
        // Taken images leave; the rest stay until pruned.
        assert!(attachments.get(2).is_none() && attachments.get(5).is_some());

        ready(&mut attachments, 6, "only.png");
        let draft = attachments.take_draft(" [image #6] ");
        assert_eq!(draft.text, "");
        assert_eq!(draft.images.len(), 1);
    }

    #[test]
    fn backspace_and_delete_remove_a_token_as_one_unit_and_undo_restores_it() {
        use super::super::test_support::{key, launch_basic};
        use crossterm::event::KeyCode;

        let mut app = App::new(launch_basic(), true);
        app.insert_chars("look ");
        app.begin_attachment(|| Ok(image("a.png")));
        app.settle_attachments();
        assert_eq!(app.input, "look [image #1] ");
        assert_eq!(app.attachment_count(), 1);

        // The trailing space goes first, then the whole token.
        app.handle_key(key(KeyCode::Backspace));
        assert_eq!(app.input, "look [image #1]");
        app.handle_key(key(KeyCode::Backspace));
        assert_eq!(app.input, "look ");
        assert_eq!(app.attachment_count(), 0);
        app.composer_undo_step();
        assert_eq!(app.input, "look [image #1]");
        assert_eq!(app.attachment_count(), 1);

        // Delete at the token's start removes it whole.
        app.input_cursor = 5;
        app.composer_selection.set_caret(&app.input, 5);
        app.handle_key(key(KeyCode::Delete));
        assert_eq!(app.input, "look ");

        // A failed read removes its token and says why.
        app.begin_attachment(|| Err("image x.png: cannot be read".to_owned()));
        assert!(app.input.contains("[image #"));
        app.settle_attachments();
        assert_eq!(app.input, "look ");
        assert_eq!(app.attachment_count(), 0);
    }

    #[test]
    fn a_returned_draft_takes_the_first_numbers_ahead_of_the_current_draft() {
        use super::super::test_support::launch_basic;

        let mut app = App::new(launch_basic(), true);
        app.begin_attachment(|| Ok(image("new.png")));
        app.settle_attachments();
        assert_eq!(app.input, "[image #1] ");
        let text = app.adopt_draft_images(Draft::new(
            "see [image #1]".to_owned(),
            vec![image("old.png"), image("other.png")],
        ));
        assert_eq!(text, "see [image #1] [image #2]");
        assert_eq!(app.input, "[image #3] ");
        app.set_input(format!("{text} {}", app.input));
        let draft = app.take_draft(&app.input.clone());
        let names: Vec<_> = draft
            .images
            .iter()
            .map(|image| image.name.as_str())
            .collect();
        assert_eq!(names, ["old.png", "other.png", "new.png"]);
    }
}
