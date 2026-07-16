//! clank-conformance — one scenario corpus, two shell targets.
//!
//! `.clank` files under `scenarios/` are the behavioral spec: each is a transcript of
//! `run`/`answer`/`abort` steps with expected stdout/stderr/exit-code/pending-prompt.
//! The same file is executed against the native in-process [`clank_shell::session::Session`]
//! (`cargo test -p clank-conformance --test native`) and against a deployed clank Golem agent
//! via the `golem` CLI (`--test golem`, enabled by `CLANK_CONFORMANCE_GOLEM=1` — see
//! `scripts/conformance-golem.sh`). The format spec lives in `scenarios/README.md`.

pub mod backend;
pub mod harness;
pub mod matcher;
pub mod scenario;
