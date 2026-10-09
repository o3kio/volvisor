//! # volvisord
//!
//! The Volvisor volume-virtualization daemon (P0 bootstrap).
//!
//! Startup order is safety-relevant and fixed:
//! 1. load and validate configuration;
//! 2. initialize structured logging (JSON, no secret material);
//! 3. acquire the journal lock and replay the intent journal;
//! 4. construct the configured provider and reconcile observed state;
//! 5. serve the Volume API v2 HTTP surface.
//!
//! Control-plane outage behavior (inherited v1 requirement, SPEC-0002
//! section 13): losing consumer connectivity never detaches healthy
//! volumes or revokes a valid writer; the daemon fails closed only for
//! new allocations and authority-changing operations.

#![deny(unsafe_code)]
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

pub mod config;
pub mod runtime;
pub mod witness;

use std::net::SocketAddr;
use std::time::{SystemTime, UNIX_EPOCH};

pub use config::Config;

/// Daemon runtime errors mapped to typed API errors where meaningful.
#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    /// Configuration was rejected.
    #[error("configuration error: {0}")]
    Config(String),
    /// The HTTP server failed.
    #[error("http server error: {0}")]
    Http(String),
}

/// Parsed bind address for the API surface.
#[derive(Debug, Clone, Copy)]
pub struct BindAddress(pub SocketAddr);

/// Wall-clock unix seconds for the witness-side clocks (lease expiry,
/// fence windows) and the writer's W5 deadline anchoring.
///
/// The witness's `lease_grace_secs` exists precisely to absorb the
/// skew and latency this clock introduces (P4a plan §3's documented
/// timing assumption); a monotonic clock cannot be used because lease
/// deadlines must survive process restarts.
pub(crate) fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

#[cfg(test)]
mod tests {
    #[test]
    fn placeholder() {
        // Replaced by config tests in config.rs.
    }
}
