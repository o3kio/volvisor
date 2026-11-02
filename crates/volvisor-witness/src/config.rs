//! # Witness daemon configuration
//!
//! TOML configuration for `volvisor-witnessd`. The lease knobs are
//! **witness-side** by design (P4a plan §3): the witness is what enforces
//! the TTL, the grace bound and the W7 fence wait, so it is what
//! configures them — a storage daemon never duplicates these values, it
//! validates its renewal interval against the TTL the witness reports.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use volvisor_types::HostId;
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
    /// non-loopback binds. Never logged. On a v2 witness this legacy
    /// token is **read-only** (inspect/health) — every mutation
    /// requires a host credential from `host_tokens` (W8).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_token: Option<String>,
    /// Per-host credentials (P4b plan §4 W8): a TOML table mapping a
    /// host id to that host's bearer token. A mutation is accepted
    /// only when the presented token resolves (server-side) to the
    /// host the request asserts. Never logged.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub host_tokens: BTreeMap<String, String>,
    /// Lease time-to-live in seconds (W5/W7).
    #[serde(default = "default_lease_ttl_secs")]
    pub lease_ttl_secs: u64,
    /// Response-latency bound the fence window assumes (W5).
    #[serde(default = "default_lease_grace_secs")]
    pub lease_grace_secs: u64,
    /// Self-fencing suspend budget in seconds (W7).
    #[serde(default = "default_suspend_budget_secs")]
    pub suspend_budget_secs: u64,
    /// Native TLS for the witness listener. Non-loopback plain HTTP is
    /// refused unless `allow_insecure` is explicitly set.
    #[serde(default)]
    pub tls: WitnessTlsConfig,
    /// Explicit cleartext-downgrade opt-out for a non-loopback listener.
    #[serde(default)]
    pub allow_insecure: bool,
}

/// Native TLS settings for the witness listener.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WitnessTlsConfig {
    /// Serve the witness over TLS.
    #[serde(default)]
    pub enabled: bool,
    /// PEM server certificate chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cert_file: Option<PathBuf>,
    /// PEM private key matching the served certificate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_file: Option<PathBuf>,
    /// Optional PEM client CA: when set, every storage daemon must present
    /// a certificate issued by one of these roots (mTLS).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_ca_file: Option<PathBuf>,
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
        self.validate_host_tokens()?;
        self.validate_transport()?;
        Ok(())
    }

    /// Transport posture: TLS fields are all-or-nothing (and their files
    /// must exist), and a non-loopback plain-HTTP listener is refused
    /// unless the operator explicitly set `allow_insecure`.
    fn validate_transport(&self) -> Result<(), ApiError> {
        if self.tls.enabled {
            validate_tls_file(self.tls.cert_file.as_ref(), "tls.cert_file")?;
            validate_tls_file(self.tls.key_file.as_ref(), "tls.key_file")?;
            if let Some(ca) = &self.tls.client_ca_file {
                if ca.as_os_str().is_empty() {
                    return Err(ApiError::invalid_request(
                        "witness tls.client_ca_file must not be empty when set",
                    ));
                }
                validate_tls_file(Some(ca), "tls.client_ca_file")?;
            }
        } else {
            if self.tls.cert_file.is_some()
                || self.tls.key_file.is_some()
                || self.tls.client_ca_file.is_some()
            {
                return Err(ApiError::invalid_request(
                    "witness tls.cert_file/key_file/client_ca_file are set while \
                     tls.enabled = false",
                ));
            }
            if !self.listen.ip().is_loopback() && !self.allow_insecure {
                return Err(ApiError::invalid_request(format!(
                    "witness listen {} is a non-loopback plain-HTTP bind; configure \
                     [tls] enabled = true or set allow_insecure = true",
                    self.listen
                )));
            }
        }
        Ok(())
    }

    /// Validate the per-host credential map (W8): every key must parse
    /// as a [`HostId`], every value must be non-empty, and no token
    /// value may be reused — neither by another host nor as the admin
    /// token — because identity resolution would be ambiguous (the
    /// same presented string must resolve to exactly one identity).
    fn validate_host_tokens(&self) -> Result<(), ApiError> {
        let mut seen_tokens = std::collections::BTreeSet::new();
        for (host, token) in &self.host_tokens {
            if HostId::new(host).is_err() {
                return Err(ApiError::invalid_request(format!(
                    "witness host_tokens key {host:?} is not a valid host id"
                )));
            }
            if token.trim().is_empty() {
                return Err(ApiError::invalid_request(format!(
                    "witness host_tokens key {host:?} requires a non-empty token"
                )));
            }
            if !seen_tokens.insert(token.as_str()) {
                return Err(ApiError::invalid_request(format!(
                    "witness host_tokens reuses a token value (host {host:?}); identity \
                     resolution must be unambiguous"
                )));
            }
            if self.auth_token.as_deref() == Some(token.as_str()) {
                return Err(ApiError::invalid_request(format!(
                    "witness host_tokens key {host:?} reuses the auth_token value; identity \
                     resolution must be unambiguous"
                )));
            }
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

/// Require a TLS path to be set and name a readable regular file.
fn validate_tls_file(path: Option<&PathBuf>, field: &str) -> Result<(), ApiError> {
    let path = path.ok_or_else(|| {
        ApiError::invalid_request(format!(
            "witness {field} is required when tls.enabled = true"
        ))
    })?;
    if path.as_os_str().is_empty() {
        return Err(ApiError::invalid_request(format!(
            "witness {field} must not be empty when set"
        )));
    }
    if !path.is_file() {
        return Err(ApiError::invalid_request(format!(
            "witness {field} {} does not exist or is not a regular file",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(listen: &str) -> WitnessConfig {
        WitnessConfig {
            listen: listen.parse().expect("socket addr"),
            state_dir: PathBuf::from("/tmp/opencode/witness-state"),
            auth_token: None,
            host_tokens: BTreeMap::new(),
            lease_ttl_secs: DEFAULT_LEASE_TTL_SECS,
            lease_grace_secs: DEFAULT_LEASE_GRACE_SECS,
            suspend_budget_secs: DEFAULT_SUSPEND_BUDGET_SECS,
            tls: WitnessTlsConfig::default(),
            allow_insecure: false,
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
        assert!(
            cfg.validate().is_err(),
            "a token alone does not authorize plain HTTP on a public bind"
        );
        cfg.allow_insecure = true;
        assert!(cfg.validate().is_ok());
        // An empty token is no token.
        cfg.auth_token = Some("  ".to_owned());
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn non_loopback_plain_http_needs_an_explicit_allow_insecure() {
        let mut cfg = config("0.0.0.0:9101");
        cfg.auth_token = Some("secret".to_owned());
        let error = cfg.validate().expect_err("plain HTTP public bind");
        assert!(
            error.to_string().contains("allow_insecure = true"),
            "error names the opt-in: {error}"
        );
        cfg.allow_insecure = true;
        cfg.validate().expect("the explicit downgrade is accepted");
    }

    #[test]
    fn tls_listener_requires_readable_certificate_and_key() {
        let mut cfg = config("127.0.0.1:9101");
        cfg.tls.enabled = true;
        let error = cfg.validate().expect_err("missing TLS identity");
        assert!(
            error.to_string().contains("tls.cert_file"),
            "error names the missing field: {error}"
        );

        let dir = tempfile::tempdir().expect("tempdir");
        let cert = dir.path().join("witness.crt");
        let key = dir.path().join("witness.key");
        std::fs::write(&cert, b"certificate").expect("write cert");
        std::fs::write(&key, b"key").expect("write key");
        cfg.tls.cert_file = Some(cert);
        cfg.tls.key_file = Some(key);
        cfg.validate()
            .expect("an existing cert/key pair is accepted");
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
        assert!(parsed.host_tokens.is_empty());
        assert!(parsed.validate().is_ok());
        // Unknown fields are rejected, never silently ignored.
        let foreign = r#"
listen = "127.0.0.1:9101"
state_dir = "/tmp/opencode/witness-state"
mystery = true
"#;
        assert!(toml::from_str::<WitnessConfig>(foreign).is_err());
    }

    #[test]
    fn host_tokens_parse_and_round_trip() {
        let toml_source = r#"
listen = "127.0.0.1:9101"
state_dir = "/tmp/opencode/witness-state"

[host_tokens]
node-a = "token-a"
node-b = "token-b"
"#;
        let parsed: WitnessConfig = toml::from_str(toml_source).expect("parse");
        assert_eq!(
            parsed.host_tokens.get("node-a").map(String::as_str),
            Some("token-a")
        );
        assert!(parsed.validate().is_ok(), "{parsed:?}");
        // Round trip: the table serializes back.
        let serialized = toml::to_string(&parsed).expect("serialize");
        let reparsed: WitnessConfig = toml::from_str(&serialized).expect("reparse");
        assert_eq!(reparsed, parsed);
    }

    #[test]
    fn host_tokens_validation_is_fail_closed() {
        // A key that is not a valid host id.
        let mut cfg = config("127.0.0.1:9101");
        cfg.host_tokens
            .insert("not/a/host".to_owned(), "t".to_owned());
        assert!(cfg.validate().is_err());
        // An empty token value.
        let mut cfg = config("127.0.0.1:9101");
        cfg.host_tokens.insert("node-a".to_owned(), "  ".to_owned());
        assert!(cfg.validate().is_err());
        // Two hosts sharing one token: ambiguous identity resolution.
        let mut cfg = config("127.0.0.1:9101");
        cfg.host_tokens
            .insert("node-a".to_owned(), "same".to_owned());
        cfg.host_tokens
            .insert("node-b".to_owned(), "same".to_owned());
        assert!(cfg.validate().is_err());
        // A host token that equals the admin token.
        let mut cfg = config("127.0.0.1:9101");
        cfg.auth_token = Some("admin".to_owned());
        cfg.host_tokens
            .insert("node-a".to_owned(), "admin".to_owned());
        assert!(cfg.validate().is_err());
        // Distinct tokens for distinct hosts: valid.
        cfg.host_tokens
            .insert("node-a".to_owned(), "host-a".to_owned());
        assert!(cfg.validate().is_ok(), "{cfg:?}");
    }
}
