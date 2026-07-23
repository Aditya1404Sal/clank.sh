//! Native `clank` binary entrypoint.
//!
//! On native targets this drives the async `std::io` shell loop (Brush is async) on a tokio
//! runtime. On wasm the entrypoint is the exported `wasi:cli/run` (see `lib.rs`/`wasm.rs`),
//! not `main`, so `main` is empty there; the canonical wasm build is `--lib`.

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    // `Runtime::new()` enables all drivers (I/O + time), which Brush needs to spawn external
    // processes via tokio.
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("clank: failed to start runtime: {e}");
            std::process::exit(1);
        }
    };

    if let Err(e) = runtime.block_on(clank_core::native::run()) {
        eprintln!("clank: {e}");
        std::process::exit(1);
    }
}

#[cfg(target_arch = "wasm32")]
fn main() {}
