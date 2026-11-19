use serde_json::{Value, json};

use super::{Reply, VERSION, receive_host, serve, shipping};

fn response(mut raw: &[u8]) -> Reply {
    let mut output = Vec::new();
    serve(&mut raw, &mut output).unwrap();
    serde_json::from_slice(&output).unwrap()
}

#[test]
fn serve_refuses_protocol_before_decoding_future_operations() {
    let future =
        shipping::canonical(&json!({"deploy_protocol": VERSION + 1, "unknown_call":true})).unwrap();
    assert!(
        matches!(response(&future), Reply::ProtocolMismatch { supported, remedy }
        if supported == VERSION && remedy.contains("same shipping artifact"))
    );
    let mismatched_call =
        shipping::canonical(&json!({"deploy_protocol": VERSION, "unknown_call":true})).unwrap();
    assert!(matches!(response(&mismatched_call), Reply::Error(message)
        if message.contains("invalid typed data")));
    assert!(matches!(response(b"{}"), Reply::Error(message)
        if message.contains("size bound")));
}

fn facts(blob: &[u8]) -> shipping::FileFacts {
    let sha256 = shipping::digest(blob);
    shipping::FileFacts {
        length: blob.len() as u64,
        display_id: sha256[..12].to_owned(),
        sha256,
    }
}

fn manifest() -> Vec<u8> {
    let revision = "a".repeat(40);
    let platforms = "x86_64-unknown-linux-musl";
    let files = shipping::NAMES
        .into_iter()
        .map(|name| (name.to_owned(), facts(name.as_bytes())))
        .collect();
    let value: Value = json!({
        "schema":1, "revision":revision, "platform":platforms, "flavor":"release", "product_version":"0.0.4",
        "lock_sha256":"e".repeat(64), "snapshot_id":shipping::snapshot_id(&files), "files":files,
        "toolchain":{
            "cargo":{"release":"1.100.0","commit_hash":"b".repeat(9)},
            "rustc":{"release":"1.100.0","commit_hash":"c".repeat(40),"host":platforms,"llvm_version":"22.1.0"}
        },
        "build_info":{
            "revision":revision,"state_key":"r28.e39.p39.t11", "flavor":"release",
            "product_version":"0.0.4","omitted_features":[],"test_hooks":false,
            "wire":{"number":39,"schema_pin":"d".repeat(64)},
            "export":{"read":[7,8],"write":8},"host_upgrade":{"coordinator":8,"receipt":5},
            "gateway":{"served":[1,2],"spoken":[1,2]},
            "runner":{"protocol":9,"retention":{"served":[1,2],"spoken":[1,2]}}
        }
    });
    shipping::canonical(&value).unwrap()
}

#[test]
fn stage_host_checks_manifest_and_bootstrap_before_accepting_exact_sidecar_bytes() {
    let temp = tempfile::tempdir().unwrap();
    let binary = temp.path().join("installer");
    let manifest = manifest();
    let pin = shipping::digest(&manifest);
    let input = b"plakous-js-hostplakous-py-host";
    let error = receive_host(&binary, &manifest, &"0".repeat(64), &mut &input[..]).unwrap_err();
    assert!(error.message().contains("manifest digest differs"));
    std::fs::write(&binary, b"other").unwrap();
    let error = receive_host(&binary, &manifest, &pin, &mut &input[..]).unwrap_err();
    assert!(error.message().contains("digest mismatch"));
    std::fs::write(&binary, b"plakd").unwrap();
    let mut trailing = input.to_vec();
    trailing.push(0);
    for (mut body, expected) in [
        (&input[..input.len() - 1], "length differs"),
        (trailing.as_slice(), "trailing SSH transfer data"),
    ] {
        let error = receive_host(&binary, &manifest, &pin, &mut body).unwrap_err();
        assert!(error.message().contains(expected), "{error:?}");
    }
    let received = receive_host(&binary, &manifest, &pin, &mut &input[..]).unwrap();
    for name in shipping::NAMES {
        assert_eq!(
            std::fs::read(received.path().join(name)).unwrap(),
            name.as_bytes()
        );
    }
    assert_eq!(
        std::fs::read(received.path().join("manifest.json")).unwrap(),
        manifest
    );
}
