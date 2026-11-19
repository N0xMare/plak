//! Deterministic crash boundaries in the real worker, absent from ordinary builds.

pub(super) fn probe() -> bool {
    if std::env::args().nth(1).as_deref() != Some("--test-cell-hooks") {
        return false;
    }
    println!("cell crash hooks enabled");
    true
}

pub(super) fn boundary(name: &str) {
    let mode = std::env::var("PLAK_TEST_CELL_CUT");
    if name == "before_delivery" && mode.as_deref() == Ok("allocator_before_delivery") {
        let allocation = Vec::<u8>::with_capacity(crate::HARD_MEMORY_BYTES + 1);
        std::hint::black_box(allocation);
    }
    if mode.as_deref() == Ok(name)
        && let Ok(ready) = std::env::var("PLAK_TEST_CELL_CUT_READY")
    {
        if std::fs::write(ready, name).is_err() {
            std::process::exit(5);
        }
        loop {
            std::thread::park();
        }
    }
}
