#![no_main]

use libfuzzer_sys::fuzz_target;
use plakous_model::SemanticJsonV1;

fuzz_target!(|data: &[u8]| {
    let Ok(raw) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(value) = SemanticJsonV1::parse(raw) else {
        return;
    };
    let canonical = std::str::from_utf8(value.canonical_bytes())
        .expect("semantic canonical bytes are UTF-8");
    let reparsed = SemanticJsonV1::parse(canonical)
        .expect("semantic canonical bytes must reparse");
    assert_eq!(reparsed.canonical_bytes(), value.canonical_bytes());
    assert_eq!(reparsed.digest(), value.digest());

    let wire = serde_json::to_vec(&value).expect("semantic wire serialization");
    let reconstructed: SemanticJsonV1 =
        serde_json::from_slice(&wire).expect("semantic wire reconstruction");
    assert_eq!(reconstructed, value);
});
