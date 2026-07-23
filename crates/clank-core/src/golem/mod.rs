//! Golem interaction: remote agent invocation ([`agent`]) and the `golem` cluster command ([`cluster`]).
//!
//! Both are dependency-injection seams — `clank-core` defines the traits here. The durable Golem-host
//! bindings are implemented in the `clank-agent` crate (wasm) and injected into the `Session`; the
//! native+cluster REST implementations live in [`rest_native`], gated on an external cluster config
//! ([`config_native`]).

pub mod agent;
pub mod cluster;
// The native+cluster path: an external cluster-config reader ([`config_native`]) and REST-backed
// `AgentInvoker`/`GolemCluster` impls ([`rest_native`]) that talk to a Golem cluster over its HTTP
// API. cfg-gated so `reqwest` never reaches the wasm build; inert unless a cluster config is present.
#[cfg(not(target_arch = "wasm32"))]
pub mod config_native;
#[cfg(not(target_arch = "wasm32"))]
pub mod rest_native;
