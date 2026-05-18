//! Shared library for the `cmmd` daemon and the `mmctl` CLI.
//!
//! Keeping these in a lib lets `mmctl` reuse the same `DaemonStatus` shape and
//! Unix socket client that `cmmd` exposes, so the two stay in lockstep.

pub mod audit;
pub mod authmux;
pub mod config;
pub mod history;
pub mod ipc;
pub mod janitor;
pub mod memory;
pub mod process;
pub mod state;
pub mod tick;
