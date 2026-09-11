//! Golem interaction: remote agent invocation ([`agent`]) and the `golem` cluster command ([`cluster`]).
//!
//! Both are dependency-injection seams — `clank-core` defines the traits here. The durable Golem-host
//! bindings are implemented in the `clank-agent` crate (wasm) and injected into the `Session`; the
//! native+cluster REST implementations (`rest`) and the external cluster-config reader
//! (`cluster_config`) live in the separate `clank-native` crate.

pub mod agent;
pub mod cluster;
pub mod error;

pub use error::Error;
// The native+cluster path — an external cluster-config reader and REST-backed `AgentInvoker`/
// `GolemCluster` impls that talk to a Golem cluster over its HTTP API — now lives in the separate
// `clank-native` crate (`cluster_config` and `rest` there), not here. It is cfg-gated there so
// `reqwest` never reaches the wasm build; inert unless a cluster config is present.
