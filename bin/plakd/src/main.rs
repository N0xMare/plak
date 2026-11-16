//! plakd command-line entrypoint over the same-package product library.

use std::process::ExitCode;

fn main() -> ExitCode {
    plakd::main_from_env()
}
