//! Daemon configuration.
//!
//! Configuration is explicit and validated at startup; defaults are
//! conservative. No secret material is ever logged (SPEC-0002 section 9).

use std::net::SocketAddr;
use std::path::PathBuf;

use serde::Deserialize;

use crate::DaemonError;

/// Provider backend selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// In-memory fake provider (testing only; never production).
    Fake,
    /// Native-local LVM provider (P1 prototype).
    Lvm,
    /// External-cluster Ceph RBD adapter (P2 prototype): volumes are
    /// RBD images in one pool of a Ceph cluster operated outside
    /// volvisor, verified fail-closed at startup.
    Ceph,
}

/// Default Ceph entity name (`--name`) when `ceph_user` is unset.
///
/// The ceph CLI resolves the matching keyring itself (CEPH_CONF /
/// keyring conventions); volvisor only passes `--name` and `-m` and never
/// reads or logs credential material.
pub const DEFAULT_CEPH_USER: &str = "client.volvisor";

/// Daemon configuration (TOML file at `--config`).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// HTTP bind address for the Volume API v2 surface.
    pub listen: SocketAddr,
    /// Directory holding the intent journal and lock file.
    pub journal_dir: PathBuf,
    /// Selected provider backend.
    pub provider: ProviderKind,
    /// LVM volume-group prefix used for claimed pools (LVM provider only).
    pub lvm_vg_prefix: Option<String>,
    /// Scoped destructive-authorization token for device claim/release
    /// (LVM provider only; required for that provider, never logged).
    pub device_claim_token: Option<String>,
    /// Durable LVM provider state path (defaults to
    /// `<journal_dir>/lvm-state.json`).
    pub lvm_state_path: Option<std::path::PathBuf>,
    /// Cluster FSID the ceph provider may operate on (ceph provider
    /// only; must match the cluster's reported fsid exactly or startup
    /// is refused — a mis-pointed cluster is never adopted).
    pub ceph_cluster_fsid: Option<String>,
    /// Ceph monitor addresses, each `host`, `host:port` or a bracketed
    /// IPv6 literal (ceph provider only; 1..=9 entries, joined into the
    /// `-m` flag of every invocation).
    pub ceph_mon_hosts: Option<Vec<String>>,
    /// The single RBD pool volumes are created in (ceph provider only).
    pub ceph_pool: Option<String>,
    /// The Ceph entity name passed as `--name` (ceph provider only; defaults
    /// to [`DEFAULT_CEPH_USER`]). Must be a full `client.<id>` entity name —
    /// `--name` takes the complete form, unlike the bare-id `--id` flag.
    /// The ceph CLI resolves the keyring itself; volvisor never reads or
    /// logs key material.
    pub ceph_user: Option<String>,
    /// Durable ceph provider state path (defaults to
    /// `<journal_dir>/ceph-state.json`).
    pub ceph_state_path: Option<std::path::PathBuf>,
    /// Filesystem root for read-only device discovery (defaults to `/`;
    /// test isolation only).
    pub sysfs_root: Option<std::path::PathBuf>,
    /// Static bearer token guarding the API's privileged surface (every
    /// mutating endpoint and the whole `/v2/admin` route group, `GET`
    /// included). When unset, those endpoints reject every request with
    /// `401` (fail closed) and the daemon refuses to bind a non-loopback
    /// address: the tokenless mode is loopback-only dev/test. Never logged.
    #[serde(default)]
    pub admin_token: Option<String>,
    /// Maximum request body size in bytes.
    #[serde(default = "default_max_body_bytes")]
    pub max_body_bytes: usize,
}

fn default_max_body_bytes() -> usize {
    1 << 20
}

impl Config {
    /// Load and validate configuration from a TOML file.
    ///
    /// # Errors
    /// Returns [`DaemonError::Config`] when the file is unreadable,
    /// malformed or fails validation.
    pub fn load(path: &std::path::Path) -> Result<Self, DaemonError> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| DaemonError::Config(format!("cannot read {}: {e}", path.display())))?;
        let cfg: Self = toml::from_str(&raw)
            .map_err(|e| DaemonError::Config(format!("cannot parse {}: {e}", path.display())))?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Validate cross-field constraints.
    fn validate(&self) -> Result<(), DaemonError> {
        if self.provider == ProviderKind::Lvm {
            if self.lvm_vg_prefix.is_none() {
                return Err(DaemonError::Config(
                    "lvm_vg_prefix is required for the lvm provider".to_owned(),
                ));
            }
            if self.device_claim_token.as_deref().unwrap_or("").is_empty() {
                return Err(DaemonError::Config(
                    "device_claim_token is required for the lvm provider (scoped destructive \
                     authorization)"
                        .to_owned(),
                ));
            }
        }
        if self.provider == ProviderKind::Ceph {
            if self.ceph_cluster_fsid.is_none() {
                return Err(DaemonError::Config(
                    "ceph_cluster_fsid is required for the ceph provider".to_owned(),
                ));
            }
            if self.ceph_mon_hosts.is_none() {
                return Err(DaemonError::Config(
                    "ceph_mon_hosts is required for the ceph provider".to_owned(),
                ));
            }
            if self.ceph_pool.is_none() {
                return Err(DaemonError::Config(
                    "ceph_pool is required for the ceph provider".to_owned(),
                ));
            }
        }
        if let Some(prefix) = &self.lvm_vg_prefix {
            if !is_simple_name(prefix, 64) {
                return Err(DaemonError::Config(
                    "lvm_vg_prefix must be 1..=64 characters of alnum, '-' and '_'".to_owned(),
                ));
            }
        }
        if let Some(fsid) = &self.ceph_cluster_fsid {
            if !is_canonical_uuid(fsid) {
                return Err(DaemonError::Config(
                    "ceph_cluster_fsid must be a canonical UUID (8-4-4-4-12 hex groups)".to_owned(),
                ));
            }
        }
        if let Some(mons) = &self.ceph_mon_hosts {
            if mons.is_empty() || mons.len() > 9 {
                return Err(DaemonError::Config(
                    "ceph_mon_hosts must contain between 1 and 9 entries".to_owned(),
                ));
            }
            if mons.iter().any(|mon| !is_valid_mon_host(mon)) {
                return Err(DaemonError::Config(
                    "each ceph_mon_hosts entry must be `host`, `host:port` or a bracketed IPv6 \
                     literal like [::1]:6789 (port 1..=65535)"
                        .to_owned(),
                ));
            }
        }
        if let Some(pool) = &self.ceph_pool {
            if !is_simple_name(pool, 64) {
                return Err(DaemonError::Config(
                    "ceph_pool must be 1..=64 characters of alnum, '-' and '_'".to_owned(),
                ));
            }
        }
        if let Some(user) = &self.ceph_user {
            // `--name` takes the FULL entity name; a bare id (or a non-client
            // entity type) is a configuration mistake that would silently
            // authenticate as the wrong principal.
            let valid = user.strip_prefix("client.").is_some_and(|id| {
                !id.is_empty()
                    && !id.chars().any(char::is_whitespace)
                    && id
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            });
            if !valid {
                return Err(DaemonError::Config(
                    "ceph_user must be a full client entity name like 'client.volvisor' \
                     (client. prefix plus a non-empty id of alnum, '-', '_' or '.') when set \
                     (leave it unset for the documented default)"
                        .to_owned(),
                ));
            }
        }
        // An explicitly empty token is a configuration mistake: unset means
        // "fail closed, loopback only", while "" would authenticate an empty
        // bearer. Reject it at startup instead.
        if self.admin_token.as_deref().is_some_and(str::is_empty) {
            return Err(DaemonError::Config(
                "admin_token must not be empty when set (leave it unset for the loopback-only \
                 fail-closed mode)"
                    .to_owned(),
            ));
        }
        Ok(())
    }

    /// The effective Ceph user id: the configured value, or the
    /// documented default [`DEFAULT_CEPH_USER`].
    ///
    /// The keyring is resolved by the ceph CLI itself (volvisor only
    /// passes `--name` (full entity); no credential material is ever read or
    /// logged).
    #[must_use]
    pub fn ceph_user_or_default(&self) -> &str {
        self.ceph_user.as_deref().unwrap_or(DEFAULT_CEPH_USER)
    }
}

/// Whether `value` is 1..=`max_len` characters of ASCII alphanumerics,
/// `-` and `_` (the LVM volume-group-prefix and Ceph pool-name rule).
fn is_simple_name(value: &str, max_len: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_len
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Whether `value` is a canonical hyphenated UUID (8-4-4-4-12 ASCII
/// hex groups), the format `ceph fsid` reports and the configuration
/// must match exactly.
fn is_canonical_uuid(value: &str) -> bool {
    const GROUPS: [usize; 5] = [8, 4, 4, 4, 12];
    let mut rest = value;
    for (index, len) in GROUPS.iter().enumerate() {
        if index > 0 {
            let Some(tail) = rest.strip_prefix('-') else {
                return false;
            };
            rest = tail;
        }
        let Some((group, tail)) = rest.split_at_checked(*len) else {
            return false;
        };
        if !group.bytes().all(|b| b.is_ascii_hexdigit()) {
            return false;
        }
        rest = tail;
    }
    rest.is_empty()
}

/// Whether `entry` is a valid Ceph monitor address: `host`,
/// `host:port`, or a bracketed IPv6 literal (`[::1]`, `[::1]:6789`).
///
/// A port must be a decimal 1..=65535; unbracketed IPv6 literals are
/// ambiguous against `host:port` and must be bracketed.
fn is_valid_mon_host(entry: &str) -> bool {
    if entry.is_empty() || entry.chars().any(char::is_whitespace) {
        return false;
    }
    if let Some(rest) = entry.strip_prefix('[') {
        let Some((host, tail)) = rest.split_once(']') else {
            return false;
        };
        if host.is_empty() {
            return false;
        }
        return match tail.strip_prefix(':') {
            Some(port) => is_valid_port(port),
            None => tail.is_empty(),
        };
    }
    match entry.split_once(':') {
        Some((host, port)) => !host.is_empty() && is_valid_port(port),
        None => true,
    }
}

/// Whether `port` is a decimal port number 1..=65535.
fn is_valid_port(port: &str) -> bool {
    !port.is_empty() && port.parse::<u16>().is_ok_and(|p| p > 0)
}

// Example configurations (TOML at `--config`):
//
// LVM provider (native-local, P1):
//
//     listen = "127.0.0.1:8787"
//     journal_dir = "/var/lib/volvisor/journal"
//     provider = "lvm"
//     lvm_vg_prefix = "volvisor"
//     device_claim_token = "scoped-destructive-auth"
//
//     # optional overrides:
//     # lvm_state_path = "/var/lib/volvisor/journal/lvm-state.json"
//     # sysfs_root = "/"
//
// Ceph provider (external-cluster RBD adapter, P2). The cluster is
// operated outside volvisor; startup is refused unless the cluster's
// reported fsid matches `ceph_cluster_fsid` exactly, the pool exists
// and a health query succeeds (fail-closed — a mis-pointed cluster is
// never adopted). The ceph CLI resolves the keyring for `ceph_user`
// itself; volvisor never reads or logs key material:
//
//     listen = "127.0.0.1:8787"
//     journal_dir = "/var/lib/volvisor/journal"
//     provider = "ceph"
//     ceph_cluster_fsid = "11111111-2222-3333-4444-555555555555"
//     ceph_mon_hosts = ["mon1.example:6789", "mon2.example:6789"]
//     ceph_pool = "volvisor"
//
//     # optional overrides:
//     # ceph_user = "client.volvisor"          (the default)
//     # ceph_state_path = "/var/lib/volvisor/journal/ceph-state.json"

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_toml() -> String {
        "\
listen = \"127.0.0.1:8787\"
journal_dir = \"/var/lib/volvisor/journal\"
provider = \"lvm\"
lvm_vg_prefix = \"volvisor\"
device_claim_token = \"scoped-destructive-auth\"
"
        .to_owned()
    }

    #[test]
    fn parses_minimal_config() {
        let cfg: Config = toml::from_str(&minimal_toml()).expect("parse");
        assert!(cfg.validate().is_ok());
        assert_eq!(cfg.provider, ProviderKind::Lvm);
        assert_eq!(cfg.max_body_bytes, 1 << 20);
    }

    #[test]
    fn lvm_provider_requires_prefix_and_token() {
        let raw = "\
listen = \"127.0.0.1:8787\"
journal_dir = \"/j\"
provider = \"lvm\"
";
        let cfg: Config = toml::from_str(raw).expect("parse");
        assert!(cfg.validate().is_err());
        let raw = raw.to_owned() + "device_claim_token = \"t\"\n";
        let cfg: Config = toml::from_str(&raw).expect("parse");
        assert!(cfg.validate().is_err(), "still missing lvm_vg_prefix");
    }

    #[test]
    fn rejects_unknown_fields() {
        let raw = minimal_toml() + "surprise = 1\n";
        assert!(toml::from_str::<Config>(&raw).is_err());
    }

    #[test]
    fn rejects_bad_prefix() {
        let raw = minimal_toml().replace("volvisor", "bad prefix!");
        let cfg: Config = toml::from_str(&raw).expect("parse");
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_explicitly_empty_admin_token() {
        let raw = minimal_toml() + "admin_token = \"\"\n";
        let cfg: Config = toml::from_str(&raw).expect("parse");
        assert!(
            cfg.validate().is_err(),
            "empty admin_token is a config mistake"
        );
    }

    #[test]
    fn accepts_unset_or_real_admin_token() {
        let cfg: Config = toml::from_str(&minimal_toml()).expect("parse");
        assert!(
            cfg.validate().is_ok(),
            "unset admin_token is the loopback-only mode"
        );
        let raw = minimal_toml() + "admin_token = \"real-token\"\n";
        let cfg: Config = toml::from_str(&raw).expect("parse");
        assert!(cfg.validate().is_ok());
    }

    fn minimal_ceph_toml() -> String {
        "\
listen = \"127.0.0.1:8787\"
journal_dir = \"/var/lib/volvisor/journal\"
provider = \"ceph\"
ceph_cluster_fsid = \"11111111-2222-3333-4444-555555555555\"
ceph_mon_hosts = [\"mon1.example:6789\", \"mon2.example:6789\"]
ceph_pool = \"volvisor\"
"
        .to_owned()
    }

    #[test]
    fn parses_minimal_ceph_config() {
        let cfg: Config = toml::from_str(&minimal_ceph_toml()).expect("parse");
        assert!(cfg.validate().is_ok());
        assert_eq!(cfg.provider, ProviderKind::Ceph);
        // The documented default user applies when ceph_user is unset.
        assert_eq!(cfg.ceph_user_or_default(), "client.volvisor");
    }

    #[test]
    fn ceph_provider_requires_fsid_mons_and_pool() {
        let raw = "\
listen = \"127.0.0.1:8787\"
journal_dir = \"/j\"
provider = \"ceph\"
";
        let cfg: Config = toml::from_str(raw).expect("parse");
        assert!(cfg.validate().is_err());
        let raw = raw.to_owned() + "ceph_cluster_fsid = \"11111111-2222-3333-4444-555555555555\"\n";
        let cfg: Config = toml::from_str(&raw).expect("parse");
        assert!(cfg.validate().is_err(), "still missing ceph_mon_hosts");
        let raw = raw.clone() + "ceph_mon_hosts = [\"mon1.example:6789\"]\n";
        let cfg: Config = toml::from_str(&raw).expect("parse");
        assert!(cfg.validate().is_err(), "still missing ceph_pool");
        let raw = raw.clone() + "ceph_pool = \"volvisor\"\n";
        let cfg: Config = toml::from_str(&raw).expect("parse");
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn ceph_fsid_must_be_a_canonical_uuid() {
        for bad in [
            "",
            "not-a-uuid",
            "11111111222233334444555555555555",
            "11111111-2222-3333-4444-5555555555555",
            "11111111-2222-3333-4444-55555555555g",
            "11111111_2222_3333_4444_555555555555",
        ] {
            let raw = minimal_ceph_toml().replace("11111111-2222-3333-4444-555555555555", bad);
            let cfg: Config = toml::from_str(&raw).expect("parse");
            assert!(cfg.validate().is_err(), "fsid {bad:?} must be rejected");
        }
    }

    #[test]
    fn ceph_mon_hosts_count_is_bounded() {
        // An empty list is not a monitor set.
        let raw = minimal_ceph_toml().replace(
            "ceph_mon_hosts = [\"mon1.example:6789\", \"mon2.example:6789\"]",
            "ceph_mon_hosts = []",
        );
        let cfg: Config = toml::from_str(&raw).expect("parse");
        assert!(cfg.validate().is_err(), "zero monitors must be rejected");
        // Ten monitors is one too many.
        let ten = ["\"mon.example:6789\""; 10].join(", ");
        let raw = minimal_ceph_toml().replace(
            "ceph_mon_hosts = [\"mon1.example:6789\", \"mon2.example:6789\"]",
            &format!("ceph_mon_hosts = [{ten}]"),
        );
        let cfg: Config = toml::from_str(&raw).expect("parse");
        assert!(cfg.validate().is_err(), "ten monitors must be rejected");
        // Nine is the documented maximum.
        let nine = ["\"mon.example:6789\""; 9].join(", ");
        let raw = minimal_ceph_toml().replace(
            "ceph_mon_hosts = [\"mon1.example:6789\", \"mon2.example:6789\"]",
            &format!("ceph_mon_hosts = [{nine}]"),
        );
        let cfg: Config = toml::from_str(&raw).expect("parse");
        assert!(cfg.validate().is_ok(), "nine monitors must be accepted");
    }

    #[test]
    fn ceph_mon_host_entries_must_be_well_formed() {
        for bad in [
            "",
            "   ",
            "mon1.example:6789 extra",
            "mon1.example:",
            "mon1.example:notaport",
            "mon1.example:0",
            "mon1.example:99999",
            ":6789",
            "[::1",
            "[]:6789",
            "[::1]:6789:1",
            // Unbracketed IPv6 is ambiguous against host:port.
            "::1",
        ] {
            let raw = minimal_ceph_toml().replace(
                "ceph_mon_hosts = [\"mon1.example:6789\", \"mon2.example:6789\"]",
                &format!("ceph_mon_hosts = [\"{bad}\"]"),
            );
            let cfg: Config = toml::from_str(&raw).expect("parse");
            assert!(
                cfg.validate().is_err(),
                "mon entry {bad:?} must be rejected"
            );
        }
        for good in [
            "mon1.example",
            "mon1.example:6789",
            "10.0.0.1:6789",
            "[::1]",
            "[::1]:6789",
        ] {
            let raw = minimal_ceph_toml().replace(
                "ceph_mon_hosts = [\"mon1.example:6789\", \"mon2.example:6789\"]",
                &format!("ceph_mon_hosts = [\"{good}\"]"),
            );
            let cfg: Config = toml::from_str(&raw).expect("parse");
            assert!(
                cfg.validate().is_ok(),
                "mon entry {good:?} must be accepted"
            );
        }
    }

    #[test]
    fn ceph_pool_uses_the_lvm_name_rule() {
        for bad in ["", "bad pool!", "p\u{f6}\u{f6}l", &"x".repeat(65)] {
            let raw = minimal_ceph_toml().replace(
                "ceph_pool = \"volvisor\"",
                &format!("ceph_pool = \"{bad}\""),
            );
            let cfg: Config = toml::from_str(&raw).expect("parse");
            assert!(cfg.validate().is_err(), "pool {bad:?} must be rejected");
        }
        let raw =
            minimal_ceph_toml().replace("ceph_pool = \"volvisor\"", "ceph_pool = \"pool-1_2\"");
        let cfg: Config = toml::from_str(&raw).expect("parse");
        assert!(cfg.validate().is_ok());
        let raw = minimal_ceph_toml().replace(
            "ceph_pool = \"volvisor\"",
            &format!("ceph_pool = \"{}\"", "x".repeat(64)),
        );
        let cfg: Config = toml::from_str(&raw).expect("parse");
        assert!(cfg.validate().is_ok(), "64 characters is the maximum");
    }

    #[test]
    fn ceph_user_must_be_set_to_something_sane() {
        // `--name` takes the full entity name: a bare id, a wrong entity
        // type, or an empty/garbage id is a config mistake.
        for bad in [
            "",
            "volvisor",
            "client.",
            "client with spaces",
            "client\tvolvisor",
            "mon.volvisor",
            "client.volvisor/extra",
        ] {
            let raw = minimal_ceph_toml() + &format!("ceph_user = \"{bad}\"\n");
            let cfg: Config = toml::from_str(&raw).expect("parse");
            assert!(cfg.validate().is_err(), "user {bad:?} must be rejected");
        }
        for good in ["client.admin", "client.volvisor-2", "client.a.b_c"] {
            let raw = minimal_ceph_toml() + &format!("ceph_user = \"{good}\"\n");
            let cfg: Config = toml::from_str(&raw).expect("parse");
            assert!(cfg.validate().is_ok(), "user {good:?} must be accepted");
        }
        let raw = minimal_ceph_toml() + "ceph_user = \"client.admin\"\n";
        let cfg: Config = toml::from_str(&raw).expect("parse");
        assert!(cfg.validate().is_ok());
        assert_eq!(cfg.ceph_user_or_default(), "client.admin");
    }

    #[test]
    fn lvm_fields_are_simply_ignored_for_ceph() {
        // Mirroring how the lvm provider ignores fake-provider fields:
        // extraneous (but well-formed) lvm_* settings are not an error.
        let raw = minimal_ceph_toml() + "lvm_state_path = \"/j/lvm-state.json\"\n";
        let cfg: Config = toml::from_str(&raw).expect("parse");
        assert!(cfg.validate().is_ok());
    }
}
