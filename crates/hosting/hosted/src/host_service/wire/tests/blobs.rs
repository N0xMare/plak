use super::*;
use plakous_host_protocol::Core;
use plakous_host_protocol::intent::DraftImage;
use plakous_host_protocol::wire::{BlobChunk, take_images};

fn output(out: &mut dyn Write, rejection: BlobRejection) -> CliResult<()> {
    let line = rejection.id + &format!(" {:?}\n", rejection.reason);
    out.write_all(line.as_bytes())
        .map_err(|_| CliFailure::new("lost blob outcome"))
}

fn line(frame: &ClientFrame<Core>) -> Result<String, WireError> {
    encode_line(frame)
}

#[test]
fn known_bad_part_and_intent_refusal_are_separate_from_transport_loss() {
    let (actions, intents) = plakous_host_protocol::intent_channel::<Core>();
    let mut wire =
        WireConnection::new(root(), Vec::new(), actions).with_blob_refusal_output(Some(output));
    let chunk = |id: &str, offset| {
        ClientFrame::Blob(BlobChunk {
            id: id.into(),
            offset,
            data: "YQ==".into(),
            last: true,
        })
    };
    assert!(wire.frame(line(&chunk("bad", 2))).unwrap());
    assert!(wire.frame(line(&chunk("staged", 0))).unwrap());
    let request = ClientFrame::Request {
        id: 1,
        op: Op::Intent {
            attachment: 33,
            client_id: None,
            intent: SessionIntent::Interrupt,
            blobs: vec![],
        },
    };
    assert!(wire.frame(line(&request)).unwrap());
    let sent = std::str::from_utf8(&wire.stdout.inner).unwrap();
    let mut sent = sent.lines();
    assert_eq!(sent.next(), Some("bad Malformed"));
    assert_eq!(sent.next(), Some("staged Discarded"));
    assert!(sent.next().unwrap().contains("not_attached"));
    assert!(sent.next().is_none());
    assert!(intents.try_recv().is_err());

    wire.stdout.inner.clear();
    assert!(wire.frame(line(&chunk("unknown-to-peer", 0))).unwrap());
    assert!(!wire.frame(Err(WireError::TooLarge)).unwrap());
    assert_eq!(wire.stdout.inner, [] as [u8; 0]);
}

#[derive(Default)]
struct LostAnswer(Vec<u8>);

impl Write for LostAnswer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.starts_with(br#"{"frame":"response""#) {
            return Err(std::io::ErrorKind::BrokenPipe.into());
        }
        self.0.write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn loss_after_intent_admission_does_not_refuse_the_admitted_blob() {
    let (actions, intents) = plakous_host_protocol::intent_channel::<Core>();
    let mut wire = WireConnection::new(root(), LostAnswer::default(), actions)
        .with_blob_refusal_output(Some(output));
    let Some(Outcome::Ok(Answer::Attached { attachment })) =
        wire.request(1, Op::Attach { node: root() })
    else {
        panic!("fixture attach was refused");
    };
    while intents.try_recv().is_ok() {}
    let original = SessionIntent::Prompt(Draft::new(
        "inspect".into(),
        vec![DraftImage {
            name: "fixture.png".into(),
            media_type: "image/png".into(),
            bytes: vec![1, 2, 3],
            source_dimensions: None,
        }],
    ));
    let mut exported = original.clone();
    let taken = take_images(&mut exported, "image");
    for chunk in taken.chunks {
        assert!(wire.frame(line(&ClientFrame::Blob(chunk))).unwrap());
    }
    let frame = ClientFrame::Request {
        id: 2,
        op: Op::Intent {
            attachment,
            client_id: None,
            intent: exported,
            blobs: taken.ids,
        },
    };
    assert!(wire.frame(line(&frame)).is_err());
    assert_eq!(&*intents.try_recv().unwrap(), &original);
    assert!(intents.try_recv().is_err());
    assert_eq!(wire.stdout.inner.0, [] as [u8; 0]);
    assert_eq!(wire.blobs.discard_reported(), []);
}
