//! Bounded attachment batches; snapshot and patch work keeps its own turn.

use std::io::{self, Write};

use super::{
    CliResult, ClientFrame, DeserializeOwned, HostFrame, ProtocolExtension, Serialize,
    WireConnection,
};

const BATCH_BYTES: usize = 256 * 1024;
const BATCH_ITEMS: usize = 32;

pub(super) struct Counted<W> {
    pub(super) inner: W,
    written: usize,
}

impl<W> Counted<W> {
    pub(super) const fn new(inner: W) -> Self {
        Self { inner, written: 0 }
    }
}

impl<W: Write> Write for Counted<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(bytes)?;
        self.written = self.written.saturating_add(written);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl<P: ProtocolExtension, W: Write> WireConnection<P, W>
where
    HostFrame<P>: Serialize,
    ClientFrame<P>: DeserializeOwned,
{
    pub(super) fn flush_paced(&mut self, bulk: bool) -> CliResult<()> {
        self.flush_answers()?;
        let Some(attachment) = self.attachment else {
            return Ok(());
        };
        if bulk {
            self.service.repair_gaps();
        }
        self.stdout.written = 0;
        for index in 0..BATCH_ITEMS {
            let items = self.service.take_for_wire(attachment, bulk, index != 0);
            let Some(item) = items.first() else {
                return Ok(());
            };
            let alone = item.paced_alone();
            self.write_observations(attachment, items)?;
            if !bulk
                || alone
                || self.attachment != Some(attachment)
                || self.stdout.written >= BATCH_BYTES
            {
                break;
            }
        }
        Ok(())
    }
}
