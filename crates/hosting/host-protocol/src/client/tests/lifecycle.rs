use super::*;

#[derive(Clone, Debug, Eq, PartialEq)]
struct Advisory;
impl ProtocolExtension for Advisory {
    type Intent = crate::Never;
    type Observation = crate::Never;
    type HostRequest = crate::Never;
    type HostResponse = crate::Never;
    type HostStreamEvent = u8;
    type HostLifecyclePhase = u8;
    type HostLifecycleReason = u8;
    type HostLifecycleTarget = u8;
    type WelcomeExtension = Option<crate::Never>;
}

#[test]
fn lifecycle_advisories_coalesce_without_filling_operator_streams() {
    let (mut peer, client) = UnixStream::pair().unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let mut reader = BufReader::new(peer.try_clone().unwrap());
    let connecting = thread::spawn(move || connect_directory::<Advisory>(Transport::new(client.try_clone().unwrap(), client), hello()));
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    peer.write_all(encode_line(&HostFrame::<Advisory>::Welcome(Welcome {
        proto:WIRE_VERSION, binary_id:"host".into(), host_generation:1, state_key:"key".into(),
        caps:Vec::new(), session:None, extension:None,
    })).unwrap().as_bytes()).unwrap();
    let (sender, receiver, _) = connecting.join().unwrap().unwrap();
    for phase in 0..200 {
        peer.write_all(encode_line(&HostFrame::<Advisory>::HostLifecycle {
            host_generation:1, phase, reason:1, target:None, retry_after_ms:None,
        }).unwrap().as_bytes()).unwrap();
    }
    for seq in 0..2 {
        peer.write_all(encode_line(&HostFrame::<Advisory>::TerminalStream {
            terminal:"operator".into(), stream:1, seq, event:1,
        }).unwrap().as_bytes()).unwrap();
    }
    let id = sender.request_operation(Op::Ping).unwrap();
    line.clear();
    reader.read_line(&mut line).unwrap();
    peer.write_all(encode_line(&HostFrame::<Advisory>::Response { id, outcome:Outcome::Ok(Answer::Pong) }).unwrap().as_bytes()).unwrap();
    assert_eq!(receiver.recv_response(Duration::from_secs(3)).unwrap(), (id, Outcome::Ok(Answer::Pong)));
    assert_eq!(receiver.try_host_lifecycle().unwrap().phase, 199);
    assert_eq!(receiver.try_host_lifecycle(), None);
    for seq in 0..2 {
        assert!(matches!(receiver.try_host_stream(), Some(HostFrame::TerminalStream { seq: actual, .. }) if actual == seq));
    }
    assert_eq!(receiver.try_host_stream(), None);
    peer.shutdown(std::net::Shutdown::Both).unwrap();
}
