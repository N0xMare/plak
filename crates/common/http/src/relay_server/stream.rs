//! Cutting an upstream body into data frames: one frame per server-sent
//! event, split only above the frame bound, and otherwise each upstream
//! chunk as it arrives, split at the bound. Nothing is coalesced.

use std::collections::VecDeque;

/// Pending upstream bytes and the frames cut from them.
pub(super) struct Chunker {
    event_stream: bool,
    max: usize,
    pending: Vec<u8>,
    ready: VecDeque<Vec<u8>>,
    /// Where the event scan stopped in `pending`, so bytes are scanned once.
    scanned: usize,
    /// Whether the scan has seen a line end on the current line position.
    scan: LineScan,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LineScan {
    /// Inside a line with content, or at the event start.
    Content,
    /// Just after a line end, so another line end ends the event.
    LineEnd,
    /// Just after a `\r`, which may be the first half of `\r\n`.
    CarriageReturn { blank_before: bool },
}

impl Chunker {
    pub(super) fn new(event_stream: bool, max: usize) -> Self {
        Self {
            event_stream,
            max: max.max(1),
            pending: Vec::new(),
            ready: VecDeque::new(),
            scanned: 0,
            scan: LineScan::Content,
        }
    }

    pub(super) fn push(&mut self, bytes: &[u8]) {
        if !self.event_stream {
            for piece in bytes.chunks(self.max) {
                self.ready.push_back(piece.to_vec());
            }
            return;
        }
        self.pending.extend_from_slice(bytes);
        self.cut_events();
    }

    /// At the end of the body: whatever is pending is one last frame (split
    /// at the bound).
    pub(super) fn finish(&mut self) {
        let rest = std::mem::take(&mut self.pending);
        for piece in rest.chunks(self.max) {
            self.ready.push_back(piece.to_vec());
        }
        self.scanned = 0;
        self.scan = LineScan::Content;
    }

    pub(super) fn next_frame(&mut self) -> Option<Vec<u8>> {
        self.ready.pop_front()
    }

    fn cut_events(&mut self) {
        let mut index = self.scanned;
        while index < self.pending.len() {
            let byte = self.pending[index];
            index += 1;
            let event_end = match (self.scan, byte) {
                (LineScan::CarriageReturn { blank_before }, b'\n') => {
                    self.scan = LineScan::LineEnd;
                    blank_before
                }
                (LineScan::CarriageReturn { blank_before }, b'\r') => {
                    if blank_before {
                        // `\r` then `\r`: the first ended a blank line.
                        index -= 1;
                        true
                    } else {
                        self.scan = LineScan::CarriageReturn { blank_before: true };
                        false
                    }
                }
                (LineScan::CarriageReturn { blank_before }, _) => {
                    if blank_before {
                        index -= 1;
                        true
                    } else {
                        self.scan = LineScan::Content;
                        false
                    }
                }
                (LineScan::LineEnd, b'\n') => true,
                (LineScan::LineEnd, b'\r') => {
                    self.scan = LineScan::CarriageReturn { blank_before: true };
                    false
                }
                (_, b'\n') => {
                    self.scan = LineScan::LineEnd;
                    false
                }
                (_, b'\r') => {
                    self.scan = LineScan::CarriageReturn {
                        blank_before: false,
                    };
                    false
                }
                _ => {
                    self.scan = LineScan::Content;
                    false
                }
            };
            if event_end {
                let rest = self.pending.split_off(index);
                let event = std::mem::replace(&mut self.pending, rest);
                for piece in event.chunks(self.max) {
                    self.ready.push_back(piece.to_vec());
                }
                index = 0;
                self.scan = LineScan::Content;
            } else if index >= self.max && self.scan == LineScan::Content {
                let rest = self.pending.split_off(self.max);
                let piece = std::mem::replace(&mut self.pending, rest);
                self.ready.push_back(piece);
                index -= self.max;
            }
        }
        self.scanned = index;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use super::*;

    fn frames(event_stream: bool, max: usize, chunks: &[&[u8]]) -> Vec<Vec<u8>> {
        let mut chunker = Chunker::new(event_stream, max);
        let mut out = Vec::new();
        for chunk in chunks {
            chunker.push(chunk);
            while let Some(frame) = chunker.next_frame() {
                out.push(frame);
            }
        }
        chunker.finish();
        while let Some(frame) = chunker.next_frame() {
            out.push(frame);
        }
        out
    }

    #[test]
    fn one_frame_per_event_whatever_the_upstream_chunking() {
        let stream: &[u8] = b"data: a\n\nevent: x\r\ndata: b\r\n\r\ndata: c\r\rdata: d\n\n";
        let expected: Vec<Vec<u8>> = vec![
            b"data: a\n\n".to_vec(),
            b"event: x\r\ndata: b\r\n\r\n".to_vec(),
            b"data: c\r\r".to_vec(),
            b"data: d\n\n".to_vec(),
        ];
        assert_eq!(frames(true, 1024, &[stream]), expected);
        let bytewise: Vec<&[u8]> = stream.chunks(1).collect();
        assert_eq!(frames(true, 1024, &bytewise), expected);
        let split: Vec<&[u8]> = stream.chunks(5).collect();
        assert_eq!(frames(true, 1024, &split), expected);
    }

    #[test]
    fn an_event_over_the_bound_is_split_and_a_tail_is_flushed_at_the_end() {
        let event = [b"data: ".as_slice(), &[b'x'; 20], b"\n\n"].concat();
        let out = frames(true, 8, &[&event, b"data: tail"]);
        assert_eq!(out.concat(), [event.as_slice(), b"data: tail"].concat());
        assert!(
            out.iter()
                .all(|frame| !frame.is_empty() && frame.len() <= 8)
        );
        assert_eq!(out.last().unwrap().as_slice(), b"il");
    }

    #[test]
    fn other_bodies_keep_upstream_chunks_split_at_the_bound() {
        assert_eq!(
            frames(false, 4, &[b"abcdef", b"gh"]),
            vec![b"abcd".to_vec(), b"ef".to_vec(), b"gh".to_vec()]
        );
    }
}
