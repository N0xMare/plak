//! Plak command-line entrypoint over the same-package product library.

use std::process::ExitCode;

fn main() -> ExitCode {
    plak::main_from_env()
}
