//! # Witness daemon configuration
//!
//! TOML configuration for `volvisor-witnessd`. The lease knobs are
//! **witness-side** by design (P4a plan §3): the witness is what enforces
//! the TTL, the grace bound and the W7 fence wait, so it is what
//! configures them — a storage daemon never duplicates these values, it
//! validates its renewal interval against the TTL the witness reports.

use std::net::SocketAddr;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use volvisor_types::error::ApiError;

/// Default lease TTL (seconds). Conservative; deployments tune it.
pub const DEFAULT_LEASE_TTL_SECS: u64 = 60;

/// Default grace bound (seconds): the response-latency bound the fence
/// window assumes (W5). Must cover witness round-trip latency under
/// load; it is an operator network responsibility, documented here.
pub const DEFAULT_LEASE_GRACE_SECS: u64 = 5;

/// Default suspend budget (seconds): the assumed time for a
/// self-fencing writer to freeze its data path (`drbdsetup suspend-io`),
/// which is fast — demotion of an open device is deliberately not
/// budgeted because a suspended device is already write-frozen.
pub const DEFAULT_SUSPEND_BUDGET_SECS: u64 = 5;

/// Witness daemon configuration (TOML at `--config`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WitnessConfig {
    /// HTTP bind address for the witness surface. A tokenless
    /// configuration may bind loopback only (the binder refuses
    /// non-loopback binds without a token, mirroring the storage
    /// daemon's convention).
    pub listen: SocketAddr,
    /// Directory holding the witness journal and lock file.
    pub state_dir: PathBuf,
    /// Bearer token protecting the witness surface; required for
    /// non-loopback binds. Never logged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_token: Option<String>,
    /// Lease time-to-live in seconds (W5/W7).
    #[serde(default = "default_lease_ttl_secs")]
    pub lease_ttl_secs: u64,
    /// Response-latency bound the fence window assumes (W5).
    #[serde(default = "default_lease_grace_secs")]
    pub lease_grace_secs: u64,
    /// Self-fencing suspend budget in seconds (W7).
    #[serde(default = "default_suspend_budget_secs")]
    pub suspend_budget_secs: u64,
}

fn default_lease_ttl_secs() -> u64 {
    DEFAULT_LEASE_TTL_SECS
}

fn default_lease_grace_secs() -> u64 {
    DEFAULT_LEASE_GRACE_SECS
}

fn default_suspend_budget_secs() -> u64 {
    DEFAULT_SUSPEND_BUDGET_SECS
}

impl WitnessConfig {
    /// Validate: positive lease knobs (a zero TTL, grace or budget would
    /// make the fence window vacuous or the lease immortal), and a token
    /// for any non-loopback bind.
    ///
    /// # Errors
    /// Typed invalid-request error naming the violated rule. The witness
    /// binary turns this into a startup refusal — a misconfigured
    /// witness never starts half-safe.
    pub fn validate(&self) -> Result<(), ApiError> {
        let core = crate::registry::WitnessCoreConfig {
            lease_ttl_secs: self.lease_ttl_secs,
            lease_grace_secs: self.lease_grace_secs,
            suspend_budget_secs: self.suspend_budget_secs,
        };
        core.validate()?;
        if self
            .auth_token
            .as_deref()
            .is_none_or(|token| token.trim().is_empty())
            && !self.listen.ip().is_loopback()
        {
            return Err(ApiError::invalid_request(
                "witness auth_token is required for non-loopback binds",
            ));
        }
        Ok(())
    }

    /// The authority-core tuning derived from this configuration.
    #[must_use]
    pub fn core_config(&self) -> crate::registry::WitnessCoreConfig {
        crate::registry::WitnessCoreConfig {
            lease_ttl_secs: self.lease_ttl_secs,
            lease_grace_secs: self.lease_grace_secs,
            suspend_budget_secs: self.suspend_budget_secs,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(listen: &str) -> WitnessConfig {
        WitnessConfig {
            listen: listen.parse().expect("socket addr"),
            state_dir: PathBuf::from("/tmp/opencode/witness-state"),
            auth_token: None,
            lease_ttl_secs: DEFAULT_LEASE_TTL_SECS,
            lease_grace_secs: DEFAULT_LEASE_GRACE_SECS,
            suspend_budget_secs: DEFAULT_SUSPEND_BUDGET_SECS,
        }
    }

    #[test]
    fn loopback_binds_without_token_are_valid() {
        assert!(config("127.0.0.1:9101").validate().is_ok());
        assert!(config("[::1]:9101").validate().is_ok());
    }

    #[test]
    fn non_loopback_binds_require_a_token() {
        let mut cfg = config("0.0.0.0:9101");
        assert!(cfg.validate().is_err());
        cfg.auth_token = Some("secret".to_owned());
        assert!(cfg.validate().is_ok());
        // An empty token is no token.
        cfg.auth_token = Some("  ".to_owned());
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn lease_knobs_must_be_positive() {
        let mut cfg = config("127.0.0.1:9101");
        cfg.lease_ttl_secs = 0;
        assert!(cfg.validate().is_err());
        cfg.lease_ttl_secs = 60;
        cfg.suspend_budget_secs = 0;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn toml_round_trip_with_defaults() {
        let toml_source = r#"
listen = "127.0.0.1:9101"
state_dir = "/tmp/opencode/witness-state"
"#;
        let parsed: WitnessConfig = toml::from_str(toml_source).expect("parse");
        assert_eq!(parsed.lease_ttl_secs, DEFAULT_LEASE_TTL_SECS);
        assert_eq!(parsed.lease_grace_secs, DEFAULT_LEASE_GRACE_SECS);
        assert_eq!(parsed.suspend_budget_secs, DEFAULT_SUSPEND_BUDGET_SECS);
        assert!(parsed.validate().is_ok());
        // Unknown fields are rejected, never silently ignored.
        let foreign = r#"
listen = "127.0.0.1:9101"
state_dir = "/tmp/opencode/witness-state"
mystery = true
"#;
        assert!(toml::from_str::<WitnessConfig>(foreign).is_err());
    }
}
