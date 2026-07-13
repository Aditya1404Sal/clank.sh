//! Golem interaction: remote agent invocation ([`agent`]) and the `golem` cluster command ([`cluster`]).
//!
//! Both are dependency-injection seams — `clank-shell` defines the traits here; the durable Golem-host
//! bindings are implemented in the `clank-agent` crate and injected into the `Session`.

pub mod agent;
pub mod cluster;
