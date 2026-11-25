use plakous_host_protocol::views::PresentationSnapshot;

use super::{
    AttachedHistory, AttachmentId, BaselineTarget, CliResult, HistoryAttachment, HistoryBinding,
    HostFrameV13, OpV13, PlakPaths, PreparedViewport, RequestId, RequestRefusal, RootTranscript,
    TranscriptRead, ViewportMode, ViewportRevision, ViewportStart, Write, first_page_frames,
    history, prepare_first_page, read_live_row, replay_frames, retained_revision, wire,
    write_frames,
};

enum Prefix {
    Saved(AttachedHistory),
    Replay(HistoryAttachment, ViewportRevision),
}

struct Prepared {
    paths: PlakPaths,
    prefix: Prefix,
    revision: u64,
    root: Option<RootTranscript>,
}

struct Bound {
    paths: PlakPaths,
    binding: HistoryAttachment,
    attachment: AttachmentId,
    revision: ViewportRevision,
    root: Option<RootTranscript>,
}

pub(in crate::host) fn prepare(
    paths: &PlakPaths,
    session: &str,
    generation: u64,
    root: Option<&RootTranscript>,
    retained: Option<&ViewportRevision>,
) -> Result<Box<dyn PreparedViewport>, wire::RequestRefusal> {
    let (prefix, revision) = if let Some(retained) =
        retained.filter(|r| r.host_generation == generation)
        && let Ok(binding) = history::replay_binding(paths, session)
    {
        (Prefix::Replay(binding, retained.clone()), retained.revision)
    } else {
        let page =
            prepare_first_page(paths, session, 0, generation, root).map_err(
                |reason| match reason {
                    RequestRefusal::Shared(reason) => reason.into_inner(),
                    RequestRefusal::Draft(_) => wire::RequestRefusal::Backpressure,
                },
            )?;
        let Some(HostFrameV13::Snapshot(first)) = page.frames.get(1) else {
            return Err(wire::RequestRefusal::Backpressure);
        };
        let revision = first.revision.revision;
        (Prefix::Saved(page), revision)
    };
    Ok(Box::new(Prepared {
        paths: paths.clone(),
        prefix,
        revision,
        root: root.cloned(),
    }))
}

impl PreparedViewport for Prepared {
    fn fence(&self) -> std::sync::Arc<str> {
        match &self.prefix {
            Prefix::Saved(page) => page.binding.fence(),
            Prefix::Replay(binding, _) => binding.fence(),
        }
    }

    fn mode(&self) -> ViewportMode {
        match &self.prefix {
            Prefix::Saved(_) => ViewportMode::Page(self.revision),
            Prefix::Replay(_, revision) => ViewportMode::Replay(revision.clone()),
        }
    }

    fn write(
        self: Box<Self>,
        out: &mut dyn Write,
        id: RequestId,
        start: ViewportStart,
    ) -> CliResult<Box<dyn HistoryBinding>> {
        let attachment = start.attachment;
        let revision = start.revision.clone();
        let AttachedHistory { binding, frames } = match self.prefix {
            Prefix::Saved(page) => first_page_frames(id, page, start),
            Prefix::Replay(mut binding, _) => {
                binding.bind(attachment);
                AttachedHistory {
                    binding,
                    frames: replay_frames(id, start),
                }
            }
        };
        write_frames(out, frames)?;
        Ok(Box::new(Bound {
            paths: self.paths,
            binding,
            attachment,
            revision,
            root: self.root,
        }))
    }
}

impl HistoryBinding for Bound {
    fn repair(&self, out: &mut dyn Write, snapshot: &PresentationSnapshot) -> CliResult<bool> {
        let Some(root) = &self.root else {
            return Ok(false);
        };
        let mut revision = retained_revision(&self.revision);
        revision.revision = snapshot.revision;
        let target = BaselineTarget {
            attachment: self.attachment,
            scope: snapshot.scope.clone(),
            revision,
        };
        let Some(frames) = history::repair(&self.paths, &self.binding, target, root) else {
            return Ok(false);
        };
        write_frames(out, frames)?;
        Ok(true)
    }

    fn read(
        &self,
        out: &mut dyn Write,
        id: RequestId,
        root: Option<&RootTranscript>,
        request: TranscriptRead,
    ) -> CliResult<()> {
        match request {
            TranscriptRead::Older(cursor) => write_frames(
                out,
                history::older(
                    &self.paths,
                    &self.binding,
                    id,
                    &plakd_wire_types::v13::HistoryCursor(cursor),
                ),
            ),
            TranscriptRead::Row(row, offset) => {
                let live = root.and_then(|root| {
                    read_live_row(
                        root,
                        self.attachment,
                        &OpV13::ReadTranscriptRow {
                            attachment: self.attachment,
                            row,
                            offset,
                        },
                    )
                });
                let frame = if let Some(outcome) = live {
                    HostFrameV13::Response { id, outcome }
                } else {
                    history::read_row(&self.paths, &self.binding, id, row, offset)
                };
                write_frames(out, [frame])
            }
            TranscriptRead::Status => unreachable!("status is served by the attachment meter"),
        }
    }
}
