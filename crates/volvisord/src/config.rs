//! Daemon configuration.
//!
//! Configuration is explicit and validated at startup; defaults are
//! conservative. No secret material is ever logged (SPEC-0002 section 9).

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::path::PathBuf;

use serde::Deserialize;
use volvisor_api::auth::{Credential, Role};

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
    /// DRBD 9 nearline baseline (P3 prototype): the local end of a
    /// single-primary replicated resource over an operator-designated
    /// volume group, verified fail-closed at startup (ADR-0007).
    Drbd,
}

impl ProviderKind {
    /// The TOML name of the provider (the `provider` field's value,
    /// and what `--check-config`'s summary prints).
    #[must_use]
    pub const fn as_toml_name(self) -> &'static str {
        match self {
            ProviderKind::Fake => "fake",
            ProviderKind::Lvm => "lvm",
            ProviderKind::Ceph => "ceph",
            ProviderKind::Drbd => "drbd",
        }
    }
}

/// Default Ceph entity name (`--name`) when `ceph_user` is unset.
///
/// The ceph CLI resolves the matching keyring itself (CEPH_CONF /
/// keyring conventions); volvisor only passes `--name` and `-m` and never
/// reads or logs credential material.
pub const DEFAULT_CEPH_USER: &str = "client.volvisor";

/// HTTP admission-control knobs (issue #45).
///
/// The long-operation model is documented in `docs/admission-control.md`:
/// `read_timeout_secs` bounds read-only routes only, never a journaled
/// mutation.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmissionConfig {
    /// Bound on a read-only provider call (list/inspect/capabilities and
    /// mobility observation).
    #[serde(default = "default_read_timeout_secs")]
    pub read_timeout_secs: u64,
    /// Bound on waiting for a global concurrency permit before answering
    /// `503 OVERLOADED`.
    #[serde(default = "default_admission_wait_timeout_secs")]
    pub queue_timeout_secs: u64,
    /// Global cap on in-flight requests (liveness and metrics exempt).
    #[serde(default = "default_max_concurrent_requests")]
    pub max_concurrent_requests: usize,
    /// Per-principal token refill rate. Rate limiting is **opt-in**: `0`
    /// (the default) disables it and preserves legacy client behavior.
    #[serde(default = "default_rate_limit_per_second")]
    pub rate_limit_per_second: u32,
    /// Per-principal token-bucket burst; must be at least one when the rate
    /// is non-zero. Defaults to `0` because the limiter is disabled by
    /// default.
    #[serde(default = "default_rate_limit_burst")]
    pub rate_limit_burst: u32,
    /// Maximum accepted `GET /v2/volumes?limit=`.
    #[serde(default = "default_max_list_limit")]
    pub max_list_limit: usize,
}

impl Default for AdmissionConfig {
    fn default() -> Self {
        Self {
            read_timeout_secs: default_read_timeout_secs(),
            queue_timeout_secs: default_admission_wait_timeout_secs(),
            max_concurrent_requests: default_max_concurrent_requests(),
            rate_limit_per_second: default_rate_limit_per_second(),
            rate_limit_burst: default_rate_limit_burst(),
            max_list_limit: default_max_list_limit(),
        }
    }
}

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
    /// Operator-designated volume group holding the nearline backing
    /// LVs (drbd provider only; must exist and be distinct from any
    /// `native-local` VG — foreign LVs in it are never touched).
    pub drbd_vg_name: Option<String>,
    /// Directory the generated `volvisor-<resource>.res` files live in
    /// (drbd provider only; defaults to `/etc/drbd.d`).
    pub drbd_config_dir: Option<std::path::PathBuf>,
    /// The local DRBD `on` node name (drbd provider only; validated
    /// against `uname -n` at startup — a wrong-host daemon must never
    /// adopt a peer's resources).
    pub drbd_node_name: Option<String>,
    /// The local replication address, an IPv4 dotted quad without port
    /// (drbd provider only).
    pub drbd_local_address: Option<String>,
    /// The peer's `on` node name (drbd provider only).
    pub drbd_peer_name: Option<String>,
    /// The peer's replication address as `<ipv4>:<port>` (drbd provider
    /// only; the peer end is operator-provisioned out of band in P3).
    pub drbd_peer_address: Option<String>,
    /// Path of the peer shared secret, an owner-only file (drbd
    /// provider only; required — peer authentication is a binding v1
    /// invariant). The secret is read at resource-generation time and
    /// never configured inline or logged.
    pub drbd_shared_secret_file: Option<std::path::PathBuf>,
    /// Inclusive lower bound of the local replication port range (drbd
    /// provider only; defaults to 7100).
    #[serde(default = "default_drbd_port_min")]
    pub drbd_port_min: u16,
    /// Inclusive upper bound of the local replication port range (drbd
    /// provider only; defaults to 7199).
    #[serde(default = "default_drbd_port_max")]
    pub drbd_port_max: u16,
    /// Inclusive lower bound of the DRBD minor range (drbd provider
    /// only; defaults to 100).
    #[serde(default = "default_drbd_minor_min")]
    pub drbd_minor_min: u32,
    /// Inclusive upper bound of the DRBD minor range (drbd provider
    /// only; defaults to 999).
    #[serde(default = "default_drbd_minor_max")]
    pub drbd_minor_max: u32,
    /// Root of the procfs mount used for the DRBD module check (drbd
    /// provider only; defaults to `/proc` — test isolation only).
    pub drbd_proc_root: Option<std::path::PathBuf>,
    /// Durable drbd provider state path (defaults to
    /// `<journal_dir>/drbd-state.json`).
    pub drbd_state_path: Option<std::path::PathBuf>,
    /// Filesystem root for read-only device discovery (defaults to `/`;
    /// test isolation only).
    pub sysfs_root: Option<std::path::PathBuf>,
    /// Static bearer token guarding the API's privileged surface (every
    /// mutating endpoint and the whole `/v2/admin` route group, `GET`
    /// included). When unset, those endpoints reject every request with
    /// `401` (fail closed) and the daemon refuses to bind a non-loopback
    /// address: the tokenless mode is loopback-only dev/test. Never logged.
    ///
    /// This legacy single token remains a full admin credential. Prefer
    /// `[credentials]` for named principals, role separation and rotation.
    #[serde(default)]
    pub admin_token: Option<String>,
    /// Scoped named credentials (admin / operator / read-only). When this
    /// table is non-empty, read routes also require authentication.
    #[serde(default)]
    pub credentials: CredentialsConfig,
    /// Native TLS configuration for the listener and outbound clients.
    #[serde(default)]
    pub tls: TlsConfig,
    /// Explicit opt-out of the plain-HTTP non-loopback refusal. A listener
    /// on a non-loopback address without TLS is refused unless this is
    /// `true`; setting it emits a startup warning and is a deliberate
    /// "trusted network / external terminator" decision, never a silent
    /// downgrade.
    #[serde(default)]
    pub allow_insecure: bool,
    /// Maximum request body size in bytes.
    #[serde(default = "default_max_body_bytes")]
    pub max_body_bytes: usize,
    /// HTTP admission-control knobs (timeouts, concurrency cap, rate limit,
    /// pagination maximum). See `docs/admission-control.md`.
    #[serde(default)]
    pub admission: AdmissionConfig,
    /// Base URL of the writer-authority witness (drbd provider only,
    /// e.g. `https://10.0.0.3:9101`). When set, the provider is
    /// witness-managed: every promotion acquires a lease first, a
    /// background task renews leases and enforces the local W5
    /// deadline, and startup/reconcile fail closed on unproven
    /// primaries. Absent — or a provider other than drbd — keeps the
    /// exact pre-authority behavior. The witness must be a **third
    /// failure domain**: the guard in `Config::validate` refuses an
    /// endpoint colocated with either replication end.
    pub witness_url: Option<String>,
    /// Bearer token for the witness surface; required when
    /// `witness_url` names a non-loopback host (the witness's own
    /// fail-closed convention). On a v2 witness this shared token is
    /// **read-only** (inspect/health). Never logged.
    pub witness_token: Option<String>,
    /// This host's W8 credential: the per-host token the witness maps
    /// to `drbd_node_name`'s identity. Required whenever
    /// `witness_url` is set — the witness protocol is v2 and every
    /// state-mutating call from this daemon (grant/renew/self-revoke/
    /// register, not only the migration surface) authenticates as this
    /// host; without the credential attach/detach/renewal would fail
    /// at runtime, so validation refuses the configuration up front
    /// (P4b plan §6). Never logged.
    pub witness_host_token: Option<String>,
    /// Writer lease renewal interval in seconds (required with
    /// `witness_url`; must be positive). The interval must stay under
    /// half the witness's lease TTL — a bound the daemon can only
    /// check lazily against every grant/renew response, because the
    /// TTL lives on the witness (P4a plan §6); a violating response is
    /// refused there.
    pub witness_renewal_interval_secs: Option<u64>,
    /// The migration table (P4b plan §6, stage B2): the coordinated
    /// VMM/storage handoff. Inert until `enabled`; validation of the
    /// contained fields runs only when it is.
    #[serde(default)]
    pub migration: MigrationConfig,
    /// The VMM table (P4b plan §6, stage B2): the ch-remote adapter's
    /// knobs. Inert unless `[migration] enabled` (a configured VMM
    /// alone enables nothing).
    #[serde(default)]
    pub vmm: VmmConfig,
}

/// The `[migration]` table: the coordinated VMM/storage handoff (P4b
/// plan §6). Cross-host cutover is opt-in — every field is inert until
/// `enabled`, and enabling requires the drbd provider, a witness and a
/// configured VMM (validated in the daemon's config validation).
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MigrationConfig {
    /// Whether the migration surface (mobility routes, peer routes,
    /// the coordinator's retry task) is served. `false` until the
    /// deployment opts in.
    #[serde(default)]
    pub enabled: bool,
    /// The shared snapshot directory (must exist locally; its
    /// cross-host readability is verified at `PREPARED` by the
    /// destination daemon, with a typed refusal).
    pub snapshot_dir: Option<std::path::PathBuf>,
    /// The peer daemon's base URL (the internal peer API — by design
    /// colocated with the peer replication end, since the destination
    /// daemon is the destination VMM's proxy; only the witness is a
    /// third failure domain in this topology).
    pub peer_api_url: Option<String>,
    /// The daemon-to-daemon credential for the peer API (distinct
    /// from the witness and consumer tokens; never logged).
    pub peer_api_token: Option<String>,
}

/// The `[vmm]` table: the ch-remote adapter's knobs (P4b plan
/// §5/§6) and the grow-notification gate's knobs (P6-B).
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VmmConfig {
    /// Path of the `ch-remote` binary (required when migration is
    /// enabled).
    pub ch_remote_bin: Option<std::path::PathBuf>,
    /// Directory holding the per-VM API sockets
    /// (`{api_socket_dir}/{vm_id}.sock`); required when migration is
    /// enabled.
    pub api_socket_dir: Option<std::path::PathBuf>,
    /// Path of the `cloud-hypervisor` binary (P6-B): probed once at
    /// startup (`<binary> --version`, shell-free, through the real
    /// runner) for the grow-notification version gate. Not probed
    /// by `--check-config` (hermeticity: the check must not depend
    /// on the host carrying a VMM).
    pub cloud_hypervisor_bin: Option<std::path::PathBuf>,
    /// The minimum cloud-hypervisor version the grow notification
    /// accepts (P6-B): semver (`37.0`, `37.0.1`, `v37.0.0` — the
    /// tolerant parse; see the provider's `vmm_version`). The
    /// repository pins no default — the operator configures the
    /// release they have actually qualified. Absent fields leave
    /// the gate refused (fail-closed): attached grows report
    /// `retry_required` with the recorded reason.
    pub minimum_version: Option<String>,
}

/// The `[tls]` table: native TLS for the Volume API listener and the
/// outbound witness/peer clients.
///
/// `enabled` controls the *listener*. The `[tls.client]` sub-table is
/// always meaningful for outbound `https://` URLs: it supplies a private-CA
/// trust bundle and/or an mTLS client identity. Unset client settings use
/// the Mozilla root program.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    /// Serve the Volume API over TLS. Requires `cert_file` and `key_file`.
    #[serde(default)]
    pub enabled: bool,
    /// PEM server certificate chain.
    pub cert_file: Option<PathBuf>,
    /// PEM private key matching the served certificate.
    pub key_file: Option<PathBuf>,
    /// Optional PEM client CA. When set, clients must present a certificate
    /// issued by one of these roots (mTLS).
    pub client_ca_file: Option<PathBuf>,
    /// Outbound trust and identity for `https://` witness/peer endpoints.
    #[serde(default)]
    pub client: TlsClientConfig,
}

/// Outbound TLS settings for https witness and peer endpoints.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsClientConfig {
    /// PEM trust bundle for a private CA. Unset uses the Mozilla roots.
    pub ca_file: Option<PathBuf>,
    /// Optional PEM client certificate chain (mTLS).
    pub cert_file: Option<PathBuf>,
    /// Optional PEM private key matching `cert_file`.
    pub key_file: Option<PathBuf>,
}

/// Scoped API credentials: named principals with rotation-friendly token
/// lists per role.
///
/// The legacy top-level `admin_token` remains supported as one admin
/// principal. When any `[credentials]` entry is configured, read routes
/// (volume metadata, capabilities and `/metrics`) require at least a
/// read-only credential; without this table they keep the historical
/// loopback-dev posture (reads open, mutations token-guarded).
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialsConfig {
    /// Named admin principals (full access).
    #[serde(default)]
    pub admin: Vec<NamedCredential>,
    /// Named operator principals (volume/migration mutations, no device
    /// enrollment or other `/v2/admin` routes).
    #[serde(default)]
    pub operator: Vec<NamedCredential>,
    /// Named read-only principals.
    #[serde(default)]
    pub read_only: Vec<NamedCredential>,
}

/// One named credential in [`CredentialsConfig`].
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NamedCredential {
    /// Stable principal id used in audit events (never the token).
    pub id: String,
    /// The bearer token presented by this principal. Never logged.
    pub token: String,
}

impl CredentialsConfig {
    /// Whether any scoped credential is configured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.admin.is_empty() && self.operator.is_empty() && self.read_only.is_empty()
    }

    /// Convert the TOML entries to API-layer credentials.
    #[must_use]
    pub fn to_credentials(&self) -> Vec<Credential> {
        let mut credentials =
            Vec::with_capacity(self.admin.len() + self.operator.len() + self.read_only.len());
        for (role, entries) in [
            (Role::Admin, &self.admin),
            (Role::Operator, &self.operator),
            (Role::ReadOnly, &self.read_only),
        ] {
            for entry in entries {
                credentials.push(Credential {
                    id: entry.id.clone(),
                    token: entry.token.clone(),
                    role,
                });
            }
        }
        credentials
    }
}

fn default_max_body_bytes() -> usize {
    1 << 20
}
fn default_read_timeout_secs() -> u64 {
    30
}
fn default_admission_wait_timeout_secs() -> u64 {
    5
}
fn default_max_concurrent_requests() -> usize {
    256
}
fn default_rate_limit_per_second() -> u32 {
    0
}
fn default_rate_limit_burst() -> u32 {
    0
}
fn default_max_list_limit() -> usize {
    1000
}
fn default_drbd_port_min() -> u16 {
    7100
}

fn default_drbd_port_max() -> u16 {
    7199
}

fn default_drbd_minor_min() -> u32 {
    100
}

fn default_drbd_minor_max() -> u32 {
    999
}

/// Resolve a witness URL's host to the addresses it names (P4a plan
/// §3): a literal IP maps to itself; a **name** resolves through the
/// system resolver with every returned address counting. Both `http://`
/// and `https://` are accepted; the scheme decides the default port used
/// for name resolution.
///
/// # Errors
/// [`DaemonError::Config`] for a non-HTTP(S) scheme, an unparseable or
/// empty host, or a name that cannot be resolved — the guard treats
/// ambiguity as colocation and refuses, and a witness the daemon
/// cannot resolve is unusable anyway.
/// Extract the host from a URL authority, handling a bracketed IPv6 literal
/// with or without a port (`[::1]`, `[::1]:9101`) and the usual
/// `host:port` form.
fn authority_host(authority: &str) -> &str {
    if let Some(rest) = authority.strip_prefix('[') {
        if let Some((host, _)) = rest.split_once(']') {
            return host;
        }
    }
    authority
        .rsplit_once(':')
        .map_or(authority, |(host, _)| host)
}

fn witness_host(url: &str) -> Result<Vec<std::net::IpAddr>, DaemonError> {
    let (scheme, rest) = url.split_once("://").ok_or_else(|| {
        DaemonError::Config(format!(
            "witness_url {url} must use the http:// or https:// scheme"
        ))
    })?;
    if scheme != "http" && scheme != "https" {
        return Err(DaemonError::Config(format!(
            "witness_url {url} must use the http:// or https:// scheme"
        )));
    }
    let authority = rest.split('/').next().unwrap_or(rest);
    let host = authority_host(authority);
    if host.is_empty() {
        return Err(DaemonError::Config(format!(
            "witness_url {url} carries no host"
        )));
    }
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return Ok(vec![ip]);
    }
    let default_port = if scheme == "https" { 443 } else { 80 };
    let resolved: Vec<std::net::IpAddr> =
        std::net::ToSocketAddrs::to_socket_addrs(&format!("{host}:{default_port}"))
            .map_err(|err| {
                DaemonError::Config(format!(
                    "witness_url {url} host {host} cannot be resolved (the failure-domain \
                     guard treats ambiguity as colocation): {err}"
                ))
            })?
            .map(|socket| socket.ip())
            .collect();
    if resolved.is_empty() {
        return Err(DaemonError::Config(format!(
            "witness_url {url} host {host} resolves to no address"
        )));
    }
    Ok(resolved)
}

/// Parse an outbound URL into its scheme and host authority.
fn outbound_url_parts<'a>(url: &'a str, field: &str) -> Result<(&'a str, &'a str), DaemonError> {
    let (scheme, rest) = url.split_once("://").ok_or_else(|| {
        DaemonError::Config(format!("{field} must use the http:// or https:// scheme"))
    })?;
    if scheme != "http" && scheme != "https" {
        return Err(DaemonError::Config(format!(
            "{field} must use the http:// or https:// scheme"
        )));
    }
    let authority = rest.split('/').next().unwrap_or(rest);
    let host = authority_host(authority);
    if host.is_empty() {
        return Err(DaemonError::Config(format!("{field} carries no host")));
    }
    Ok((scheme, host))
}

/// Whether a host literal is loopback. Names other than `localhost` are
/// deliberately treated as non-loopback: a plain-HTTP URL that only *might*
/// resolve locally is refused unless the operator explicitly opted in.
fn host_is_loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// The host portion of a configured `<host>:<port>` peer address.
fn host_of(address: &str) -> &str {
    address.rsplit_once(':').map_or(address, |(host, _)| host)
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

    /// Whether any credential (legacy admin token or scoped named
    /// credentials) is configured.
    #[must_use]
    pub fn has_credentials(&self) -> bool {
        self.admin_token.is_some()
            || !self.credentials.admin.is_empty()
            || !self.credentials.operator.is_empty()
            || !self.credentials.read_only.is_empty()
    }

    /// Build the API layer's admission configuration from the daemon knobs.
    #[must_use]
    pub fn api_config(&self) -> volvisor_api::ApiConfig {
        volvisor_api::ApiConfig {
            max_body_bytes: self.max_body_bytes,
            read_timeout: std::time::Duration::from_secs(self.admission.read_timeout_secs),
            admission_wait_timeout: std::time::Duration::from_secs(
                self.admission.queue_timeout_secs,
            ),
            max_concurrent_requests: self.admission.max_concurrent_requests,
            rate_limit_per_second: self.admission.rate_limit_per_second,
            rate_limit_burst: self.admission.rate_limit_burst,
            max_list_limit: self.admission.max_list_limit,
        }
    }

    /// Render the concise `--check-config` summary (P7-A, ADR-0009):
    /// the facts an operator verifies at a glance — bind address,
    /// provider, state location, witness and migration posture. Never
    /// includes token values (SPEC-0002 section 9); the witness URL
    /// and the paths are configuration facts, not secrets.
    #[must_use]
    pub fn check_summary(&self) -> String {
        let witness = match (&self.witness_url, self.witness_renewal_interval_secs) {
            (Some(url), Some(secs)) => format!("witness {url} (renews every {secs}s)"),
            _ => "no witness".to_owned(),
        };
        let migration = if self.migration.enabled {
            "migration enabled"
        } else {
            "migration disabled"
        };
        let transport = if self.tls.enabled {
            "tls".to_owned()
        } else if self.allow_insecure {
            "plain-http (allow_insecure)".to_owned()
        } else {
            "plain-http loopback".to_owned()
        };
        let read_auth = if self.credentials.is_empty() {
            "open (legacy)".to_owned()
        } else {
            format!(
                "scoped ({} admin, {} operator, {} read-only)",
                self.credentials.admin.len(),
                self.credentials.operator.len(),
                self.credentials.read_only.len()
            )
        };
        format!(
            "config ok: listen {}, provider {}, journal_dir {}, transport {transport}, \
             read-auth {read_auth}, {witness}, {migration}",
            self.listen,
            self.provider.as_toml_name(),
            self.journal_dir.display()
        )
    }

    /// Validate cross-field constraints.
    fn validate(&self) -> Result<(), DaemonError> {
        if self.provider == ProviderKind::Lvm {
            self.validate_lvm_fields()?;
        }
        if self.provider == ProviderKind::Ceph {
            self.validate_ceph_fields()?;
        }
        if self.provider == ProviderKind::Drbd {
            self.validate_drbd_fields()?;
        }
        self.validate_witness_fields()?;
        self.validate_migration_fields()?;
        self.validate_transport_fields()?;
        self.validate_credentials()?;
        self.validate_field_shapes()
    }

    /// Migration fields (P4b plan §6, stage B2): the coordinated
    /// handoff is opt-in. Enabling requires the drbd provider (the
    /// only nearline class), a witness (the authority substrate the
    /// cut depends on), a locally-existing snapshot directory, the
    /// peer daemon's URL and credential, and a configured VMM. An
    /// inert `[migration]`/`[vmm]` (enabled = false) validates
    /// nothing further — the fields are dead config until the
    /// deployment opts in.
    fn validate_migration_fields(&self) -> Result<(), DaemonError> {
        if !self.migration.enabled {
            return Ok(());
        }
        if self.provider != ProviderKind::Drbd {
            return Err(DaemonError::Config(
                "migration.enabled applies to the drbd provider only (the handoff is the \
                 nearline replication cutover)"
                    .to_owned(),
            ));
        }
        if self.witness_url.is_none() {
            return Err(DaemonError::Config(
                "migration.enabled requires a witness (the cut's authority substrate: \
                 barriers, RevokeSet/GrantSet)"
                    .to_owned(),
            ));
        }
        let snapshot_dir = self.migration.snapshot_dir.clone().ok_or_else(|| {
            DaemonError::Config(
                "migration.snapshot_dir is required when migration is enabled".to_owned(),
            )
        })?;
        if !snapshot_dir.is_dir() {
            return Err(DaemonError::Config(format!(
                "migration.snapshot_dir {} does not exist or is not a directory (its \
                 cross-host readability is verified at PREPARED by the destination daemon)",
                snapshot_dir.display()
            )));
        }
        let peer_url = self.migration.peer_api_url.clone().ok_or_else(|| {
            DaemonError::Config(
                "migration.peer_api_url is required when migration is enabled (the \
                 destination daemon is the destination VMM's proxy)"
                    .to_owned(),
            )
        })?;
        let (scheme, host) = outbound_url_parts(&peer_url, "migration.peer_api_url")?;
        if scheme == "http" && !self.allow_insecure && !host_is_loopback(host) {
            return Err(DaemonError::Config(format!(
                "migration.peer_api_url {peer_url} is plain HTTP to a non-loopback host; \
                 use https:// or set allow_insecure = true (a deliberate downgrade)"
            )));
        }
        if self
            .migration
            .peer_api_token
            .as_deref()
            .is_none_or(|token| token.trim().is_empty())
        {
            return Err(DaemonError::Config(
                "migration.peer_api_token is required when migration is enabled (the \
                 daemon-to-daemon credential, distinct from the witness and consumer tokens)"
                    .to_owned(),
            ));
        }
        let vmm_bin = self.vmm.ch_remote_bin.clone().ok_or_else(|| {
            DaemonError::Config(
                "vmm.ch_remote_bin is required when migration is enabled".to_owned(),
            )
        })?;
        if vmm_bin.as_os_str().is_empty() {
            return Err(DaemonError::Config(
                "vmm.ch_remote_bin must not be empty".to_owned(),
            ));
        }
        if self
            .vmm
            .api_socket_dir
            .as_ref()
            .is_none_or(|dir| dir.as_os_str().is_empty())
        {
            return Err(DaemonError::Config(
                "vmm.api_socket_dir is required when migration is enabled (the per-VM \
                 socket convention: {api_socket_dir}/{vm_id}.sock)"
                    .to_owned(),
            ));
        }
        Ok(())
    }

    /// Witness fields (P4a plan §3/§6): witness management applies only
    /// to the drbd provider, requires a renewal interval, follows the
    /// witness's fail-closed token convention for non-loopback
    /// endpoints, and is refused outright when the endpoint shares a
    /// failure domain with either replication end (a colocated witness
    /// is indistinguishable, from the fence's perspective, from no
    /// witness at all).
    fn validate_witness_fields(&self) -> Result<(), DaemonError> {
        let Some(url) = &self.witness_url else {
            // Absent witness: pre-authority mode. None of the other
            // witness fields may be set — they would silently imply a
            // configuration the daemon does not follow.
            if self.witness_renewal_interval_secs.is_some() {
                return Err(DaemonError::Config(
                    "witness_renewal_interval_secs is set without witness_url".to_owned(),
                ));
            }
            if self.witness_token.is_some() {
                return Err(DaemonError::Config(
                    "witness_token is set without witness_url".to_owned(),
                ));
            }
            if self.witness_host_token.is_some() {
                return Err(DaemonError::Config(
                    "witness_host_token is set without witness_url".to_owned(),
                ));
            }
            return Ok(());
        };
        if self.provider != ProviderKind::Drbd {
            return Err(DaemonError::Config(
                "witness_url applies to the drbd provider only (the other classes have no \
                 remote writer authority)"
                    .to_owned(),
            ));
        }
        let interval = self.witness_renewal_interval_secs.ok_or_else(|| {
            DaemonError::Config(
                "witness_renewal_interval_secs is required when witness_url is set".to_owned(),
            )
        })?;
        if interval == 0 {
            return Err(DaemonError::Config(
                "witness_renewal_interval_secs must be > 0".to_owned(),
            ));
        }
        let witness_addresses = witness_host(url)?;
        if witness_addresses.iter().any(std::net::IpAddr::is_loopback) {
            return Err(DaemonError::Config(format!(
                "witness_url {url} is loopback: the witness would run on this storage host, \
                 not a third failure domain (P4a plan §3); colocation is refused"
            )));
        }
        let (scheme, _host) = outbound_url_parts(url, "witness_url")?;
        if scheme == "http" && !self.allow_insecure {
            return Err(DaemonError::Config(format!(
                "witness_url {url} is plain HTTP; the witness is a control-plane authority \
                 and must be encrypted. Use https:// or set allow_insecure = true (a \
                 deliberate downgrade that emits a startup warning)"
            )));
        }
        if self
            .witness_token
            .as_deref()
            .is_none_or(|token| token.trim().is_empty())
        {
            return Err(DaemonError::Config(format!(
                "witness_token is required for the non-loopback witness_url {url} (the \
                 witness's own fail-closed convention)"
            )));
        }
        self.ensure_witness_failure_domain(&witness_addresses, url)?;
        // W8 (P4b plan §6), checked last so the more fundamental URL,
        // colocation and shared-token rules report first: a v2 witness
        // refuses every mutation from the shared token; this daemon's
        // grant/renew/self-revoke/register calls authenticate as this
        // host. Without the credential the daemon would start and fail
        // at the first attach — refuse the configuration up front
        // instead.
        match self.witness_host_token.as_deref() {
            Some(token) if !token.trim().is_empty() => {}
            _ => {
                return Err(DaemonError::Config(
                    "witness_host_token is required when witness_url is set (the witness \
                     protocol is v2: this host's credential for grant/renew/revoke/register)"
                        .to_owned(),
                ));
            }
        }
        Ok(())
    }

    /// Transport posture: a non-loopback plain-HTTP listener is refused
    /// unless the operator explicitly set `allow_insecure = true`. TLS
    /// server fields are required together and must name readable files;
    /// outbound client identity is likewise all-or-nothing. The URL-level
    /// plain-HTTP refusals live in the witness/migration validators, where
    /// the endpoint is known.
    fn validate_transport_fields(&self) -> Result<(), DaemonError> {
        if self.tls.enabled {
            require_readable_file(self.tls.cert_file.as_ref(), "tls.cert_file")?;
            require_readable_file(self.tls.key_file.as_ref(), "tls.key_file")?;
            if let Some(ca) = &self.tls.client_ca_file {
                if ca.as_os_str().is_empty() {
                    return Err(DaemonError::Config(
                        "tls.client_ca_file must not be empty when set".to_owned(),
                    ));
                }
                require_readable_file(Some(ca), "tls.client_ca_file")?;
            }
        } else {
            if self.tls.cert_file.is_some()
                || self.tls.key_file.is_some()
                || self.tls.client_ca_file.is_some()
            {
                return Err(DaemonError::Config(
                    "tls.cert_file/key_file/client_ca_file are set while tls.enabled = false; \
                     serve TLS or remove the orphan fields"
                        .to_owned(),
                ));
            }
            if !self.listen.ip().is_loopback() && !self.allow_insecure {
                return Err(DaemonError::Config(format!(
                    "listen {} is a non-loopback plain-HTTP bind; refusing to start. \
                     Configure [tls] enabled = true (native TLS), or set \
                     allow_insecure = true to accept an explicit cleartext downgrade",
                    self.listen
                )));
            }
        }
        if self.tls.client.cert_file.is_some() != self.tls.client.key_file.is_some() {
            return Err(DaemonError::Config(
                "tls.client.cert_file and tls.client.key_file must be configured together \
                 (the outbound mTLS identity)"
                    .to_owned(),
            ));
        }
        if self.tls.client.ca_file.is_some() {
            require_readable_file(self.tls.client.ca_file.as_ref(), "tls.client.ca_file")?;
        }
        if self.tls.client.cert_file.is_some() {
            require_readable_file(self.tls.client.cert_file.as_ref(), "tls.client.cert_file")?;
            require_readable_file(self.tls.client.key_file.as_ref(), "tls.client.key_file")?;
        }
        Ok(())
    }

    /// Scoped credential validation: named principals must be unique, token
    /// values must be non-empty and unique across every role, and no scoped
    /// token may duplicate the legacy `admin_token` (ambiguous resolution).
    fn validate_credentials(&self) -> Result<(), DaemonError> {
        let mut ids = BTreeSet::new();
        let mut tokens = BTreeSet::new();
        for (role, entries) in [
            ("admin", &self.credentials.admin),
            ("operator", &self.credentials.operator),
            ("read_only", &self.credentials.read_only),
        ] {
            for entry in entries {
                if !is_credential_id(&entry.id) {
                    return Err(DaemonError::Config(format!(
                        "credentials.{role} id {:?} must be 1..=64 characters of alphanumeric, \
                         '-', '_' or '.' (a stable audit principal id)",
                        entry.id
                    )));
                }
                if entry.token.trim().is_empty() {
                    return Err(DaemonError::Config(format!(
                        "credentials.{role} id {:?} requires a non-empty token",
                        entry.id
                    )));
                }
                if !ids.insert(entry.id.as_str()) {
                    return Err(DaemonError::Config(format!(
                        "credentials.{role} id {:?} is duplicated; principal ids must be unique",
                        entry.id
                    )));
                }
                if !tokens.insert(entry.token.as_str()) {
                    return Err(DaemonError::Config(format!(
                        "credentials.{role} id {:?} reuses a token value; identity resolution \
                         must be unambiguous",
                        entry.id
                    )));
                }
                if self.admin_token.as_deref() == Some(entry.token.as_str()) {
                    return Err(DaemonError::Config(format!(
                        "credentials.{role} id {:?} reuses the legacy admin_token value; \
                         identity resolution must be unambiguous",
                        entry.id
                    )));
                }
            }
        }
        Ok(())
    }

    /// The two-sided failure-domain guard (P4a plan §3): the witness
    /// host must share no address with this host's replication address
    /// or the configured peer address. Name-vs-literal resolution is
    /// conservative: a witness named by DNS counts every resolved
    /// address, and an unresolvable name refuses startup (ambiguity is
    /// treated as colocation). Volvisor cannot verify physical
    /// placement — deployment remains an operator responsibility,
    /// documented in the plan's honesty section.
    fn ensure_witness_failure_domain(
        &self,
        witness_addresses: &[std::net::IpAddr],
        url: &str,
    ) -> Result<(), DaemonError> {
        let Some(local) = &self.drbd_local_address else {
            return Ok(());
        };
        let Some(peer) = &self.drbd_peer_address else {
            return Ok(());
        };
        for (side, configured) in [("local", local.as_str()), ("peer", host_of(peer))] {
            let Ok(side_ip) = configured.parse::<std::net::IpAddr>() else {
                // validate_drbd_field_shapes rejects these shapes
                // elsewhere; nothing to compare here.
                continue;
            };
            if witness_addresses.contains(&side_ip) {
                return Err(DaemonError::Config(format!(
                    "witness_url {url} resolves into the {side} replication end's address \
                     ({configured}); a witness on a data host defeats the failure-domain \
                     claim (P4a plan §3)"
                )));
            }
        }
        Ok(())
    }

    /// Fields required only when the lvm provider is selected.
    fn validate_lvm_fields(&self) -> Result<(), DaemonError> {
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
        Ok(())
    }

    /// Fields required only when the ceph provider is selected.
    fn validate_ceph_fields(&self) -> Result<(), DaemonError> {
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
        Ok(())
    }

    /// Fields required only when the drbd provider is selected.
    fn validate_drbd_fields(&self) -> Result<(), DaemonError> {
        // drbd_config_dir is NOT required: it defaults to the DRBD
        // convention /etc/drbd.d (see drbd_config_dir_or_default).
        for (missing, field) in [
            (self.drbd_vg_name.is_none(), "drbd_vg_name"),
            (self.drbd_node_name.is_none(), "drbd_node_name"),
            (self.drbd_local_address.is_none(), "drbd_local_address"),
            (self.drbd_peer_name.is_none(), "drbd_peer_name"),
            (self.drbd_peer_address.is_none(), "drbd_peer_address"),
            (
                self.drbd_shared_secret_file.is_none(),
                "drbd_shared_secret_file",
            ),
        ] {
            if missing {
                return Err(DaemonError::Config(format!(
                    "{field} is required for the drbd provider"
                )));
            }
        }
        Ok(())
    }

    /// Shape checks for individually-optional fields, independent of the
    /// selected provider (a wrong shape is a configuration mistake even
    /// when the field is currently ignored).
    fn validate_field_shapes(&self) -> Result<(), DaemonError> {
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
        self.validate_admission_fields()?;
        self.validate_drbd_field_shapes()?;
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
        // The grow-notification gate's fields (P6-B): shape only —
        // the version probe itself is deliberately NOT run here
        // (`--check-config` stays hermetic: it must not depend on
        // the host carrying a VMM binary).
        if self
            .vmm
            .cloud_hypervisor_bin
            .as_ref()
            .is_some_and(|bin| bin.as_os_str().is_empty())
        {
            return Err(DaemonError::Config(
                "vmm.cloud_hypervisor_bin must not be empty when set".to_owned(),
            ));
        }
        if let Some(minimum) = self.vmm.minimum_version.as_deref() {
            if volvisor_provider::Semver::parse(minimum).is_err() {
                return Err(DaemonError::Config(format!(
                    "vmm.minimum_version {minimum:?} is not a semantic version (expected \
                     MAJOR[.MINOR[.PATCH]] with an optional leading v, pre-release and build \
                     metadata)"
                )));
            }
        }
        Ok(())
    }

    /// Admission-control knob bounds (issue #45). The defaults are safe;
    /// these checks refuse a configuration that would disable the cap
    /// silently or make the rate limiter ill-defined.
    fn validate_admission_fields(&self) -> Result<(), DaemonError> {
        let admission = &self.admission;
        if !(1..=3600).contains(&admission.read_timeout_secs) {
            return Err(DaemonError::Config(
                "admission.read_timeout_secs must be between 1 and 3600".to_owned(),
            ));
        }
        if !(1..=300).contains(&admission.queue_timeout_secs) {
            return Err(DaemonError::Config(
                "admission.queue_timeout_secs must be between 1 and 300".to_owned(),
            ));
        }
        if !(1..=65_536).contains(&admission.max_concurrent_requests) {
            return Err(DaemonError::Config(
                "admission.max_concurrent_requests must be between 1 and 65536".to_owned(),
            ));
        }
        if admission.rate_limit_per_second > 0 && admission.rate_limit_burst == 0 {
            return Err(DaemonError::Config(
                "admission.rate_limit_burst must be at least 1 when rate_limit_per_second is non-zero"
                    .to_owned(),
            ));
        }
        if admission.rate_limit_burst > 1_000_000 {
            return Err(DaemonError::Config(
                "admission.rate_limit_burst must not exceed 1000000".to_owned(),
            ));
        }
        if !(1..=1000).contains(&admission.max_list_limit) {
            return Err(DaemonError::Config(
                "admission.max_list_limit must be between 1 and 1000".to_owned(),
            ));
        }
        Ok(())
    }

    /// Shape checks for optional drbd_* fields (the address and secret
    /// validators are the provider's own, reused so there is one source
    /// of truth).
    fn validate_drbd_field_shapes(&self) -> Result<(), DaemonError> {
        if let Some(vg) = &self.drbd_vg_name {
            if !is_simple_name(vg, 64) {
                return Err(DaemonError::Config(
                    "drbd_vg_name must be 1..=64 characters of alnum, '-' and '_'".to_owned(),
                ));
            }
        }
        if self.drbd_port_min > self.drbd_port_max {
            return Err(DaemonError::Config(
                "drbd_port_min must not exceed drbd_port_max".to_owned(),
            ));
        }
        if self.drbd_minor_min > self.drbd_minor_max {
            return Err(DaemonError::Config(
                "drbd_minor_min must not exceed drbd_minor_max".to_owned(),
            ));
        }
        if self.drbd_minor_max > 4095 {
            return Err(DaemonError::Config(
                "drbd_minor_max must not exceed 4095 (the DRBD kernel minor space)".to_owned(),
            ));
        }
        if let Some(address) = &self.drbd_local_address {
            if !volvisor_drbd::resgen::is_ipv4_literal(address) {
                return Err(DaemonError::Config(
                    "drbd_local_address must be an IPv4 dotted quad (e.g. 10.0.0.1)".to_owned(),
                ));
            }
        }
        // Node names beyond the drbdsetup status connection-line wrap
        // budget are rejected here (mirroring the provider's own
        // validation) so the daemon fails at config load, not at the
        // first status parse.
        for (field, value) in [
            ("drbd_node_name", &self.drbd_node_name),
            ("drbd_peer_name", &self.drbd_peer_name),
        ] {
            if let Some(name) = value {
                if name.chars().count() > volvisor_drbd::provider::NODE_NAME_MAX_CHARS {
                    return Err(DaemonError::Config(format!(
                        "{field} must not exceed {} characters (the drbdsetup status \
                         connection-line wrap budget)",
                        volvisor_drbd::provider::NODE_NAME_MAX_CHARS
                    )));
                }
            }
        }
        if let Some(address) = &self.drbd_peer_address {
            if let Err(e) = volvisor_drbd::provider::split_peer_address(address) {
                return Err(DaemonError::Config(format!(
                    "drbd_peer_address is malformed: {e}"
                )));
            }
        }
        if let Some(secret) = &self.drbd_shared_secret_file {
            if secret.as_os_str().is_empty() {
                return Err(DaemonError::Config(
                    "drbd_shared_secret_file must not be empty when set".to_owned(),
                ));
            }
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

    /// The effective DRBD config directory: the configured value, or
    /// the DRBD convention `/etc/drbd.d`.
    #[must_use]
    pub fn drbd_config_dir_or_default(&self) -> &std::path::PathBuf {
        static DEFAULT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
        self.drbd_config_dir
            .as_ref()
            .unwrap_or_else(|| DEFAULT.get_or_init(|| PathBuf::from("/etc/drbd.d")))
    }

    /// The effective procfs root for the DRBD module check: the
    /// configured value, or `/proc`.
    #[must_use]
    pub fn drbd_proc_root_or_default(&self) -> &std::path::PathBuf {
        static DEFAULT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
        self.drbd_proc_root
            .as_ref()
            .unwrap_or_else(|| DEFAULT.get_or_init(|| PathBuf::from("/proc")))
    }
}

/// Check a configuration file the way `--check-config` does (P7-A,
/// ADR-0009): load and validate it, and render the concise summary
/// the operator sees. This is the install smoke surface — it proves
/// the binary reads and accepts its configuration without devices or
/// a running daemon. The journal is not opened and the server does
/// not start.
///
/// # Errors
/// [`DaemonError::Config`] when the file is unreadable, malformed or
/// fails validation; the caller prints the typed error to stderr and
/// exits non-zero.
pub fn check_config(path: &std::path::Path) -> Result<String, DaemonError> {
    let config = Config::load(path)?;
    Ok(config.check_summary())
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

/// Whether `value` is a sane audit principal id: non-empty, bounded and
/// free of whitespace/control characters (it lands in structured logs).
fn is_credential_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// Require an optional path to be set and name a readable regular file.
fn require_readable_file(path: Option<&PathBuf>, field: &str) -> Result<(), DaemonError> {
    let path = path.ok_or_else(|| {
        DaemonError::Config(format!("{field} is required when tls.enabled = true"))
    })?;
    if path.as_os_str().is_empty() {
        return Err(DaemonError::Config(format!(
            "{field} must not be empty when set"
        )));
    }
    if !path.is_file() {
        return Err(DaemonError::Config(format!(
            "{field} {} does not exist or is not a regular file",
            path.display()
        )));
    }
    Ok(())
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
        assert_eq!(cfg.admission.read_timeout_secs, 30);
        assert_eq!(cfg.admission.max_concurrent_requests, 256);
        assert_eq!(cfg.admission.rate_limit_per_second, 0);
        assert_eq!(cfg.admission.rate_limit_burst, 0);
        let api = cfg.api_config();
        assert_eq!(api.max_body_bytes, 1 << 20);
        assert_eq!(api.read_timeout, std::time::Duration::from_secs(30));
        assert_eq!(api.rate_limit_per_second, 0);
        assert_eq!(api.rate_limit_burst, 0);
    }

    #[test]
    fn rate_limiting_is_opt_in() {
        let cfg: Config = toml::from_str(&minimal_toml()).expect("parse");
        assert!(
            cfg.validate().is_ok(),
            "0/0 must be a valid disabled limiter"
        );
        assert_eq!(cfg.api_config().rate_limit_per_second, 0);
        assert_eq!(cfg.api_config().rate_limit_burst, 0);

        let raw = format!(
            "{}\n[admission]\nrate_limit_per_second = 100\nrate_limit_burst = 200\n",
            minimal_toml()
        );
        let cfg: Config = toml::from_str(&raw).expect("parse");
        assert!(cfg.validate().is_ok());
        assert_eq!(cfg.api_config().rate_limit_per_second, 100);
        assert_eq!(cfg.api_config().rate_limit_burst, 200);
    }

    #[test]
    fn admission_knobs_are_validated() {
        let raw = format!(
            "{}\n[admission]\nrate_limit_per_second = 0\nrate_limit_burst = 0\nmax_list_limit = 50\n",
            minimal_toml()
        );
        let cfg: Config = toml::from_str(&raw).expect("parse");
        assert!(cfg.validate().is_ok());
        assert_eq!(cfg.api_config().max_list_limit, 50);

        let raw = format!(
            "{}\n[admission]\nrate_limit_per_second = 10\nrate_limit_burst = 0\n",
            minimal_toml()
        );
        let cfg: Config = toml::from_str(&raw).expect("parse");
        assert!(
            cfg.validate().is_err(),
            "a non-zero rate with a zero burst must be refused"
        );

        let raw = format!(
            "{}\n[admission]\nmax_concurrent_requests = 0\n",
            minimal_toml()
        );
        let cfg: Config = toml::from_str(&raw).expect("parse");
        assert!(cfg.validate().is_err());
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

    #[test]
    fn non_loopback_plain_http_listener_requires_an_explicit_downgrade() {
        let raw = minimal_toml().replace("127.0.0.1:8787", "0.0.0.0:8787");
        let cfg: Config = toml::from_str(&raw).expect("parse");
        let error = cfg.validate().expect_err("plain HTTP on a public bind");
        assert!(
            error.to_string().contains("non-loopback plain-HTTP bind"),
            "error explains the refusal: {error}"
        );

        let raw = raw + "allow_insecure = true\n";
        let cfg: Config = toml::from_str(&raw).expect("parse");
        cfg.validate()
            .expect("allow_insecure is the explicit downgrade opt-in");

        // Loopback plain HTTP remains the supported dev/test posture.
        let cfg: Config = toml::from_str(&minimal_toml()).expect("parse");
        cfg.validate().expect("loopback plain HTTP stays valid");
    }

    #[test]
    fn tls_listener_requires_certificate_and_key_files() {
        let raw = minimal_toml() + "[tls]\nenabled = true\n";
        let cfg: Config = toml::from_str(&raw).expect("parse");
        let error = cfg.validate().expect_err("missing TLS identity");
        assert!(
            error.to_string().contains("tls.cert_file"),
            "error names the missing field: {error}"
        );

        let dir = tempfile::tempdir().expect("tempdir");
        let cert = dir.path().join("server.crt");
        let key = dir.path().join("server.key");
        std::fs::write(&cert, b"certificate").expect("write cert");
        std::fs::write(&key, b"key").expect("write key");
        let raw = minimal_toml()
            + &format!(
                "[tls]\nenabled = true\ncert_file = \"{}\"\nkey_file = \"{}\"\n",
                cert.display(),
                key.display()
            );
        let cfg: Config = toml::from_str(&raw).expect("parse");
        cfg.validate()
            .expect("an existing cert/key pair is accepted");

        // A TLS identity with enabled = false is orphan configuration, not
        // a silent fallback to plain HTTP.
        let raw = minimal_toml()
            + &format!(
                "[tls]\ncert_file = \"{}\"\nkey_file = \"{}\"\n",
                cert.display(),
                key.display()
            );
        let cfg: Config = toml::from_str(&raw).expect("parse");
        let error = cfg.validate().expect_err("orphan TLS fields");
        assert!(
            error.to_string().contains("tls.enabled = false"),
            "error explains the orphan fields: {error}"
        );
    }

    #[test]
    fn tls_client_identity_is_all_or_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cert = dir.path().join("client.crt");
        std::fs::write(&cert, b"certificate").expect("write cert");
        let raw = minimal_toml() + &format!("[tls.client]\ncert_file = \"{}\"\n", cert.display());
        let cfg: Config = toml::from_str(&raw).expect("parse");
        let error = cfg.validate().expect_err("client cert without key");
        assert!(
            error.to_string().contains("configured together"),
            "error explains the pair rule: {error}"
        );
    }

    #[test]
    fn plain_http_witness_and_peer_need_the_explicit_downgrade() {
        let witness = minimal_drbd_toml()
            + "witness_url = \"http://10.0.0.3:9101\"\n\
               witness_token = \"witness-secret\"\n\
               witness_host_token = \"host-a-secret\"\n\
               witness_renewal_interval_secs = 15\n";
        let cfg: Config = toml::from_str(&witness).expect("parse");
        let error = cfg.validate().expect_err("plain HTTP witness");
        assert!(
            error.to_string().contains("must be encrypted"),
            "error explains the witness encryption rule: {error}"
        );
        let cfg: Config = toml::from_str(&(witness + "allow_insecure = true\n")).expect("parse");
        cfg.validate().expect("allow_insecure opts into plain HTTP");

        let dir = tempfile::tempdir().expect("tempdir");
        let full = migration_toml(dir.path());
        let plain_peer = full.replace("https://10.0.0.2:7780", "http://10.0.0.2:7780");
        let cfg: Config = toml::from_str(&plain_peer).expect("parse");
        let error = cfg.validate().expect_err("plain HTTP peer");
        assert!(
            error.to_string().contains("migration.peer_api_url"),
            "error names the peer URL: {error}"
        );
        let cfg: Config =
            toml::from_str(&format!("allow_insecure = true\n{plain_peer}")).expect("parse");
        cfg.validate()
            .expect("allow_insecure opts into a plain HTTP peer");
    }

    #[test]
    fn scoped_credentials_are_unique_and_role_mapped() {
        let raw = minimal_toml()
            + "[[credentials.admin]]\nid = \"admin-a\"\ntoken = \"admin-secret\"\n\
               [[credentials.operator]]\nid = \"operator-a\"\ntoken = \"operator-secret\"\n\
               [[credentials.read_only]]\nid = \"reader-a\"\ntoken = \"reader-secret\"\n";
        let cfg: Config = toml::from_str(&raw).expect("parse");
        cfg.validate().expect("unique named credentials");
        assert!(cfg.has_credentials());
        assert!(
            cfg.check_summary()
                .contains("scoped (1 admin, 1 operator, 1 read-only)")
        );
        assert_eq!(
            cfg.credentials.to_credentials(),
            vec![
                Credential {
                    id: "admin-a".to_owned(),
                    token: "admin-secret".to_owned(),
                    role: Role::Admin,
                },
                Credential {
                    id: "operator-a".to_owned(),
                    token: "operator-secret".to_owned(),
                    role: Role::Operator,
                },
                Credential {
                    id: "reader-a".to_owned(),
                    token: "reader-secret".to_owned(),
                    role: Role::ReadOnly,
                },
            ]
        );

        let duplicate_id = minimal_toml()
            + "[[credentials.admin]]\nid = \"same\"\ntoken = \"one\"\n\
               [[credentials.operator]]\nid = \"same\"\ntoken = \"two\"\n";
        let cfg: Config = toml::from_str(&duplicate_id).expect("parse");
        let error = cfg.validate().expect_err("duplicate principal id");
        assert!(
            error.to_string().contains("duplicated"),
            "error names the duplicate id: {error}"
        );

        let duplicate_token = minimal_toml()
            + "[[credentials.admin]]\nid = \"admin-a\"\ntoken = \"shared\"\n\
               [[credentials.read_only]]\nid = \"reader-a\"\ntoken = \"shared\"\n";
        let cfg: Config = toml::from_str(&duplicate_token).expect("parse");
        let error = cfg.validate().expect_err("duplicate token");
        assert!(
            error.to_string().contains("reuses a token value"),
            "error names the ambiguous token: {error}"
        );

        let legacy_collision = minimal_toml()
            + "admin_token = \"legacy\"\n\
               [[credentials.operator]]\nid = \"operator-a\"\ntoken = \"legacy\"\n";
        let cfg: Config = toml::from_str(&legacy_collision).expect("parse");
        let error = cfg.validate().expect_err("legacy token collision");
        assert!(
            error.to_string().contains("legacy admin_token"),
            "error names the collision: {error}"
        );

        let bad_id = minimal_toml()
            + "[[credentials.read_only]]\nid = \"has space\"\ntoken = \"reader-secret\"\n";
        let cfg: Config = toml::from_str(&bad_id).expect("parse");
        let error = cfg.validate().expect_err("unstable audit id");
        assert!(
            error.to_string().contains("stable audit principal id"),
            "error explains the id rule: {error}"
        );
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

    fn minimal_drbd_toml() -> String {
        "\
listen = \"127.0.0.1:8787\"
journal_dir = \"/var/lib/volvisor/journal\"
provider = \"drbd\"
drbd_vg_name = \"volvisor-nearline\"
drbd_node_name = \"host-a\"
drbd_local_address = \"10.0.0.1\"
drbd_peer_name = \"host-b\"
drbd_peer_address = \"10.0.0.2:7100\"
drbd_shared_secret_file = \"/etc/volvisor/drbd-peer-secret\"
"
        .to_owned()
    }

    #[test]
    fn parses_minimal_drbd_config() {
        let cfg: Config = toml::from_str(&minimal_drbd_toml()).expect("parse");
        assert!(cfg.validate().is_ok());
        assert_eq!(cfg.provider, ProviderKind::Drbd);
        // The documented defaults apply when unset.
        assert_eq!(
            cfg.drbd_config_dir_or_default(),
            &std::path::PathBuf::from("/etc/drbd.d")
        );
        assert_eq!(
            cfg.drbd_proc_root_or_default(),
            &std::path::PathBuf::from("/proc")
        );
        assert_eq!(cfg.drbd_port_min, 7100);
        assert_eq!(cfg.drbd_port_max, 7199);
        assert_eq!(cfg.drbd_minor_min, 100);
        assert_eq!(cfg.drbd_minor_max, 999);
    }

    #[test]
    fn drbd_provider_requires_every_end_field() {
        let raw = "\
listen = \"127.0.0.1:8787\"
journal_dir = \"/j\"
provider = \"drbd\"
";
        let cfg: Config = toml::from_str(raw).expect("parse");
        assert!(cfg.validate().is_err());
        // Each required field, added one at a time, keeps the config
        // invalid until the last one lands (drbd_config_dir is absent
        // on purpose: it defaults to /etc/drbd.d).
        let mut raw = raw.to_owned();
        for line in [
            "drbd_vg_name = \"vg\"\n",
            "drbd_node_name = \"host-a\"\n",
            "drbd_local_address = \"10.0.0.1\"\n",
            "drbd_peer_name = \"host-b\"\n",
            "drbd_peer_address = \"10.0.0.2:7100\"\n",
        ] {
            raw.push_str(line);
            let cfg: Config = toml::from_str(&raw).expect("parse");
            assert!(cfg.validate().is_err(), "still missing a required field");
        }
        raw.push_str("drbd_shared_secret_file = \"/etc/volvisor/secret\"\n");
        let cfg: Config = toml::from_str(&raw).expect("parse");
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn drbd_vg_uses_the_lvm_name_rule() {
        let raw = minimal_drbd_toml().replace(
            "drbd_vg_name = \"volvisor-nearline\"",
            "drbd_vg_name = \"has space\"",
        );
        let cfg: Config = toml::from_str(&raw).expect("parse");
        assert!(cfg.validate().is_err());
        let raw = minimal_drbd_toml().replace(
            "drbd_vg_name = \"volvisor-nearline\"",
            "drbd_vg_name = \"ok-vg\"",
        );
        let cfg: Config = toml::from_str(&raw).expect("parse");
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn drbd_ranges_must_be_ordered_and_bounded() {
        let raw = minimal_drbd_toml() + "drbd_port_min = 7200\ndrbd_port_max = 7199\n";
        let cfg: Config = toml::from_str(&raw).expect("parse");
        assert!(cfg.validate().is_err(), "inverted port range");
        let raw = minimal_drbd_toml() + "drbd_minor_min = 200\ndrbd_minor_max = 100\n";
        let cfg: Config = toml::from_str(&raw).expect("parse");
        assert!(cfg.validate().is_err(), "inverted minor range");
        let raw = minimal_drbd_toml() + "drbd_minor_max = 9999\n";
        let cfg: Config = toml::from_str(&raw).expect("parse");
        assert!(
            cfg.validate().is_err(),
            "minor space beyond the DRBD kernel limit"
        );
    }

    #[test]
    fn drbd_addresses_are_shape_checked_at_config_load() {
        // A malformed address is a Config error at load time, not an
        // INVALID_REQUEST that only surfaces after provider construction.
        let raw = minimal_drbd_toml().replace(
            "drbd_local_address = \"10.0.0.1\"",
            "drbd_local_address = \"host-a\"",
        );
        let cfg: Config = toml::from_str(&raw).expect("parse");
        let error = cfg.validate().expect_err("must refuse");
        assert!(
            error.to_string().contains("drbd_local_address"),
            "error names the field: {error}"
        );
        for bad in ["10.0.0.2", "10.0.0.2:0", "10.0.0.2:abc", "node-b:7100"] {
            let raw = minimal_drbd_toml().replace(
                "drbd_peer_address = \"10.0.0.2:7100\"",
                &format!("drbd_peer_address = \"{bad}\""),
            );
            let cfg: Config = toml::from_str(&raw).expect("parse");
            let error = cfg.validate().expect_err("must refuse");
            assert!(
                error.to_string().contains("drbd_peer_address"),
                "error names the field: {error}"
            );
        }
    }

    #[test]
    fn drbd_node_names_beyond_the_status_wrap_budget_are_refused_at_config_load() {
        // drbdsetup status wraps piped output at 80 columns; a peer
        // node name beyond the budget would push the connection line's
        // role/connection token onto a continuation line the status
        // parser rejects. Refuse it at config load, naming the field.
        let too_long = "h".repeat(volvisor_drbd::provider::NODE_NAME_MAX_CHARS + 1);
        for (field, original) in [("drbd_node_name", "host-a"), ("drbd_peer_name", "host-b")] {
            let raw = minimal_drbd_toml().replace(
                &format!("{field} = \"{original}\""),
                &format!("{field} = \"{too_long}\""),
            );
            let cfg: Config = toml::from_str(&raw).expect("parse");
            let error = cfg.validate().expect_err("must refuse");
            assert!(
                error.to_string().contains(field),
                "error names the field: {error}"
            );
        }
    }

    #[test]
    fn ceph_and_lvm_fields_are_simply_ignored_for_drbd() {
        let raw = minimal_drbd_toml()
            + "ceph_pool = \"volvisor\"\nlvm_state_path = \"/j/lvm-state.json\"\n";
        let cfg: Config = toml::from_str(&raw).expect("parse");
        assert!(cfg.validate().is_ok());
    }

    // ---------------------------------------------------------- witness

    /// A minimal witness section over the minimal drbd config: a witness
    /// on a distinct third address, with tokens and renewal interval.
    fn witness_toml() -> String {
        minimal_drbd_toml()
            + "witness_url = \"https://10.0.0.3:9101\"\n\
               witness_token = \"witness-secret\"\n\
               witness_host_token = \"host-a-secret\"\n\
               witness_renewal_interval_secs = 15\n"
    }

    #[test]
    fn parses_and_accepts_a_third_domain_witness() {
        let cfg: Config = toml::from_str(&witness_toml()).expect("parse");
        cfg.validate().expect("a distinct witness is valid");
        assert_eq!(cfg.witness_renewal_interval_secs, Some(15));
        assert_eq!(
            cfg.witness_host_token.as_deref(),
            Some("host-a-secret"),
            "the host credential parses alongside the shared token"
        );
    }

    // ------------------------------------------------------ migration

    /// A migration section over the witness fixture: an existing
    /// snapshot directory (tempdir), the peer daemon's URL and token,
    /// and a configured VMM.
    fn migration_toml(snapshot_dir: &std::path::Path) -> String {
        format!(
            "{}[migration]\n\
             enabled = true\n\
             snapshot_dir = \"{}\"\n\
             peer_api_url = \"https://10.0.0.2:7780\"\n\
             peer_api_token = \"peer-secret\"\n\
             [vmm]\n\
             ch_remote_bin = \"/usr/bin/ch-remote\"\n\
             api_socket_dir = \"/run/volvisor/vms\"\n",
            witness_toml(),
            snapshot_dir.display()
        )
    }

    #[test]
    fn parses_and_accepts_a_full_migration_section() {
        let dir = tempfile::tempdir().expect("tempdir");
        let raw = migration_toml(dir.path());
        let cfg: Config = toml::from_str(&raw).expect("parse");
        cfg.validate().expect("a full migration section is valid");
        assert!(cfg.migration.enabled);
        assert_eq!(
            cfg.vmm.ch_remote_bin.as_deref(),
            Some(std::path::Path::new("/usr/bin/ch-remote"))
        );
    }

    // --------------------------------------------------- grow gate

    #[test]
    fn validates_the_grow_gate_fields_without_probing_the_host() {
        // The path deliberately does not exist on the test host:
        // validation is shape-only (hermeticity — the probe runs at
        // daemon startup, never in --check-config).
        let raw = minimal_toml()
            + "[vmm]\n\
               cloud_hypervisor_bin = \"/nonexistent/cloud-hypervisor\"\n\
               minimum_version = \"37.0\"\n";
        let cfg: Config = toml::from_str(&raw).expect("parse");
        cfg.validate().expect("the gate fields are valid shapes");
        assert_eq!(
            cfg.vmm.minimum_version.as_deref(),
            Some("37.0"),
            "the minimum parses verbatim; the semver interpretation is the gate's"
        );
    }

    #[test]
    fn a_malformed_minimum_version_is_refused() {
        let raw = minimal_toml() + "[vmm]\nminimum_version = \"37.x\"\n";
        let cfg: Config = toml::from_str(&raw).expect("parse");
        let error = cfg.validate().expect_err("not a semantic version");
        assert!(
            error.to_string().contains("semantic version"),
            "error names the rule: {error}"
        );
    }

    #[test]
    fn an_empty_cloud_hypervisor_bin_is_refused() {
        let raw = minimal_toml() + "[vmm]\ncloud_hypervisor_bin = \"\"\n";
        let cfg: Config = toml::from_str(&raw).expect("parse");
        let error = cfg.validate().expect_err("an empty binary path");
        assert!(
            error.to_string().contains("cloud_hypervisor_bin"),
            "error names the rule: {error}"
        );
    }

    #[test]
    fn migration_requires_the_drbd_provider_and_a_witness() {
        // The handoff is the nearline replication cutover: a non-drbd
        // provider has no cut, and the cut's authority substrate
        // (barriers, RevokeSet/GrantSet) is the witness.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut raw = minimal_drbd_toml()
            + &format!(
                "[migration]\nenabled = true\nsnapshot_dir = \"{}\"\n",
                dir.path().display()
            );
        let cfg: Config = toml::from_str(&raw).expect("parse");
        let error = cfg.validate().expect_err("witness is required");
        assert!(
            error.to_string().contains("requires a witness"),
            "error names the rule: {error}"
        );
        raw = minimal_toml() + "[migration]\nenabled = true\n";
        let cfg: Config = toml::from_str(&raw).expect("parse");
        let error = cfg.validate().expect_err("drbd is required");
        assert!(
            error.to_string().contains("drbd provider only"),
            "error names the rule: {error}"
        );
    }

    #[test]
    fn migration_requires_an_existing_snapshot_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("does-not-exist");
        let raw = migration_toml(&missing);
        let cfg: Config = toml::from_str(&raw).expect("parse");
        let error = cfg.validate().expect_err("missing snapshot dir");
        assert!(
            error.to_string().contains("snapshot_dir"),
            "error names the field: {error}"
        );
    }

    #[test]
    fn migration_requires_the_peer_url_token_and_vmm() {
        let dir = tempfile::tempdir().expect("tempdir");
        let full = migration_toml(dir.path());
        for removed in [
            "peer_api_url = \"https://10.0.0.2:7780\"\n",
            "peer_api_token = \"peer-secret\"\n",
            "[vmm]\nch_remote_bin = \"/usr/bin/ch-remote\"\napi_socket_dir = \"/run/volvisor/vms\"\n",
        ] {
            let raw = full.replace(removed, "");
            let cfg: Config = toml::from_str(&raw).expect("parse");
            let error = cfg.validate().expect_err("incomplete migration section");
            assert!(!error.to_string().is_empty(), "a typed refusal is produced");
        }
        // The peer surface accepts https (and http on loopback); a
        // different scheme is refused up front.
        let raw = full.replace("https://10.0.0.2:7780", "ftp://10.0.0.2:7780");
        let cfg: Config = toml::from_str(&raw).expect("parse");
        let error = cfg.validate().expect_err("non-http peer url");
        assert!(
            error.to_string().contains("peer_api_url"),
            "error names the field: {error}"
        );
    }

    #[test]
    fn an_inert_migration_section_validates_nothing() {
        // enabled = false: the fields are dead config — no snapshot
        // dir, peer or VMM is required until the deployment opts in.
        let raw = witness_toml() + "[migration]\nenabled = false\nsnapshot_dir = \"/nope\"\n";
        let cfg: Config = toml::from_str(&raw).expect("parse");
        cfg.validate().expect("inert migration section");
    }

    #[test]
    fn witness_url_requires_the_host_token() {
        // A v2 witness refuses mutations from the shared token: the
        // daemon's own grant/renew/revoke path needs the per-host
        // credential or it would fail at the first attach.
        let raw = minimal_drbd_toml()
            + "witness_url = \"https://10.0.0.3:9101\"\n\
               witness_token = \"t\"\n\
               witness_renewal_interval_secs = 15\n";
        let cfg: Config = toml::from_str(&raw).expect("parse");
        let error = cfg.validate().expect_err("host token is mandatory");
        assert!(
            error.to_string().contains("witness_host_token"),
            "error names the field: {error}"
        );
        // An empty host token is no host token.
        let raw = minimal_drbd_toml()
            + "witness_url = \"https://10.0.0.3:9101\"\n\
               witness_token = \"t\"\n\
               witness_host_token = \"  \"\n\
               witness_renewal_interval_secs = 15\n";
        let cfg: Config = toml::from_str(&raw).expect("parse");
        let error = cfg.validate().expect_err("empty host token refused");
        assert!(
            error.to_string().contains("witness_host_token"),
            "error names the field: {error}"
        );
    }

    #[test]
    fn witness_fields_without_url_are_refused() {
        for extra in [
            "witness_token = \"t\"\n",
            "witness_host_token = \"t\"\n",
            "witness_renewal_interval_secs = 15\n",
        ] {
            let cfg: Config = toml::from_str(&(minimal_drbd_toml() + extra)).expect("parse");
            let error = cfg.validate().expect_err("orphan witness field");
            assert!(
                error.to_string().contains("without witness_url"),
                "error names the rule: {error}"
            );
        }
    }

    #[test]
    fn witness_url_requires_the_drbd_provider() {
        let raw = minimal_toml()
            + "witness_url = \"https://10.0.0.3:9101\"\n\
               witness_token = \"t\"\n\
               witness_renewal_interval_secs = 15\n";
        let cfg: Config = toml::from_str(&raw).expect("parse");
        let error = cfg.validate().expect_err("lvm has no witness");
        assert!(
            error.to_string().contains("drbd provider only"),
            "error names the rule: {error}"
        );
    }

    #[test]
    fn witness_requires_a_positive_renewal_interval() {
        for interval in [None, Some(0)] {
            let raw = minimal_drbd_toml()
                + "witness_url = \"https://10.0.0.3:9101\"\n\
                   witness_token = \"t\"\n"
                + &interval.map_or(String::new(), |i| {
                    format!("witness_renewal_interval_secs = {i}\n")
                });
            let cfg: Config = toml::from_str(&raw).expect("parse");
            let error = cfg
                .validate()
                .expect_err("the renewal interval is mandatory and positive");
            assert!(
                error.to_string().contains("witness_renewal_interval_secs"),
                "error names the field: {error}"
            );
        }
    }

    #[test]
    fn loopback_witness_is_refused_as_colocation() {
        for url in ["http://127.0.0.1:9101", "http://[::1]:9101"] {
            let raw = minimal_drbd_toml()
                + &format!(
                    "witness_url = \"{url}\"\n\
                     witness_token = \"t\"\n\
                     witness_renewal_interval_secs = 15\n"
                );
            let cfg: Config = toml::from_str(&raw).expect("parse");
            let error = cfg.validate().expect_err("loopback witness");
            assert!(
                error.to_string().contains("third failure domain"),
                "error explains the colocation refusal: {error}"
            );
        }
    }

    #[test]
    fn non_loopback_witness_requires_a_token() {
        let raw = minimal_drbd_toml()
            + "witness_url = \"https://10.0.0.3:9101\"\n\
               witness_host_token = \"h\"\n\
               witness_renewal_interval_secs = 15\n";
        let cfg: Config = toml::from_str(&raw).expect("parse");
        let error = cfg.validate().expect_err("tokenless non-loopback witness");
        assert!(
            error.to_string().contains("witness_token is required"),
            "error names the rule: {error}"
        );
    }

    #[test]
    fn witness_colocated_with_either_replication_end_is_refused() {
        for (url, side) in [
            ("https://10.0.0.1:9101", "local"),
            ("https://10.0.0.2:9101", "peer"),
        ] {
            let raw = minimal_drbd_toml()
                + &format!(
                    "witness_url = \"{url}\"\n\
                     witness_token = \"t\"\n\
                     witness_host_token = \"h\"\n\
                     witness_renewal_interval_secs = 15\n"
                );
            let cfg: Config = toml::from_str(&raw).expect("parse");
            let error = cfg.validate().expect_err("colocated witness");
            assert!(
                error
                    .to_string()
                    .contains(&format!("{side} replication end")),
                "error names the side ({side}): {error}"
            );
        }
    }

    #[test]
    fn witness_url_accepts_https_and_refuses_other_schemes() {
        // https is the production shape; the host credential is required
        // exactly as for plain http, and the scheme itself validates.
        let raw = minimal_drbd_toml()
            + "witness_url = \"https://10.0.0.3:9101\"\n\
               witness_token = \"witness-secret\"\n\
               witness_host_token = \"host-a-secret\"\n\
               witness_renewal_interval_secs = 15\n";
        let cfg: Config = toml::from_str(&raw).expect("parse");
        cfg.validate().expect("https witness is valid");

        let raw = minimal_drbd_toml()
            + "witness_url = \"ftp://10.0.0.3:9101\"\n\
               witness_token = \"t\"\n\
               witness_host_token = \"h\"\n\
               witness_renewal_interval_secs = 15\n";
        let cfg: Config = toml::from_str(&raw).expect("parse");
        let error = cfg.validate().expect_err("ftp witness");
        assert!(
            error.to_string().contains("http:// or https://"),
            "error names the scheme rule: {error}"
        );
    }

    #[test]
    fn unresolvable_witness_names_refuse_startup() {
        let raw = minimal_drbd_toml()
            + "witness_url = \"https://witness.invalid:9101\"\n\
               witness_token = \"t\"\n\
               witness_renewal_interval_secs = 15\n";
        let cfg: Config = toml::from_str(&raw).expect("parse");
        let error = cfg.validate().expect_err("unresolvable witness");
        assert!(
            error.to_string().contains("cannot be resolved"),
            "ambiguity is treated as colocation: {error}"
        );
    }

    #[test]
    fn resolvable_witness_names_count_every_address() {
        // `localhost` resolves to loopback on every POSIX host: the
        // name-based resolution path must catch what a literal
        // comparison alone would miss.
        let raw = minimal_drbd_toml()
            + "witness_url = \"http://localhost:9101\"\n\
               witness_token = \"t\"\n\
               witness_renewal_interval_secs = 15\n";
        let cfg: Config = toml::from_str(&raw).expect("parse");
        let error = cfg.validate().expect_err("loopback via name");
        assert!(
            error.to_string().contains("third failure domain"),
            "the resolved loopback is caught: {error}"
        );
    }

    #[test]
    fn witness_host_parses_urls_with_ports_and_paths() {
        let addresses =
            witness_host("https://10.0.0.3:9101/v1").expect("literal with port and path");
        let expected: std::net::IpAddr = "10.0.0.3".parse().expect("ip");
        assert_eq!(addresses, vec![expected]);
        let addresses = witness_host("http://10.0.0.3").expect("literal without port");
        assert_eq!(addresses, vec![expected]);
        assert!(witness_host("https://10.0.0.3:9101").is_ok());
        let loopback: std::net::IpAddr = "::1".parse().expect("ip");
        assert_eq!(
            witness_host("https://[::1]").expect("bracketed IPv6 without port"),
            vec![loopback]
        );
        assert_eq!(
            witness_host("https://[::1]:9101").expect("bracketed IPv6 with port"),
            vec![loopback]
        );
        assert_eq!(
            outbound_url_parts("http://[::1]", "peer").expect("bracketed IPv6"),
            ("http", "::1")
        );
        assert_eq!(
            outbound_url_parts("https://[2001:db8::1]:9101/v1", "peer")
                .expect("bracketed IPv6 with port"),
            ("https", "2001:db8::1")
        );
        assert!(witness_host("http://").is_err(), "no host");
        assert!(witness_host("ftp://10.0.0.3").is_err(), "wrong scheme");
    }

    #[test]
    fn host_of_strips_the_port() {
        assert_eq!(host_of("10.0.0.2:7100"), "10.0.0.2");
        assert_eq!(host_of("10.0.0.2"), "10.0.0.2");
    }
}
