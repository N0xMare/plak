//! Enqueue clock for the isolated WebSocket soak.

const STAMP_BYTES: usize = u64::BITS as usize;

#[derive(Clone, Copy)]
pub(crate) struct Enabled(bool);

#[derive(Clone, Copy)]
pub(crate) struct Stamp(Option<u64>);

impl Enabled {
    pub(crate) const fn off() -> Self {
        Self(false)
    }

    pub(crate) fn web() -> Self {
        Self(super::soak_enqueue())
    }

    pub(crate) fn now(self) -> Stamp {
        Stamp(self.0.then(|| {
            let clock = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
            let sec = u64::try_from(clock.tv_sec).unwrap_or_default();
            let ns = u64::try_from(clock.tv_nsec).unwrap_or_default();
            sec * 1_000_000_000 + ns
        }))
    }
}

impl Stamp {
    pub(crate) const fn off() -> Self {
        Self(None)
    }

    pub(crate) const fn bytes(self) -> usize {
        if self.0.is_some() { STAMP_BYTES } else { 0 }
    }

    pub(crate) fn append(self, line: &mut Vec<u8>) {
        // Whitespace preserves the frame schema; the newline is the outbox delimiter.
        if let Some(ns) = self.0 {
            line.pop();
            line.extend(
                (0..STAMP_BYTES)
                    .rev()
                    .map(|bit| if ns & (1 << bit) == 0 { b' ' } else { b'\t' }),
            );
            line.push(b'\n');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{STAMP_BYTES, Stamp};

    #[test]
    fn stamp_round_trips_most_significant_bit_first() {
        for value in [0, 0x8000_0000_0000_0001, 0x0123_4567_89ab_cdef] {
            let mut line = b"{}\n".to_vec();
            Stamp(Some(value)).append(&mut line);
            assert_eq!(&line[..2], b"{}");
            assert_eq!(line.last(), Some(&b'\n'));
            assert_eq!(line.len(), 3 + STAMP_BYTES);
            let decoded = line[2..2 + STAMP_BYTES].iter().fold(0, |bits, byte| {
                (bits << 1)
                    | match byte {
                        b' ' => 0,
                        b'\t' => 1,
                        _ => panic!("not a binary whitespace digit"),
                    }
            });
            assert_eq!(decoded, value);
        }
    }
}
