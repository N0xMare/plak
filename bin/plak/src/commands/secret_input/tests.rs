use super::*;
use std::env;
use std::io::{BufReader, Read, Write};

#[test]
fn raw_bound_stops_before_reading_or_trimming_unbounded_input() {
    struct Endless {
        read: usize,
        byte: u8,
    }
    impl Read for Endless {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            assert!(self.read < 16, "reader crossed the raw input bound");
            assert_eq!(buffer.len(), 1);
            buffer[0] = self.byte;
            self.read += 1;
            Ok(1)
        }
    }
    impl io::BufRead for Endless {
        fn fill_buf(&mut self) -> io::Result<&[u8]> {
            panic!("must use bounded reads")
        }
        fn consume(&mut self, _: usize) {
            panic!("must use bounded reads")
        }
    }
    for byte in *b"x \t" {
        let mut input = Endless { read: 0, byte };
        let error = read_plain(&mut input, &mut Vec::new(), 16).expect_err("raw bound");
        assert!(error.message().contains("too long"));
        assert_eq!(input.read, 16);
    }
    let mut input = b" padded \n".as_slice();
    let secret = read_plain(&mut input, &mut Vec::new(), 9).expect("bounded line");
    assert_eq!(secret.as_str(), "padded");
    assert_eq!(format!("{secret:?}"), "SecretLine(<redacted>)");
}

// A fresh process owns both the signal handler and its controlling PTY.
// The ordinary offline test never touches the developer's terminal.
#[test]
fn pty_secret_child() -> Result<(), Box<dyn std::error::Error>> {
    struct PromptProbe;
    impl Write for PromptProbe {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            io::stderr().write(bytes)
        }
        fn flush(&mut self) -> io::Result<()> {
            let hidden = rustix::termios::tcgetattr(io::stdin())?;
            assert!(
                !hidden
                    .local_modes
                    .intersects(LocalModes::ECHO | LocalModes::ECHONL)
            );
            assert!(
                hidden
                    .local_modes
                    .contains(LocalModes::ISIG | LocalModes::ICANON)
            );
            io::stderr().flush()
        }
    }
    let Ok(scenario) = env::var("PLAK_TEST_OAUTH_PTY") else {
        return Ok(());
    };
    let stdin = io::stdin();
    let before = rustix::termios::tcgetattr(&stdin)?;
    let flags = rustix::fs::fcntl_getfl(&stdin)?;
    if scenario == "panic" {
        struct PanicInput;
        impl Read for PanicInput {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                panic!("synthetic reader failure")
            }
        }
        impl io::BufRead for PanicInput {
            fn fill_buf(&mut self) -> io::Result<&[u8]> {
                panic!("synthetic reader failure")
            }
            fn consume(&mut self, _: usize) {}
        }
        assert!(
            std::panic::catch_unwind(|| {
                let _ = read_system(&mut PanicInput, &mut PromptProbe, 16);
            })
            .is_err()
        );
    } else {
        let result = read_system(&mut stdin.lock(), &mut PromptProbe, 16);
        match scenario.as_str() {
            "success" => assert_eq!(result.expect("synthetic paste").as_str().len(), 12),
            "bound" => assert!(result.expect_err("bound").message().contains("too long")),
            "cancel" => assert!(result.expect_err("cancel").message().contains("cancelled")),
            _ => panic!("unknown PTY scenario"),
        }
    }
    let restored = rustix::termios::tcgetattr(&stdin)?;
    assert_eq!(restored.local_modes, before.local_modes);
    assert_eq!(restored.input_modes, before.input_modes);
    assert_eq!(restored.output_modes, before.output_modes);
    assert_eq!(restored.control_modes, before.control_modes);
    assert_eq!(format!("{restored:?}"), format!("{before:?}"));
    assert_eq!(rustix::fs::fcntl_getfl(&stdin)?, flags);
    writeln!(io::stderr(), "RESTORED")?;
    Ok(())
}

#[test]
fn owned_pty_hides_paste_and_restores_after_success_refusal_cancel_and_panic()
-> Result<(), Box<dyn std::error::Error>> {
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};
    use std::sync::mpsc;
    for scenario in ["success", "bound", "cancel", "panic"] {
        let pair = native_pty_system().openpty(PtySize::default())?;
        let mut command = CommandBuilder::new(env::current_exe()?);
        command.args([
            "--exact",
            "commands::secret_input::tests::pty_secret_child",
            "--nocapture",
        ]);
        command.env("PLAK_TEST_OAUTH_PTY", scenario);
        let mut child = pair.slave.spawn_command(command)?;
        let mut writer = pair.master.take_writer()?;
        let reader = pair.master.try_clone_reader()?;
        let (tx, rx) = mpsc::channel();
        let join = std::thread::spawn(move || {
            let mut reader = BufReader::new(reader);
            loop {
                let mut bytes = [0; 256];
                match reader.read(&mut bytes) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.send(bytes[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        let mut output = Vec::new();
        let result = (|| -> Result<(), Box<dyn std::error::Error>> {
            while !output.windows(2).any(|bytes| bytes == b"> ") {
                output.extend(rx.recv_timeout(std::time::Duration::from_secs(10))?);
            }
            if scenario != "panic" {
                match scenario {
                    "success" => writer.write_all(b"synthetic123\n")?,
                    "bound" => writer.write_all(b"                                \n")?,
                    "cancel" => writer.write_all(b"synthetic123\x03")?,
                    _ => unreachable!(),
                }
                writer.flush()?;
            }
            while !output.windows(8).any(|bytes| bytes == b"RESTORED") {
                output.extend(rx.recv_timeout(std::time::Duration::from_secs(10))?);
            }
            assert!(!output.windows(12).any(|bytes| bytes == b"synthetic123"));
            Ok(())
        })();
        if result.is_err() {
            let _ = child.kill();
        }
        let status = child.wait()?;
        drop(writer);
        drop(pair);
        join.join().map_err(|_| "PTY reader panicked")?;
        result?;
        assert!(status.success());
    }
    Ok(())
}
