//! The golem conformance tier: every `scenarios/*.clank` against a DEPLOYED clank agent
//! through the `golem` CLI.
//!
//! Requires a running server with clank deployed; trials are reported `ignored` unless
//! `CLANK_CONFORMANCE_GOLEM=1`. The normal entry point is `scripts/conformance-golem.sh`,
//! which stands up a throwaway server, deploys, and runs this binary.

fn main() {
    clank_conformance::harness::main(clank_conformance::backend::BackendKind::Golem)
}
