//! Shared plumbing for all Tratschkathl processes: paths, the bus protocol and client,
//! version state, settings, terminal frame types and process helpers.

pub mod client;
pub mod frame;
pub mod logging;
pub mod model;
pub mod paths;
pub mod proto;
pub mod settings;
pub mod util;
pub mod versions;

pub use client::{Bus, Handler, Incoming};
pub use serde_json::{json, Value};

/// Bus protocol version (major, minor). A major mismatch between running units forces them
/// into the same reload set.
pub const PROTO: (u32, u32) = (1, 0);

/// Exit code Core uses to ask the launcher for a reboot into the current Core version.
pub const EXIT_REBOOT: i32 = 75;

/// Standard `--self-test` output for a unit binary.
pub fn self_test_json(unit: &str) -> String {
    serde_json::json!({ "unit": unit, "proto": [PROTO.0, PROTO.1], "version": env!("CARGO_PKG_VERSION") })
        .to_string()
}
