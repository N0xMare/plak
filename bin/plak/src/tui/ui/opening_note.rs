//! Launch-only note placement across bounded presentation replacements.

use super::{App, ScrollBlock};
use sha2::{Digest as _, Sha256};
use std::fmt::Write as _;

pub(super) struct OpeningNotePosition(Boundary);

enum Boundary {
    Anchored {
        call_id: String,
        following: usize,
    },
    Prefix {
        len: usize,
        digest: [u8; 32],
        full_window: bool,
    },
}

struct DigestWriter(Sha256);

impl std::fmt::Write for DigestWriter {
    fn write_str(&mut self, text: &str) -> std::fmt::Result {
        self.0.update(text.as_bytes());
        Ok(())
    }
}

fn prefix_digest(blocks: &[ScrollBlock]) -> [u8; 32] {
    let mut writer = DigestWriter(Sha256::new());
    let _ = write!(writer, "{blocks:?}");
    writer.0.finalize().into()
}

impl App {
    pub(super) fn restore_opening_note(
        &mut self,
        blocks: &mut Vec<ScrollBlock>,
        full_window: bool,
    ) {
        let Some(note) = self.opening_note.clone() else {
            return;
        };
        blocks.retain(|block| !matches!(block, ScrollBlock::System(text) if *text == note));
        let position = match &self.opening_note_position {
            None => {
                let boundary = blocks
                    .iter()
                    .enumerate()
                    .rev()
                    .find_map(|(index, block)| match block {
                        ScrollBlock::Tool {
                            call_id: Some(call_id),
                            ..
                        } => Some(Boundary::Anchored {
                            call_id: call_id.clone(),
                            following: blocks.len() - index - 1,
                        }),
                        _ => None,
                    })
                    .unwrap_or_else(|| Boundary::Prefix {
                        len: blocks.len(),
                        digest: prefix_digest(blocks),
                        full_window,
                    });
                self.opening_note_position = Some(OpeningNotePosition(boundary));
                Some(blocks.len())
            }
            Some(OpeningNotePosition(Boundary::Anchored { call_id, following })) => blocks
                .iter()
                .position(|block| {
                    matches!(block,
                    ScrollBlock::Tool { call_id: Some(id), .. } if id == call_id)
                })
                .map(|index| index + 1 + following)
                .filter(|index| *index <= blocks.len()),
            Some(OpeningNotePosition(Boundary::Prefix {
                len,
                digest,
                full_window: was_full,
            })) => {
                // Anonymous rows cannot prove identity after a full-window eviction,
                // even when repeated text makes the replacement look unchanged.
                blocks
                    .get(..*len)
                    .filter(|prefix| !was_full && !full_window && prefix_digest(prefix) == *digest)
                    .map(|_| *len)
            }
        };
        if let Some(position) = position {
            blocks.insert(position, ScrollBlock::System(note));
        } else {
            self.opening_note = None;
            self.opening_note_position = None;
        }
    }
}
