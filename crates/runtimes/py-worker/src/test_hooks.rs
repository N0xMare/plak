//! Deterministic crash boundaries in the real worker, absent from ordinary builds.

pub(super) fn probe() -> bool {
    if std::env::args().nth(1).as_deref() != Some("--test-cell-hooks") {
        return false;
    }
    println!("cell crash hooks enabled");
    true
}

pub(super) fn boundary(name: &str) {
    if std::env::var("PLAK_TEST_CELL_CUT").as_deref() == Ok(name)
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
