//! Standalone `waget` CLI. The embeddable library ([`waget::run`]) does the work; this wrapper
//! supplies a runtime, forwards argv, and writes the outcome (wstd's reactor on wasm, tokio native).

use std::io::Write;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let outcome = block_on(waget::run(&args));

    let _ = std::io::stdout().write_all(&outcome.stdout);
    let _ = std::io::stderr().write_all(&outcome.stderr);
    std::process::exit(i32::from(outcome.exit_code));
}

#[cfg(target_arch = "wasm32")]
fn block_on(fut: impl std::future::Future<Output = waget::Outcome>) -> waget::Outcome {
    wstd::runtime::block_on(fut)
}

#[cfg(not(target_arch = "wasm32"))]
fn block_on(fut: impl std::future::Future<Output = waget::Outcome>) -> waget::Outcome {
    // Runtime construction failure is unrecoverable in the standalone CLI: fail fast.
    #[allow(clippy::expect_used)]
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(fut)
}
