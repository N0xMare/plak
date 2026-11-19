//! `plakous-py-host`: the micro-monty worker on the Plak cell protocol.
//! Diagnostics go to stderr; stdout carries only frames.

use std::{alloc::System, io, panic, process::ExitCode};

use plakous_cell::{EngineConfig, LossClass, WorkerFrame, write_frame_blocking};

mod cell;
mod codec;
#[cfg(all(feature = "test-hooks", debug_assertions))]
mod test_hooks;
#[cfg(not(all(feature = "test-hooks", debug_assertions)))]
mod test_hooks {
    pub(super) fn boundary(_name: &str) {}
    pub(super) fn probe() -> bool {
        false
    }
}

#[global_allocator]
static ALLOC: cap::Cap<System> = cap::Cap::new(System, HARD_MEMORY_BYTES);

const HARD_MEMORY_BYTES: usize = 656 * 1024 * 1024;

const _: () = assert!(matches!(
    EngineConfig::MONTY,
    EngineConfig::Monty { memory_bytes } if memory_bytes * 5 / 2 + 16 * 1024 * 1024 == HARD_MEMORY_BYTES as u64
));

fn main() -> ExitCode {
    if test_hooks::probe() {
        return ExitCode::SUCCESS;
    }
    install_panic_hook();
    // The interpreter and global capped allocator run on one thread.
    panic::catch_unwind(cell::serve).unwrap_or(ExitCode::from(101))
}

/// Writes a best-effort `HostLost` frame before the default unwind, so the
/// parent gets a parseable last gasp for ordinary panics. Hard crashes (stack
/// overflow, allocator abort) bypass this: the parent treats a stream that
/// ends without a terminal as loss.
fn install_panic_hook() {
    let default_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        let _ = write_frame_blocking(
            &mut io::stdout(),
            &WorkerFrame::HostLost {
                class: LossClass::EngineFailure,
                message: format!("Python worker panicked: {info}"),
            },
        );
        default_hook(info);
    }));
}
