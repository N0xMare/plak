//! Hosted reload requests have no terminal or connected-client owner.

use std::os::unix::fs::PermissionsExt as _;
use std::time::{Duration, Instant};

use super::*;

fn wait_json(path: &std::path::Path) -> serde_json::Value {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Ok(bytes) = std::fs::read(path) {
            return serde_json::from_slice(&bytes).unwrap();
        }
        assert!(
            Instant::now() < deadline,
            "no reload record: {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_host_refuses_requested_reload_without_any_client_or_redispatch() {
    for busy in [false, true] {
        let Some(fixture) = fixture() else { return };
        let config = fixture.home().join("config.toml");
        let original = std::fs::read_to_string(&config).unwrap();
        std::fs::write(
            config,
            format!("{original}\n[tui]\nreload_requests = true\n"),
        )
        .unwrap();
        let step = if busy {
            json!({"stream":[{"text":ANSWER},{"stall":30000}]})
        } else {
            json!({"output":{"text":ANSWER}})
        };
        fixture.write_script(&json!({"version":1,"steps":[step]}));
        let mut host = start(&fixture);
        let mut probe = UnixStream::connect(&host.socket).unwrap();
        probe.set_read_timeout(Some(WAIT)).unwrap();
        probe
            .write_all(
                encode_line(&ClientFrame::<PlakExtension>::Hello(Hello {
                    proto: WIRE_VERSION,
                    binary_id: "reload-test".into(),
                    caps: Vec::new(),
                }))
                .unwrap()
                .as_bytes(),
            )
            .unwrap();
        let mut welcome = String::new();
        BufReader::new(probe).read_line(&mut welcome).unwrap();
        let HostFrame::<PlakExtension>::Welcome(welcome) = decode_line(welcome.trim_end()).unwrap()
        else {
            panic!("not welcomed");
        };
        let build = match welcome.binary_id.as_str() {
            "" | "unknown" => String::new(),
            binary => format!(" (build {binary})"),
        };
        let mut client = Client::connect(&host);
        let Outcome::Ok(Answer::Created { session, node, .. }) =
            client.request(1, new_session(&fixture.workspace()))
        else {
            panic!("no session")
        };
        let attachment = client.attach(2, &node);
        client.prompt(3, attachment, "stay in the service", ANSWER);
        drop(client);
        let channel = fixture.home().join("run/reload-requests");
        let listener = wait_json(&channel.join(format!("{session}.listener")));
        assert_eq!(listener["pid"], host.child.as_ref().unwrap().id());
        let request = uuid::Uuid::new_v4().to_string();
        let path = channel.join(format!("{session}.pending"));
        std::fs::write(
            &path,
            serde_json::to_vec(&json!({
                "version":1,"request_id":request,"session_id":session,
                "expect_binary":"0".repeat(64),"nonce":listener["nonce"]
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::rename(&path, channel.join(format!("{session}.request"))).unwrap();
        let outcome = wait_json(&channel.join(format!("{request}.outcome")));
        assert_eq!(outcome["outcome"], "refused");
        assert_eq!(
            outcome["reason"],
            format!(
                "this session runs in plak host{build}; restarting the host moves it onto the installed build (run the installed build's `plak host upgrade --expect-binary <id>` with its binary id)"
            )
        );
        let mut later = Client::connect(&host);
        later.attach(1, &node);
        assert_eq!(later.rows(2)[0].session, session);
        assert_eq!(crash::dispatches(&fixture), 1, "reload redispatched");
        drop(later);
        stop(&mut host);
    }
}
