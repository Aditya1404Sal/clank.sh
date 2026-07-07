//! Native `clank` binary entrypoint.
//!
//! On native targets this runs the blocking `std::io` shell loop. On wasm the entrypoint
//! is the exported `wasi:cli/run` (see `lib.rs`/`wasm.rs`), not `main`, so `main` is empty
//! there; the canonical wasm build is `--lib`.

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    if let Err(e) = clank_shell::native::run() {
        eprintln!("clank: {e}");
        std::process::exit(1);
    }
}

#[cfg(target_arch = "wasm32")]
fn main() {}
