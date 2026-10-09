//! Parsing of `drbdsetup status` text and LVM JSON reports.
//!
//! The `drbdsetup status <res>` **text** form (non-tty, non-verbose,
//! single-volume) was verified against the drbd-utils source before
//! this parser was written; the fake test world emits the same shape
//! verbatim, so provider and simulation can never drift. The verified
//! grammar for one resource is:
//!
//! ```text
//! <res> role:<Primary|Secondary>            # indent 0
//!   [suspended:<reasons>]                   # same line, when any
//!   [force-io-failures:<yes|no>]            #   suspension is active
//!   disk:<DiskState>                        # indent 2 — ONE line:
//!   [client:<...>]                          #   device_status prints
//!   [quorum:<yes|no>]                       #   every token through
//!   [open:<yes|no>]                         #   the column-oriented
//!                                           #   wrap_printf
//!   <peer-node> role:<Role>                 # indent 2, when Connected
//!     [replication:<X>] peer-disk:<DiskState>   # indent 4, when Connected
//!     [peer-client:<...>] [done:%.2f] [resync-suspended:<...>]
//!   <peer-node> connection:<CState>         # indent 2, when NOT Connected
//! <blank line>
//! ```
//!
//! Line structure is the detail most easily gotten wrong, so it is
//! pinned here with its sources: `drbdsetup.c`'s `resource_status`,
//! `device_status`, `peer_device_status` and `connection_status`
//! print through `wrap_printf` (user/shared/wrap_printf.c), which is
//! **column-oriented** — consecutive calls with the same indentation
//! continue the SAME line until an explicit `\n`. A real non-verbose
//! status therefore carries `disk:`, `quorum:` and `open:` on one
//! indent-2 line (confirmed against real drbd-utils 9.30.0 /
//! kmod 9.2.12 output: `  disk:UpToDate open:yes`), and
//! `replication:` on the peer-device line BEFORE `peer-disk:` (only
//! while resyncing/verifying — an established peer prints no
//! `replication:` token at all), with `done:%.2f` carrying no `%`
//! suffix. The token scans below are order-independent so a future
//! re-ordering still parses, but unknown tokens fail loudly.
//!
//! The `open:` token is printed unconditionally by drbd-utils against
//! kernel 9.2.9 and newer, so every real non-verbose status of this
//! generation carries it; the parser keeps it optional only for older
//! kernels. `quorum:` appears only when quorum is enabled on the
//! resource — a volvisor resource cannot print it today (volvisor
//! never enables quorum), but the parser must not explode the day
//! that changes. `suspended:` appears on the resource line whenever
//! any suspension reason is set (the operator `drbdsetup
//! suspend-io`, fencing, quorum, or the kernel's no-data-access
//! suspension after local data-access loss); `force-io-failures:` is
//! printed when the resource is configured to fail local I/O. The
//! disk-failure observation path additionally depends on the
//! **device-line** `client:` marker: a local disk that failed and
//! detached renders as `disk:Diskless client:no`, so rejecting that
//! token would turn every disk-failure verdict into an INTERNAL
//! parse failure — the exact wrong direction for the unhealthiest
//! observable state.
//!
//! The connection is named by the **peer node name** (the mesh `on
//! <host>` configuration names connections after the peer host);
//! when it is `Connected` the peer's role is printed on that line
//! instead of a connection state. An unknown resource makes
//! `drbdsetup` exit non-zero with `<res>: No such resource` on
//! stderr (callers detect this by stderr content; the runner does
//! not expose exit codes).
//!
//! LVM parsing mirrors `volvisor-lvm`'s report module (the crates
//! deliberately do not share it): permissive `Option` fields, sizes as
//! numbers or strings, and — the one addition here — `lv_tags`, which
//! plain-JSON `lvs` emits as a single comma-separated string while
//! `--reportformat json_std` emits an array; both are accepted.

use serde::Deserialize;
use serde::de::DeserializeOwned;
use volvisor_types::{ApiError, ApiErrorCode};

/// The local or peer role of a resource.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// The resource is Primary on this node.
    Primary,
    /// The resource is Secondary on this node.
    Secondary,
}

impl Role {
    /// Parse the `role:` value; anything else is an honest error (a
    /// role is never guessed — dual-primary handling is out of scope
    /// and must surface loudly, not silently map).
    fn parse(value: &str) -> Result<Self, ApiError> {
        match value {
            "Primary" => Ok(Self::Primary),
            "Secondary" => Ok(Self::Secondary),
            other => Err(parse_error(format!("unknown role {other:?}"))),
        }
    }
}

/// A disk state as printed by `drbdsetup status`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiskState {
    /// Consistent, no resync needed.
    UpToDate,
    /// Freshly created metadata, never seeded.
    Inconsistent,
    /// Consistent but behind (a usable but stale replica).
    Outdated,
    /// Consistent without being UpToDate/Outdated (only meaningful
    /// with an established connection).
    Consistent,
    /// Local I/O failed.
    Failed,
    /// No local backing attached.
    Diskless,
    /// Disk state unknown (typical while disconnected).
    DUnknown,
    /// Any other spelling this build prints (kept verbatim, never
    /// guessed into a stronger claim).
    Other(String),
}

impl DiskState {
    fn parse(value: &str) -> Self {
        match value {
            "UpToDate" => Self::UpToDate,
            "Inconsistent" => Self::Inconsistent,
            "Outdated" => Self::Outdated,
            "Consistent" => Self::Consistent,
            "Failed" => Self::Failed,
            "Diskless" => Self::Diskless,
            "DUnknown" => Self::DUnknown,
            other => Self::Other(other.to_owned()),
        }
    }
}

/// A connection state as printed by `drbdsetup status`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionState {
    /// Connection established.
    Connected,
    /// Trying to connect.
    WFConnection,
    /// No connection configured/attempted.
    StandAlone,
    /// Any other spelling this build prints.
    Other(String),
}

impl ConnectionState {
    fn parse(value: &str) -> Self {
        match value {
            "Connected" => Self::Connected,
            "WFConnection" => Self::WFConnection,
            "StandAlone" => Self::StandAlone,
            other => Self::Other(other.to_owned()),
        }
    }

    /// Whether this state reports an established connection.
    #[must_use]
    pub fn is_connected(&self) -> bool {
        matches!(self, Self::Connected)
    }
}

/// The parsed status of one running DRBD resource.
///
/// `Eq` is deliberately not derived: `resync_done` is an observed
/// percentage and float equality is meaningless there.
#[derive(Clone, Debug, PartialEq)]
pub struct ResourceStatus {
    /// The resource name from the status line.
    pub name: String,
    /// The local role.
    pub role: Role,
    /// The local disk state.
    pub local_disk: DiskState,
    /// Whether the resource reports an established connection (the
    /// peer-role line form, or an explicit `connection:Connected`).
    pub connected: bool,
    /// The connection state line, when the status used the
    /// `connection:` form (i.e. the peer role line was absent).
    pub connection: Option<ConnectionState>,
    /// The peer's role, when connected.
    pub peer_role: Option<Role>,
    /// The peer's disk state, when connected.
    pub peer_disk: Option<DiskState>,
    /// The replication direction while resyncing or verifying
    /// (`SyncSource`, `SyncTarget`, ...) — printed only when the
    /// replication state is beyond `Established`, BEFORE
    /// `peer-disk:` on the peer-device line.
    pub replication: Option<String>,
    /// Resync progress (`done:` percentage), when resyncing. Real
    /// drbdsetup prints the value with no `%` suffix; a trailing `%`
    /// is tolerated.
    pub resync_done: Option<f64>,
    /// Whether the local device is currently held open (the indent-2
    /// `open:` token, printed unconditionally by drbd-utils against
    /// kernel 9.2.9 and newer). `None` when the token is absent (an
    /// older kernel); a `Some(false)` here is the detached-resource
    /// fact the demotion path can rely on.
    pub local_open: Option<bool>,
    /// The quorum verdict (the indent-2 `quorum:` token), present only
    /// when quorum is enabled on the resource.
    pub quorum: Option<bool>,
    /// The resource-level suspension reason list (`suspended:` on the
    /// resource line, verbatim), present only while any suspension is
    /// active: the operator `drbdsetup suspend-io` (`user`), the
    /// kernel's no-data-access suspension (`no-data`), fencing
    /// (`fencing`) or quorum (`quorum`), comma-combined. `None` = not
    /// suspended. I/O on a suspended resource is frozen, so this is a
    /// first-class health fact, not decoration.
    pub suspended: Option<String>,
    /// Whether the resource is configured to fail local I/O
    /// (`force-io-failures:yes` on the resource line — the
    /// disk-failure / fencing emulation path). Printed only when set,
    /// so `None` means the token was absent.
    pub force_io_failures: Option<bool>,
}

/// An `INTERNAL` parse error for `drbdsetup status` output.
fn parse_error(detail: impl Into<String>) -> ApiError {
    ApiError::new(
        ApiErrorCode::Internal,
        format!(
            "failed to parse `drbdsetup status` output: {}",
            detail.into()
        ),
    )
}

/// Parse a `yes`/`no` token value (`open:`, `quorum:`); anything else
/// is an honest error (fail-closed on unknown spellings).
fn parse_yes_no(field: &str, value: &str) -> Result<bool, ApiError> {
    match value {
        "yes" => Ok(true),
        "no" => Ok(false),
        other => Err(parse_error(format!(
            "unknown {field} value {other:?} (expected yes|no)"
        ))),
    }
}

/// The mutable parse accumulator for one `drbdsetup status` output.
#[derive(Default)]
struct StatusFields {
    name: Option<String>,
    role: Option<Role>,
    local_disk: Option<DiskState>,
    connection: Option<ConnectionState>,
    connected: bool,
    peer_role: Option<Role>,
    peer_disk: Option<DiskState>,
    replication: Option<String>,
    resync_done: Option<f64>,
    local_open: Option<bool>,
    quorum: Option<bool>,
    suspended: Option<String>,
    force_io_failures: Option<bool>,
}

impl StatusFields {
    /// Resource line (indent 0): `<res> role:<Role>` optionally
    /// followed by `suspended:<reasons>` and
    /// `force-io-failures:<yes|no>` (all on the same line;
    /// drbdsetup.c prints the qualifiers only when a suspension is
    /// active / I/O failing is configured).
    fn parse_resource_line(&mut self, tokens: &[&str], line: &str) -> Result<(), ApiError> {
        self.name = Some(tokens[0].to_owned());
        for token in &tokens[1..] {
            if let Some(value) = token.strip_prefix("role:") {
                self.role = Some(Role::parse(value)?);
            } else if let Some(value) = token.strip_prefix("suspended:") {
                if value.is_empty() {
                    return Err(parse_error(format!(
                        "malformed resource line (empty suspended:) {line:?}"
                    )));
                }
                self.suspended = Some(value.to_owned());
            } else if let Some(value) = token.strip_prefix("force-io-failures:") {
                self.force_io_failures = Some(parse_yes_no("force-io-failures", value)?);
            } else {
                return Err(parse_error(format!("malformed resource line {line:?}")));
            }
        }
        Ok(())
    }

    /// Indent-2 line: the device line (`disk:`, `client:`,
    /// `quorum:`, `open:` — all on ONE line, see the module grammar)
    /// or the peer line (`<peer-node> role:<Role>` when connected,
    /// `<peer-node> connection:<State>` when not).
    fn parse_device_or_peer_line(&mut self, tokens: &[&str], line: &str) -> Result<(), ApiError> {
        let first = tokens[0];
        if first.starts_with("volume:") {
            return Err(parse_error(
                "multi-volume status output is not supported by this parser",
            ));
        }
        if first.starts_with("disk:")
            || first.starts_with("client:")
            || first.starts_with("quorum:")
            || first.starts_with("open:")
        {
            for token in tokens {
                if let Some(value) = token.strip_prefix("disk:") {
                    self.local_disk = Some(DiskState::parse(value));
                } else if token.starts_with("client:") {
                    // Local diskless marker (`client:no|yes`); a known
                    // spelling this parser does not model — tolerated,
                    // never guessed from.
                } else if let Some(value) = token.strip_prefix("quorum:") {
                    self.quorum = Some(parse_yes_no("quorum", value)?);
                } else if let Some(value) = token.strip_prefix("open:") {
                    self.local_open = Some(parse_yes_no("open", value)?);
                } else {
                    return Err(parse_error(format!("malformed device line {line:?}")));
                }
            }
        } else if tokens.len() == 2 && tokens[1].starts_with("role:") {
            self.peer_role = Some(Role::parse(tokens[1].trim_start_matches("role:"))?);
            self.connected = true;
        } else if tokens.len() == 2 && tokens[1].starts_with("connection:") {
            // Disconnected peer line:
            // `  <peer-node> connection:<State>`.
            let state = ConnectionState::parse(tokens[1].trim_start_matches("connection:"));
            self.connected = self.connected || state.is_connected();
            self.connection = Some(state);
        } else {
            return Err(parse_error(format!("malformed line {line:?}")));
        }
        Ok(())
    }

    /// Peer-device line (indent 4): `[replication:<X>]
    /// peer-disk:<DiskState> [peer-client:<...>] [done:%.2f]
    /// [resync-suspended:<...>]` — order-independent token scan (real
    /// drbdsetup prints `replication:` BEFORE `peer-disk:`; see the
    /// module grammar). `peer-disk:` must be present.
    fn parse_peer_device_line(&mut self, tokens: &[&str], line: &str) -> Result<(), ApiError> {
        let mut have_peer_disk = false;
        for token in tokens {
            if let Some(value) = token.strip_prefix("peer-disk:") {
                self.peer_disk = Some(DiskState::parse(value));
                have_peer_disk = true;
            } else if let Some(value) = token.strip_prefix("replication:") {
                self.replication = Some(value.to_owned());
            } else if let Some(value) = token.strip_prefix("done:") {
                self.resync_done = value.trim_end_matches('%').parse().ok();
            } else if token.starts_with("peer-client:") || token.starts_with("resync-suspended:") {
                // Known spellings this parser does not model.
            } else if token.starts_with("volume:") {
                return Err(parse_error(
                    "multi-volume status output is not supported by this parser",
                ));
            } else {
                return Err(parse_error(format!("malformed peer-device line {line:?}")));
            }
        }
        if !have_peer_disk {
            return Err(parse_error(format!(
                "peer-device line without peer-disk: {line:?}"
            )));
        }
        Ok(())
    }
}

/// Parse the text output of `drbdsetup status <res>` (one resource).
///
/// Strict where the grammar is known (a resource line with a role, a
/// local `disk:` line) and permissive about ordering of the optional
/// peer lines. Only the verified single-volume shape is supported;
/// multi-volume output (with `volume:` prefixes) is a typed error, not
/// a guess.
///
/// # Errors
/// `INTERNAL` when the output does not match the verified grammar.
pub fn parse_drbdsetup_status(stdout: &str) -> Result<ResourceStatus, ApiError> {
    let mut fields = StatusFields::default();

    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        let tokens: Vec<&str> = line.split_whitespace().collect();
        let Some(first) = tokens.first().copied() else {
            continue;
        };
        match indent {
            0 => fields.parse_resource_line(&tokens, line)?,
            2 => fields.parse_device_or_peer_line(&tokens, line)?,
            4 => fields.parse_peer_device_line(&tokens, line)?,
            // Deeper nesting or unexpected indentation: multi-volume
            // output (`volume:0`) and other unknown shapes fail loudly.
            _ => {
                if first.starts_with("volume:") {
                    return Err(parse_error(
                        "multi-volume status output is not supported by this parser",
                    ));
                }
                return Err(parse_error(format!("unexpected indentation in {line:?}")));
            }
        }
    }

    let name = fields.name.ok_or_else(|| parse_error("no resource line"))?;
    let role = fields
        .role
        .ok_or_else(|| parse_error("resource line carries no role"))?;
    let local_disk = fields
        .local_disk
        .ok_or_else(|| parse_error("no local disk: line"))?;
    Ok(ResourceStatus {
        name,
        role,
        local_disk,
        connected: fields.connected,
        connection: fields.connection,
        peer_role: fields.peer_role,
        peer_disk: fields.peer_disk,
        replication: fields.replication,
        resync_done: fields.resync_done,
        local_open: fields.local_open,
        quorum: fields.quorum,
        suspended: fields.suspended,
        force_io_failures: fields.force_io_failures,
    })
}

// ---------------------------------------------------------------------------
// LVM report parsing (mirrors volvisor-lvm's report module; kept
// deliberately unshared so each crate stays independently reviewable)
// ---------------------------------------------------------------------------

/// A JSON scalar that is either a number or a numeric string.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum FlexibleNumber {
    /// A JSON number.
    Number(u64),
    /// A string holding a decimal number (LVM report style).
    Text(String),
}

impl FlexibleNumber {
    /// The numeric value, or `None` when the text is not a plain decimal.
    #[must_use]
    pub fn to_u64(&self) -> Option<u64> {
        match self {
            Self::Number(value) => Some(*value),
            Self::Text(text) => text.trim().parse().ok(),
        }
    }
}

/// Parse an LVM JSON report: `{"report":[{"<key>":[{...row...}]}]}`.
///
/// Unknown sections and rows without the expected key are skipped; a row
/// that exists but cannot be deserialized is an `INTERNAL` error (never a
/// silent drop of state-relevant data).
///
/// # Errors
/// `INTERNAL` when the output is not JSON or a row fails to deserialize.
pub fn parse_report<T: DeserializeOwned>(
    stdout: &str,
    section_key: &str,
) -> Result<Vec<T>, ApiError> {
    let value: serde_json::Value = serde_json::from_str(stdout).map_err(|e| {
        ApiError::new(
            ApiErrorCode::Internal,
            format!("failed to parse LVM JSON report ({section_key}): {e}"),
        )
    })?;
    let mut rows = Vec::new();
    let Some(reports) = value.get("report").and_then(serde_json::Value::as_array) else {
        return Ok(rows);
    };
    for section in reports {
        let Some(section_rows) = section
            .get(section_key)
            .and_then(serde_json::Value::as_array)
        else {
            continue;
        };
        for row in section_rows {
            let row: T = serde_json::from_value(row.clone()).map_err(|e| {
                ApiError::new(
                    ApiErrorCode::Internal,
                    format!("failed to parse LVM report row ({section_key}): {e}"),
                )
            })?;
            rows.push(row);
        }
    }
    Ok(rows)
}

/// One `vgs` report row.
#[derive(Clone, Debug, Deserialize)]
pub struct VgRow {
    /// Volume group name.
    #[serde(default)]
    pub vg_name: Option<String>,
    /// Free space in the VG, in bytes.
    #[serde(default)]
    pub vg_free: Option<FlexibleNumber>,
    /// Total size of the VG, in bytes.
    #[serde(default)]
    pub vg_size: Option<FlexibleNumber>,
    /// Physical extent size of the VG, in bytes (LVM rounds every
    /// allocation up to whole extents).
    #[serde(default)]
    pub vg_extent_size: Option<FlexibleNumber>,
}

impl VgRow {
    /// Free bytes, when reported and parseable.
    #[must_use]
    pub fn free_bytes(&self) -> Option<u64> {
        self.vg_free.as_ref().and_then(FlexibleNumber::to_u64)
    }

    /// Total bytes, when reported and parseable.
    #[must_use]
    pub fn size_bytes(&self) -> Option<u64> {
        self.vg_size.as_ref().and_then(FlexibleNumber::to_u64)
    }

    /// Physical extent bytes, when reported and parseable.
    #[must_use]
    pub fn extent_bytes(&self) -> Option<u64> {
        self.vg_extent_size
            .as_ref()
            .and_then(FlexibleNumber::to_u64)
    }
}

/// One `lvs` report row.
#[derive(Clone, Debug, Deserialize)]
pub struct LvRow {
    /// Volume group name.
    #[serde(default)]
    pub vg_name: Option<String>,
    /// Logical volume name.
    #[serde(default)]
    pub lv_name: Option<String>,
    /// Logical volume size in bytes.
    #[serde(default)]
    pub lv_size: Option<FlexibleNumber>,
    /// The `lv_tags` column. Plain-JSON `lvs` reports a single
    /// comma-separated string (`--reportformat json`); the `json_std`
    /// format reports an array of strings. Both are accepted.
    #[serde(default)]
    pub lv_tags: Option<serde_json::Value>,
}

impl LvRow {
    /// The `vg/lv` path of this row, when both names are present.
    #[must_use]
    pub fn vg_slash_lv(&self) -> Option<String> {
        match (&self.vg_name, &self.lv_name) {
            (Some(vg), Some(lv)) => Some(format!("{vg}/{lv}")),
            _ => None,
        }
    }

    /// Size in bytes, when reported and parseable.
    #[must_use]
    pub fn size_bytes(&self) -> Option<u64> {
        self.lv_size.as_ref().and_then(FlexibleNumber::to_u64)
    }

    /// The value of one LV tag (`<key>=<value>`), accepting both the
    /// comma-separated string form and the `json_std` array form.
    #[must_use]
    pub fn tag(&self, key: &str) -> Option<String> {
        let raw = self.lv_tags.as_ref()?;
        let prefix = format!("{key}=");
        let extract = |tag: &str| tag.strip_prefix(&prefix).map(str::to_owned);
        match raw {
            serde_json::Value::String(text) => text.split(',').find_map(|tag| extract(tag.trim())),
            serde_json::Value::Array(items) => items
                .iter()
                .filter_map(serde_json::Value::as_str)
                .find_map(extract),
            _ => None,
        }
    }
}

/// The parsed `drbdsetup show-gi` report (the verified v9 shape: the
/// ASCII header, the `dt_print_v9_uuids` line and the flag legend —
/// see the test fixture's `GiSet` documentation for the byte-level
/// provenance).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GiReport {
    /// The non-zero data-generation UUIDs of the reported set (current,
    /// bitmap base, history slots), as zero-padded uppercase 16-hex
    /// digits — the lineage identifiers the registration records and
    /// the adopt flow compares as a set.
    pub lineage_uuids: Vec<String>,
}

/// Parse `drbdsetup show-gi <resource> <peer-node-id> <volume>` output.
///
/// Only the `dt_print_v9_uuids` line is structural for volvisor: four
/// 16-hex-digit UUID fields (`current:bitmap:history:history`),
/// zero meaning "unset". The twelve flag digits that follow are
/// kernel-computed and carry no identity — ignored here.
///
/// # Errors
/// `INTERNAL` when no UUID line is present — the lineage is never
/// guessed.
pub fn parse_drbdsetup_show_gi(stdout: &str) -> Result<GiReport, ApiError> {
    for line in stdout.lines() {
        let fields: Vec<&str> = line.trim().split(':').collect();
        if fields.len() < 16 {
            continue;
        }
        let uuids = &fields[..4];
        if uuids.iter().all(|field| {
            field.len() == 16
                && field
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_lowercase())
        }) {
            let lineage_uuids = uuids
                .iter()
                .filter(|field| field.bytes().any(|byte| byte != b'0'))
                .map(|field| (*field).to_owned())
                .collect();
            return Ok(GiReport { lineage_uuids });
        }
    }
    Err(ApiError::new(
        ApiErrorCode::Internal,
        "failed to parse `drbdsetup show-gi` output: no data-generation UUID line".to_owned(),
    ))
}

/// Parse the decimal-bytes output of `blockdev --getsize64 <dev>`
/// (a single decimal number followed by a newline).
///
/// # Errors
/// `INTERNAL` when the output is not a plain decimal number — the
/// device size is never guessed.
pub fn parse_blockdev_size(stdout: &str) -> Result<u64, ApiError> {
    stdout.trim().parse::<u64>().map_err(|e| {
        ApiError::new(
            ApiErrorCode::Internal,
            format!("failed to parse `blockdev --getsize64` output {stdout:?}: {e}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_connected_single_volume_shape() {
        // Verified grammar: resource line, ONE device line carrying
        // disk:/open: (wrap_printf columns), peer line named by the
        // peer node name, peer-disk line, trailing blank line.
        let stdout = "vol-r0 role:Secondary\n  \
                      disk:UpToDate open:no\n  \
                      node-b role:Secondary\n    \
                      peer-disk:UpToDate\n\n";
        let status = parse_drbdsetup_status(stdout).expect("parse");
        assert_eq!(status.name, "vol-r0");
        assert_eq!(status.role, Role::Secondary);
        assert_eq!(status.local_disk, DiskState::UpToDate);
        assert_eq!(status.local_open, Some(false));
        assert_eq!(status.quorum, None);
        assert_eq!(status.suspended, None);
        assert_eq!(status.force_io_failures, None);
        assert!(status.connected);
        assert_eq!(status.connection, None);
        assert_eq!(status.peer_role, Some(Role::Secondary));
        assert_eq!(status.peer_disk, Some(DiskState::UpToDate));
        assert_eq!(status.replication, None);
        assert_eq!(status.resync_done, None);
    }

    #[test]
    fn parses_the_disconnected_shape() {
        let stdout = "vol-r0 role:Primary\n  \
                      disk:UpToDate open:yes\n  \
                      node-b connection:WFConnection\n\n";
        let status = parse_drbdsetup_status(stdout).expect("parse");
        assert_eq!(status.role, Role::Primary);
        assert_eq!(status.local_open, Some(true));
        assert!(!status.connected);
        assert_eq!(status.connection, Some(ConnectionState::WFConnection));
        assert_eq!(status.peer_role, None);
        assert_eq!(status.peer_disk, None);
    }

    #[test]
    fn parses_resync_progress_on_the_peer_disk_line() {
        // Real print order: `replication:` BEFORE `peer-disk:`,
        // `done:` with no `%` suffix (drbdsetup.c peer_device_status
        // via the column-oriented wrap_printf).
        let stdout = "vol-r0 role:Primary\n  \
                      disk:UpToDate open:no\n  \
                      node-b role:Secondary\n    \
                      replication:SyncSource peer-disk:Inconsistent done:12.50\n\n";
        let status = parse_drbdsetup_status(stdout).expect("parse");
        assert_eq!(status.peer_disk, Some(DiskState::Inconsistent));
        assert_eq!(status.replication.as_deref(), Some("SyncSource"));
        assert_eq!(status.resync_done, Some(12.5));
    }

    /// A verbatim `drbdsetup status <res>` sample exactly as
    /// drbd-utils 9.29.0 prints it against kernel >= 9.2.9,
    /// constructed in drbdsetup.c's print order (role; ONE device
    /// line with disk + open — unconditional; peer line with role
    /// when connected; peer-disk with `replication:` FIRST and
    /// `done:%.2f` without `%` while resyncing). Line structure
    /// cross-confirmed against real drbd-utils 9.30.0 / kmod 9.2.12
    /// output (`  disk:UpToDate open:yes`).
    #[test]
    fn parses_a_verbatim_drbd_utils_9_29_0_connected_status() {
        let stdout = "vol-r0 role:Secondary\n  \
                      disk:UpToDate open:yes\n  \
                      node-b role:Secondary\n    \
                      replication:SyncSource peer-disk:Inconsistent done:37.50\n\n";
        let status = parse_drbdsetup_status(stdout).expect("parse");
        assert_eq!(status.name, "vol-r0");
        assert_eq!(status.role, Role::Secondary);
        assert_eq!(status.local_disk, DiskState::UpToDate);
        assert_eq!(status.local_open, Some(true));
        assert!(status.connected);
        assert_eq!(status.peer_role, Some(Role::Secondary));
        assert_eq!(status.peer_disk, Some(DiskState::Inconsistent));
        assert_eq!(status.replication.as_deref(), Some("SyncSource"));
        assert_eq!(status.resync_done, Some(37.5));
    }

    /// The disconnected WFConnection variant of the verbatim 9.29.0
    /// shape (peer line carries `connection:` instead of a role, no
    /// peer-disk line).
    #[test]
    fn parses_a_verbatim_drbd_utils_9_29_0_wfconnection_status() {
        let stdout = "vol-r0 role:Primary\n  \
                      disk:UpToDate open:yes\n  \
                      node-b connection:WFConnection\n\n";
        let status = parse_drbdsetup_status(stdout).expect("parse");
        assert_eq!(status.role, Role::Primary);
        assert_eq!(status.local_disk, DiskState::UpToDate);
        assert_eq!(status.local_open, Some(true));
        assert!(!status.connected);
        assert_eq!(status.connection, Some(ConnectionState::WFConnection));
        assert_eq!(status.peer_role, None);
        assert_eq!(status.peer_disk, None);
        assert_eq!(status.replication, None);
    }

    #[test]
    fn parses_the_quorum_token_when_quorum_is_enabled() {
        // A volvisor resource cannot print this today (quorum is never
        // enabled), but the spelling is a known grammar element: it
        // must parse, not explode. On real output it shares the
        // device line with disk: and open:.
        let stdout = "vol-r0 role:Secondary\n  \
                      disk:UpToDate quorum:yes open:no\n  \
                      node-b role:Secondary\n    \
                      peer-disk:UpToDate\n\n";
        let status = parse_drbdsetup_status(stdout).expect("parse");
        assert_eq!(status.quorum, Some(true));
        assert_eq!(status.local_open, Some(false));
    }

    /// The resource-line suspension qualifiers (drbdsetup.c
    /// resource_status): `suspended:` whenever any suspension reason
    /// is set (operator suspend-io, fencing, quorum, or the kernel's
    /// no-data-access suspension), `force-io-failures:` when I/O
    /// failing is configured. Both share the resource line.
    #[test]
    fn parses_the_resource_line_suspension_qualifiers() {
        let stdout = "vol-r0 role:Primary suspended:no-data\n  \
                      disk:UpToDate open:yes\n  \
                      node-b role:Secondary\n    \
                      peer-disk:UpToDate\n\n";
        let status = parse_drbdsetup_status(stdout).expect("parse");
        assert_eq!(status.suspended.as_deref(), Some("no-data"));
        assert_eq!(status.force_io_failures, None);

        let stdout = "vol-r0 role:Primary suspended:user force-io-failures:yes\n  \
                      disk:UpToDate open:yes\n\n";
        let status = parse_drbdsetup_status(stdout).expect("parse");
        assert_eq!(status.suspended.as_deref(), Some("user"));
        assert_eq!(status.force_io_failures, Some(true));
    }

    /// A local diskless device prints `client:<...>` on the device
    /// line and a diskless peer prints `peer-client:<...>` after
    /// `peer-disk:` (non-tty output); both are known spellings this
    /// parser tolerates without modeling.
    #[test]
    fn tolerates_the_diskless_client_markers() {
        let stdout = "vol-r0 role:Secondary\n  \
                      disk:Diskless client:no open:yes\n  \
                      node-b role:Secondary\n    \
                      peer-disk:Diskless peer-client:no\n\n";
        let status = parse_drbdsetup_status(stdout).expect("parse");
        assert_eq!(status.local_disk, DiskState::Diskless);
        assert_eq!(status.peer_disk, Some(DiskState::Diskless));
    }

    #[test]
    fn rejects_malformed_and_multi_volume_output() {
        assert!(parse_drbdsetup_status("garbage\n").is_err());
        assert!(parse_drbdsetup_status("vol-r0 role:SideWays\n  disk:UpToDate\n").is_err());
        assert!(parse_drbdsetup_status("vol-r0 role:Primary\n").is_err()); // no disk line
        // Unknown yes/no spellings on the known device-line tokens
        // stay malformed (fail-closed).
        assert!(
            parse_drbdsetup_status("vol-r0 role:Primary\n  disk:UpToDate open:maybe\n").is_err()
        );
        assert!(parse_drbdsetup_status("vol-r0 role:Primary\n  disk:UpToDate quorum:1\n").is_err());
        // An unknown token on the device line or the peer-device
        // line, and a peer-device line without `peer-disk:`, fail
        // loudly rather than being silently dropped.
        assert!(
            parse_drbdsetup_status("vol-r0 role:Primary\n  disk:UpToDate frobnicated:yes\n")
                .is_err()
        );
        assert!(
            parse_drbdsetup_status(
                "vol-r0 role:Primary\n  disk:UpToDate\n  node-b role:Secondary\n    done:12.50\n"
            )
            .is_err()
        );
        // Single-volume grammar has no `volume:0` prefix; a multi-volume
        // shape must fail loudly instead of being mis-parsed (on the
        // device line and on the peer-device line alike).
        assert!(parse_drbdsetup_status("vol-r0 role:Primary\n  volume:0 disk:UpToDate\n").is_err());
        assert!(
            parse_drbdsetup_status(
                "vol-r0 role:Primary\n  disk:UpToDate\n    volume:0 peer-disk:UpToDate\n"
            )
            .is_err()
        );
    }

    #[test]
    fn unknown_disk_states_are_kept_verbatim() {
        // Legacy pre-9.2.9 shape: no `open:` token (the parser keeps
        // it optional for older kernels) and an unrecognized disk
        // spelling.
        let stdout = "vol-r0 role:Secondary\n  disk:SomeFutureState\n\n";
        let status = parse_drbdsetup_status(stdout).expect("parse");
        assert_eq!(
            status.local_disk,
            DiskState::Other("SomeFutureState".to_owned())
        );
        assert_eq!(status.local_open, None);
    }

    #[test]
    fn lvm_report_parses_string_and_number_sizes() {
        let stdout = r#"{"report":[{"vg":[
            {"vg_name":"vg0","vg_free":"1024","vg_size":"2048","vg_extent_size":"4194304"},
            {"vg_name":"vg1","vg_free":10,"vg_size":20}
        ]}]}"#;
        let rows: Vec<VgRow> = parse_report(stdout, "vg").expect("parse");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].free_bytes(), Some(1024));
        assert_eq!(rows[0].extent_bytes(), Some(4_194_304));
        assert_eq!(rows[1].free_bytes(), Some(10));
        let empty: Vec<LvRow> = parse_report(stdout, "lv").expect("parse");
        assert!(empty.is_empty());
    }

    #[test]
    fn lv_tags_accept_comma_string_and_array_forms() {
        let string_form = r#"{"report":[{"lv":[
            {"vg_name":"vg0","lv_name":"lv0","lv_size":"1024",
             "lv_tags":"volvisor.owner=vol-1,volvisor.generation=1"}
        ]}]}"#;
        let rows: Vec<LvRow> = parse_report(string_form, "lv").expect("parse");
        assert_eq!(rows[0].tag("volvisor.owner").as_deref(), Some("vol-1"));
        assert_eq!(rows[0].tag("volvisor.generation").as_deref(), Some("1"));
        assert_eq!(rows[0].tag("other"), None);

        let array_form = r#"{"report":[{"lv":[
            {"vg_name":"vg0","lv_name":"lv0","lv_size":2048,
             "lv_tags":["volvisor.owner=vol-2","unrelated"]}
        ]}]}"#;
        let rows: Vec<LvRow> = parse_report(array_form, "lv").expect("parse");
        assert_eq!(rows[0].tag("volvisor.owner").as_deref(), Some("vol-2"));
        assert_eq!(rows[0].size_bytes(), Some(2048));

        // Absent tags column: no tag, no error.
        let absent = r#"{"report":[{"lv":[
            {"vg_name":"vg0","lv_name":"lv0"}
        ]}]}"#;
        let rows: Vec<LvRow> = parse_report(absent, "lv").expect("parse");
        assert_eq!(rows[0].tag("volvisor.owner"), None);
    }

    #[test]
    fn blockdev_size_parses_decimal_bytes() {
        assert_eq!(parse_blockdev_size("2147483648\n").expect("size"), 2 << 30);
        assert!(parse_blockdev_size("").is_err());
        assert!(parse_blockdev_size("not a number").is_err());
    }

    #[test]
    fn show_gi_parser_extracts_nonzero_uuids_from_the_verified_shape() {
        // The verbatim v9 UUID line (dt_print_v9_uuids,
        // drbdtool_common.c:64-87): four 16-hex-digit fields over the
        // twelve flag digits.
        let text = "\n       +--<  Current data generation UUID  >-\n\
       V               V                 V         V\n\
4B3BDA92B09E4EC7:0000000000000000:0A1B2C3D4E5F6071:0000000000000000:0:0:0:0:1:0:0:0:0:0:0:0\n\
                                                                    ^ ^ ^ ^ ^ ^ ^ ^ ^ ^ ^ ^\n";
        let report = parse_drbdsetup_show_gi(text).expect("parse");
        assert_eq!(
            report.lineage_uuids,
            vec!["4B3BDA92B09E4EC7".to_owned(), "0A1B2C3D4E5F6071".to_owned()]
        );
        // No UUID line: never guessed.
        assert!(parse_drbdsetup_show_gi("no header\nonly legend\n").is_err());
        // Lowercase hex is not the verified shape (X64(016) is
        // %016lX — uppercase): refused rather than mapped.
        assert!(parse_drbdsetup_show_gi(
            "4b3bda92b09e4ec7:0000000000000000:0000000000000000:0000000000000000:0:0:0:0:1:0:0:0:0:0:0:0\n"
        )
        .is_err());
    }
}
