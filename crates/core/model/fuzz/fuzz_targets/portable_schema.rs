#![no_main]

use libfuzzer_sys::fuzz_target;
use plakous_model::{PortableToolSchemaV1, SemanticJsonV1};

fuzz_target!(|data: &[u8]| {
    let Ok(raw) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(document) = SemanticJsonV1::parse(raw) else {
        return;
    };
    let Ok(schema) = PortableToolSchemaV1::new(document.value().as_ref().clone()) else {
        return;
    };
    let wire = serde_json::to_vec(&schema).expect("portable-schema serialization");
    let reconstructed: PortableToolSchemaV1 =
        serde_json::from_slice(&wire).expect("portable-schema reconstruction");
    assert_eq!(reconstructed, schema);

    let _ = schema.parse_instance(raw);
});
