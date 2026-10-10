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
    #[serde(default)]
    pub admin_token: Option<String>,
    /// Maximum request body size in bytes.
    #[serde(default = "default_max_body_bytes")]
    pub max_body_bytes: usize,
    /// Base URL of the writer-authority witness (drbd provider only,
    /// e.g. `http://10.0.0.3:9101`). When set, the provider is
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

/// The `[vmm]` table: the ch-remote adapter's knobs (P4b plan §5/§6).
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
}

fn default_max_body_bytes() -> usize {
    1 << 20
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
/// system resolver with every returned address counting. The witness
/// surface is plain HTTP in P4a, so only the `http://` scheme is
/// accepted.
///
/// # Errors
/// [`DaemonError::Config`] for a non-HTTP scheme, an unparseable or
/// empty host, or a name that cannot be resolved — the guard treats
/// ambiguity as colocation and refuses, and a witness the daemon
/// cannot resolve is unusable anyway.
fn witness_host(url: &str) -> Result<Vec<std::net::IpAddr>, DaemonError> {
    let rest = url.strip_prefix("http://").ok_or_else(|| {
        DaemonError::Config(format!(
            "witness_url {url} must use the http:// scheme (the witness surface is plain \
             HTTP in P4a)"
        ))
    })?;
    let authority = rest.split('/').next().unwrap_or(rest);
    // `host:port` (IPv6 literals are bracketed, as in `[::1]:9101`).
    let host = authority
        .rsplit_once(':')
        .map_or(authority, |(host, _)| host);
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host.is_empty() {
        return Err(DaemonError::Config(format!(
            "witness_url {url} carries no host"
        )));
    }
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return Ok(vec![ip]);
    }
    let resolved: Vec<std::net::IpAddr> =
        std::net::ToSocketAddrs::to_socket_addrs(&format!("{host}:80"))
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
        format!(
            "config ok: listen {}, provider {}, journal_dir {}, {witness}, {migration}",
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
        if !peer_url.starts_with("http://")
            || peer_url
                .strip_prefix("http://")
                .is_some_and(|rest| rest.trim_matches('/').is_empty())
        {
            return Err(DaemonError::Config(
                "migration.peer_api_url must use the http:// scheme and name a host".to_owned(),
            ));
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
            + "witness_url = \"http://10.0.0.3:9101\"\n\
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
             peer_api_url = \"http://10.0.0.2:7780\"\n\
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
            "peer_api_url = \"http://10.0.0.2:7780\"\n",
            "peer_api_token = \"peer-secret\"\n",
            "[vmm]\nch_remote_bin = \"/usr/bin/ch-remote\"\napi_socket_dir = \"/run/volvisor/vms\"\n",
        ] {
            let raw = full.replace(removed, "");
            let cfg: Config = toml::from_str(&raw).expect("parse");
            let error = cfg.validate().expect_err("incomplete migration section");
            assert!(!error.to_string().is_empty(), "a typed refusal is produced");
        }
        // An https peer URL is refused (the peer surface is plain
        // HTTP in this phase, like the witness).
        let raw = full.replace("http://10.0.0.2:7780", "https://10.0.0.2:7780");
        let cfg: Config = toml::from_str(&raw).expect("parse");
        let error = cfg.validate().expect_err("https peer url");
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
            + "witness_url = \"http://10.0.0.3:9101\"\n\
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
            + "witness_url = \"http://10.0.0.3:9101\"\n\
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
            + "witness_url = \"http://10.0.0.3:9101\"\n\
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
                + "witness_url = \"http://10.0.0.3:9101\"\n\
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
            + "witness_url = \"http://10.0.0.3:9101\"\n\
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
            ("http://10.0.0.1:9101", "local"),
            ("http://10.0.0.2:9101", "peer"),
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
    fn witness_url_must_be_plain_http() {
        let raw = minimal_drbd_toml()
            + "witness_url = \"https://10.0.0.3:9101\"\n\
               witness_token = \"t\"\n\
               witness_renewal_interval_secs = 15\n";
        let cfg: Config = toml::from_str(&raw).expect("parse");
        let error = cfg.validate().expect_err("https witness");
        assert!(
            error.to_string().contains("http://"),
            "error names the scheme rule: {error}"
        );
    }

    #[test]
    fn unresolvable_witness_names_refuse_startup() {
        let raw = minimal_drbd_toml()
            + "witness_url = \"http://witness.invalid:9101\"\n\
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
            witness_host("http://10.0.0.3:9101/v1").expect("literal with port and path");
        let expected: std::net::IpAddr = "10.0.0.3".parse().expect("ip");
        assert_eq!(addresses, vec![expected]);
        let addresses = witness_host("http://10.0.0.3").expect("literal without port");
        assert_eq!(addresses, vec![expected]);
        assert!(witness_host("http://10.0.0.3:9101").is_ok());
        assert!(witness_host("http://").is_err(), "no host");
        assert!(witness_host("ftp://10.0.0.3").is_err(), "wrong scheme");
    }

    #[test]
    fn host_of_strips_the_port() {
        assert_eq!(host_of("10.0.0.2:7100"), "10.0.0.2");
        assert_eq!(host_of("10.0.0.2"), "10.0.0.2");
    }
}
