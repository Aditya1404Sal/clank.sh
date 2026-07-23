//! Standalone `wcurl` CLI. The embeddable library ([`wcurl::run`]) does the work; this wrapper
//! just supplies a runtime, forwards argv, and writes the outcome. On wasm the runtime is wstd's
//! (the same reactor the WASI-HTTP client needs); on native it's a small tokio runtime.

use std::io::Write;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let outcome = block_on(wcurl::run(&args));

    let _ = std::io::stdout().write_all(&outcome.stdout);
    let _ = std::io::stderr().write_all(&outcome.stderr);
    std::process::exit(i32::from(outcome.exit_code));
}

#[cfg(target_arch = "wasm32")]
fn block_on(fut: impl std::future::Future<Output = wcurl::Outcome>) -> wcurl::Outcome {
    wstd::runtime::block_on(fut)
}

#[cfg(not(target_arch = "wasm32"))]
fn block_on(fut: impl std::future::Future<Output = wcurl::Outcome>) -> wcurl::Outcome {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(fut)
}
