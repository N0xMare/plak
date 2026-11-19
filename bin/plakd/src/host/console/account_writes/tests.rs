use plakd_wire_types::console::SecretInput;
use zeroize::Zeroizing;

fn secret(value: &str) -> SecretInput {
    SecretInput::new(Zeroizing::new(value.into())).expect("bounded fixture")
}

#[test]
fn secret_decode_bounds_whitespace_and_never_includes_input_in_debug_or_errors() {
    let secret = secret("synthetic-private-paste");
    assert!(!format!("{secret:?}").contains("synthetic-private-paste"));
    let too_large =
        serde_json::to_string(&" ".repeat(SecretInput::MAX_BYTES + 1)).expect("fixture");
    let error = serde_json::from_str::<SecretInput>(&too_large).expect_err("byte bound");
    assert!(!error.to_string().contains(&" ".repeat(100)));
    assert!(SecretInput::new(Zeroizing::new("x".repeat(SecretInput::MAX_BYTES + 1))).is_err());
}
