//! What a test does when something it needs isn't on this machine.
#![allow(dead_code)]
use std::io::Write;

/// True on GitHub Actions (and most other CI), which sets `CI`. CI installs the tools the tests need, so a tool
/// missing there is a broken workflow, and a test that quietly skipped would leave the run green having proved nothing.
pub fn in_ci() -> bool {
    std::env::var_os("CI").is_some_and(|value| !value.is_empty() && value != "false")
}

/// Call when `needed` is missing, then return from the test. With `required` the test fails instead of skipping.
/// The skip is written straight to the terminal, not through `eprintln!`: libtest hides what a passing test prints,
/// and a skip nobody sees reads as a pass.
pub fn skip(test: &str, needed: &str, required: bool) {
    if required {
        panic!("{test} needs {needed}, which is missing here");
    }
    let _ = writeln!(std::io::stderr(), "\nSKIPPED {test}: {needed} is missing; the test did not run.");
}
