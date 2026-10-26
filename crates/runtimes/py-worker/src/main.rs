//! `plakous-py-host`: the Monty cell worker. It speaks the cell protocol on
//! stdin and stdout and drives the pinned upstream `monty_proto` child state
//! machine in process. Diagnostics go to stderr; stdout carries only frames.

use std::{io, panic, process::ExitCode, thread};

use plakous_cell::{LossClass, WorkerFrame, write_frame_blocking};

mod cell;
mod codec;
mod prompts;
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
static ALLOC: monty_alloc::LimitedAllocator = monty_alloc::LimitedAllocator;

/// Native stack for the worker thread. The interpreter's container `repr` and
/// `==` and the value exporter recurse once per nesting level, up to the
/// 1000-frame recursion limit. The memory is reserved, not committed.
const WORKER_STACK_SIZE: usize = 16 * 1024 * 1024;

fn main() -> ExitCode {
    if test_hooks::probe() {
        return ExitCode::SUCCESS;
    }
    install_panic_hook();
    match thread::Builder::new()
        .name("plakous-py-host".to_owned())
        .stack_size(WORKER_STACK_SIZE)
        .spawn(cell::serve)
    {
        // A panic has already written its loss frame in the hook.
        Ok(worker) => worker.join().unwrap_or(ExitCode::from(101)),
        Err(error) => {
            eprintln!("plakous-py-host: failed to spawn the worker thread: {error}");
            ExitCode::FAILURE
        }
    }
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
