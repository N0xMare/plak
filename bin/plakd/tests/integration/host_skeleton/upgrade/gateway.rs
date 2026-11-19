use super::*;

#[test]
fn cross_key_preflight_refuses_codex_behind_a_v1_gateway_before_surrender() {
    let Some(fixture) = fixture() else { return };
    let source = seed(&fixture, 7);
    let mut host = serving_host(spawn_host(
        fixture.command().arg("host").env_remove("PLAK_TEST_MODEL"),
    ))
    .unwrap();
    let mut client = Client::connect(&host);
    assert!(client.rows(1).iter().any(|row| row.session == source));

    // The source has already latched its local gateway link. The destination probe
    // reads the new configuration, with no scripted-model bypass.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let answering = std::thread::spawn(move || answer_versions(listener));
    let key = fixture.home().join("fixture-caller.key");
    write_private(&key, "fixture-key");
    let config = fixture.home().join("config.toml");
    let mut edited = std::fs::read_to_string(&config)
        .unwrap()
        .parse::<toml_edit::DocumentMut>()
        .unwrap();
    edited["gateway"]["address"] = toml_edit::value(format!("tcp:{address}"));
    edited["gateway"]["key_file"] = toml_edit::value(key.to_str().unwrap());
    write_private(&config, &edited.to_string());

    let result = upgrade(&fixture, &binary(&fixture));
    assert!(!result.status.success());
    let reason = String::from_utf8_lossy(&result.stderr);
    assert!(reason.contains("host_upgrade_credentials"), "{reason}");
    assert!(
        reason.contains("gateway protocol too old; upgrade the gateway"),
        "{reason}"
    );
    assert_eq!(answering.join().unwrap(), "GET /versions HTTP/1.1\r\n");
    assert_eq!(client.request(2, Op::Ping), Outcome::Ok(Answer::Pong));
    assert!(client.rows(3).iter().any(|row| row.session == source));
    assert!(!fixture.home().join("host/upgrades").exists());
    assert!(
        !fixture
            .home()
            .join(format!("state/{DESTINATION}/runtime.sqlite3"))
            .exists()
    );
    stop(&mut host);
}

fn answer_versions(listener: std::net::TcpListener) -> String {
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};

    listener.set_nonblocking(true).unwrap();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            tokio::time::timeout(WAIT, async {
                let (stream, _) = listener.accept().await.unwrap();
                let mut peer = tokio::io::BufReader::new(stream);
                let mut first = String::new();
                peer.read_line(&mut first).await.unwrap();
                let mut line = String::new();
                while line != "\r\n" {
                    line.clear();
                    assert_ne!(peer.read_line(&mut line).await.unwrap(), 0);
                }
                let body = br#"{"versions":[1],"instance":"11111111-2222-4333-8444-555555555555"}"#;
                let mut stream = peer.into_inner();
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len(),
                );
                stream.write_all(head.as_bytes()).await.unwrap();
                stream.write_all(body).await.unwrap();
                stream.shutdown().await.unwrap();
                first
            })
            .await
            .unwrap()
        })
}
