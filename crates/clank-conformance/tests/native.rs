//! The native conformance tier: every `scenarios/*.clank` against an in-process Session.
//!
//! `cargo test -p clank-conformance --test native` — no server, no network.

fn main() {
    clank_conformance::harness::main(clank_conformance::backend::BackendKind::Native)
}
