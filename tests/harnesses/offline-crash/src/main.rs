//! Thin process entry point for the offline Plakous crash harness.

#![forbid(unsafe_code)]

#[tokio::main]
async fn main() {
    if std::env::args().nth(1).as_deref() != Some("--child") {
        eprintln!("usage: plakous-offline-crash-harness --child < command.jsonl");
        std::process::exit(2);
    }
    std::process::exit(plakous_offline_crash_harness::child_main().await);
}
