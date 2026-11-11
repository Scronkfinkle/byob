//! The sheets backend as a JSON-lines process (`docs/plugins-backend.md`): what byob runs
//! in-process, here on stdin/stdout so tests and benches can drive it like the Python one.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

fn main() -> std::process::ExitCode {
    match byob_sheets::serve_stdio() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("byob-sheets-backend: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
