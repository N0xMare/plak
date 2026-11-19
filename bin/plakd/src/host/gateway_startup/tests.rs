use std::cell::Cell;
use std::future::Future;
use std::pin::Pin;

use rustix::net::{AddressFamily, SocketAddrUnix, SocketType, bind, listen, socket};

use super::*;

async fn still_waiting(mut proof: Pin<&mut impl Future<Output = Result<(), String>>>) {
    tokio::select! {
        biased;
        result = &mut proof => panic!("startup decided too early: {result:?}"),
        () = time::sleep(STARTUP_RETRY * 2) => {}
    }
}

#[tokio::test]
async fn startup_waits_for_the_holder_and_a_connectable_unix_listener() {
    let directory = tempfile::tempdir().unwrap();
    let configured = directory.path().join("gw.sock");
    let advertised = Cell::new(false);
    let proof = wait_for_listener(&configured, || Ok(advertised.get()));
    tokio::pin!(proof);
    still_waiting(proof.as_mut()).await;

    // A gateway from an earlier build may advertise before it listens.
    advertised.set(true);
    still_waiting(proof.as_mut()).await; // ENOENT
    let fd = socket(AddressFamily::UNIX, SocketType::STREAM, None).unwrap();
    bind(&fd, &SocketAddrUnix::new(&configured).unwrap()).unwrap();
    still_waiting(proof.as_mut()).await; // ECONNREFUSED: bound, not listening
    listen(&fd, 16).unwrap();
    proof.await.unwrap();
}
