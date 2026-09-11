//! Native `clank` binary entrypoint.
//!
//! On native targets this drives the async `std::io` shell loop (Brush is async) on a tokio
//! runtime, via `clank_native::run`. There is no wasm counterpart to this binary: the agent is a
//! component (`crates/clank-agent`) whose entrypoints are its exported agent methods, not a
//! `wasi:cli/run` command. `main` is empty on wasm purely so the crate still builds there.

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

    if let Err(e) = runtime.block_on(clank_native::run()) {
        eprintln!("clank: {e}");
        std::process::exit(1);
    }
}

#[cfg(target_arch = "wasm32")]
fn main() {}
