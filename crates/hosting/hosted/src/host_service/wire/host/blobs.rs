use plakous_host_protocol::wire::{BlobRejection, BlobRejectionReason};

use super::{
    BlobRefusalOutput, CliResult, ClientFrame, DeserializeOwned, HostConnection, HostFrame,
    ProtocolExtension, Serialize, Write,
};

impl<P: ProtocolExtension, W: Write + Clone> HostConnection<P, W>
where
    P: 'static,
    P::Intent: Send,
    P::Observation: Send,
    HostFrame<P>: Serialize,
    ClientFrame<P>: DeserializeOwned,
{
    /// Supplies the unserved draft blob projection before the first client frame.
    #[must_use]
    pub const fn with_blob_refusal_output(mut self, output: BlobRefusalOutput) -> Self {
        self.blob_refusal_output = Some(output);
        self
    }

    pub(super) fn discard_blob_staging(&mut self) -> CliResult<()> {
        if let Some(link) = &mut self.node {
            link.connection.discard_blob_staging()?;
        }
        Ok(())
    }

    pub(super) fn discard_blob_part(&mut self, id: String) -> CliResult<()> {
        let Some(output) = self.blob_refusal_output else {
            return Ok(());
        };
        if let Some(link) = &mut self.node {
            // A paused part invalidates any staging for the same id. Other
            // blobs still belong to the next request or an explicit detach.
            link.connection.blobs.discard_id(&id);
        }
        output(
            &mut self.out,
            BlobRejection {
                id,
                reason: BlobRejectionReason::Discarded,
            },
        )
    }
}
