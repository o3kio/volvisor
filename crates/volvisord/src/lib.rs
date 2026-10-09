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

use std::net::SocketAddr;

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

#[cfg(test)]
mod tests {
    #[test]
    fn placeholder() {
        // Replaced by config tests in config.rs.
    }
}
