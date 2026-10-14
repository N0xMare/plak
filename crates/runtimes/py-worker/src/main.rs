//! Process-only Monty entry point. The interpreter and wire state machine are upstream.

use std::process::ExitCode;

mod subprocess;

#[global_allocator]
static ALLOC: monty_alloc::LimitedAllocator = monty_alloc::LimitedAllocator;

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() == 1 && args[0] == "--version" {
        println!("monty 1.1.0");
        ExitCode::SUCCESS
    } else if args.len() == 1 && args[0] == "subprocess" {
        subprocess::run()
    } else {
        eprintln!("usage: monty subprocess | --version");
        ExitCode::FAILURE
    }
}
