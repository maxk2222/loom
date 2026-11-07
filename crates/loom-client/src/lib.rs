//! loom-client library: workstation daemon internals.

pub mod adapters;
pub mod backend_manager;
pub mod config;
pub mod connector;
pub mod local_api;
pub mod local_ui;
pub mod notes;
pub mod scheduler;
pub mod sysmon;
pub mod telemetry;
#[cfg(feature = "tray")]
pub mod tray;
pub mod updater;
