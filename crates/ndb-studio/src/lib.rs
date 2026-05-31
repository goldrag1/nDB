//! nDB Studio — open any nDB as familiar tables, project it creatively, and
//! edit it as versioned commits with time-travel, with the engine in-process.
//!
//! Layers:
//! - [`store`] — the engine host + catalog (the only code that touches
//!   `ndb-engine`); everything above it speaks `serde_json`.
//! - [`jsonval`] — `Value` ⇆ JSON conversion.
//! - [`http`] — the local HTTP API and the embedded single-file web UI. This
//!   module is the single frontend↔backend seam a later Tauri shell swaps.

pub mod http;
pub mod jsonval;
pub mod store;
