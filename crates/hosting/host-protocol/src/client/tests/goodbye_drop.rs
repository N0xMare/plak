use super::*;
use std::os::unix::net::UnixListener;
use std::process::{Command, Stdio};

#[test]
#[ignore = "subprocess helper for immediate-exit goodbye regression"]
fn immediate_exit_child() {
    let path = std::env::var_os("PLAK_WIRE_DROP_TEST_SOCKET").unwrap();
    let socket = UnixStream::connect(path).unwrap();
    let shutdown = socket.try_clone().unwrap();
    let transport = Transport::new(socket.try_clone().unwrap(), socket)
        .with_write_shutdown(move || shutdown.shutdown(std::net::Shutdown::Write));
    let (sender, receiver, _) = connect_directory::<Core>(transport, hello()).unwrap();
    sender.request_operation(Op::Ping).unwrap();
    if std::env::var_os("PLAK_WIRE_DROP_TEST_EXPLICIT").is_some() {
        sender.goodbye();
    }
    drop((sender, receiver));
    std::process::exit(0);
}

struct SocketPath(std::path::PathBuf);

impl Drop for SocketPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[test]
fn last_handle_drop_flushes_a_queued_frame_and_bye_before_immediate_process_exit() {
    let path =
        SocketPath(std::env::temp_dir().join(format!("wd-{}", uuid::Uuid::new_v4().simple())));
    let listener = UnixListener::bind(&path.0).unwrap();
    listener.set_nonblocking(true).unwrap();
    for iteration in 0..200 {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "client::tests::goodbye_drop::immediate_exit_child",
                "--ignored",
            ])
            .env("PLAK_WIRE_DROP_TEST_SOCKET", &path.0)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if iteration % 2 == 0 {
            command.env("PLAK_WIRE_DROP_TEST_EXPLICIT", "1");
        } else {
            command.env_remove("PLAK_WIRE_DROP_TEST_EXPLICIT");
        }
        let child = command.spawn().unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        let socket = loop {
            match listener.accept() {
                Ok((socket, _)) => break socket,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < until, "child did not connect: {iteration}");
                    thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("accept: {error}"),
            }
        };
        // BSD sockets inherit the listener's O_NONBLOCK on accept; Linux does not.
        socket.set_nonblocking(false).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut host = Host {
            lines: BufReader::new(socket.try_clone().unwrap()),
            stream: socket,
        };
        assert!(matches!(host.read(), ClientFrame::Hello(_)));
        host.send(&HostFrame::Welcome(Welcome {
            session: None,
            ..welcome()
        }));
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "child {iteration}: {output:?}");
        assert_eq!(host.request(), (1, Op::Ping), "iteration {iteration}");
        assert_eq!(host.read(), ClientFrame::Bye, "iteration {iteration}");
        assert_eq!(host.lines.read_line(&mut String::new()).unwrap(), 0);
    }
}
