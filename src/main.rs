//! Isolera command-line entry point.
//!
//! The argument parser and every command live in [`isolera::cli`]; this binary is
//! a thin wrapper that maps the returned [`isolera::cli::Outcome`] onto a process
//! exit code, so a provisioning script can branch on the failure category.

fn main() {
    std::process::exit(isolera::cli::main());
}
