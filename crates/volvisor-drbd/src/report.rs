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
//!   disk:<DiskState>                        # indent 2
//!   <peer-node> role:<Role>                 # indent 2, when Connected
//!     peer-disk:<DiskState>                 # indent 4, when Connected
//!     [replication:<SyncTarget|...> done:%.2f]
//!   <peer-node> connection:<CState>         # indent 2, when NOT Connected
//! <blank line>
//! ```
//!
//! The connection is named by the **peer node name**; when it is
//! `Connected` the peer's role is printed on that line instead of a
//! connection state. An unknown resource makes `drbdsetup` exit
//! non-zero with `<res>: No such resource` on stderr (callers detect
//! this by stderr content; the runner does not expose exit codes).
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
    /// The replication direction during resync (`SyncTarget`, ...).
    pub replication: Option<String>,
    /// Resync progress (`done:` percentage), when resyncing.
    pub resync_done: Option<f64>,
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
    let mut name: Option<String> = None;
    let mut role: Option<Role> = None;
    let mut local_disk: Option<DiskState> = None;
    let mut connection: Option<ConnectionState> = None;
    let mut connected = false;
    let mut peer_role: Option<Role> = None;
    let mut peer_disk: Option<DiskState> = None;
    let mut replication: Option<String> = None;
    let mut resync_done: Option<f64> = None;

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
            // Resource line: `<res> role:<Role>`.
            0 => {
                if tokens.len() != 2 || !tokens[1].starts_with("role:") {
                    return Err(parse_error(format!("malformed resource line {line:?}")));
                }
                name = Some(first.to_owned());
                role = Some(Role::parse(tokens[1].trim_start_matches("role:"))?);
            }
            // Device/connection line (`disk:`, `connection:`) or the
            // connected peer line (`<peer-node> role:<Role>`).
            2 => {
                if let Some(value) = first.strip_prefix("disk:") {
                    local_disk = Some(DiskState::parse(value));
                } else if let Some(value) = first.strip_prefix("connection:") {
                    let state = ConnectionState::parse(value);
                    connected = connected || state.is_connected();
                    connection = Some(state);
                } else if tokens.len() == 2 && tokens[1].starts_with("role:") {
                    peer_role = Some(Role::parse(tokens[1].trim_start_matches("role:"))?);
                    connected = true;
                } else if tokens.len() == 2 && tokens[1].starts_with("connection:") {
                    // Disconnected peer line:
                    // `  <peer-node> connection:<State>`.
                    let state = ConnectionState::parse(tokens[1].trim_start_matches("connection:"));
                    connected = connected || state.is_connected();
                    connection = Some(state);
                } else {
                    return Err(parse_error(format!("malformed line {line:?}")));
                }
            }
            // Peer-device line: `peer-disk:<DiskState>` optionally
            // followed by `replication:<X>` and `done:<N>`.
            4 => {
                if let Some(value) = first.strip_prefix("peer-disk:") {
                    peer_disk = Some(DiskState::parse(value));
                }
                for token in &tokens[1..] {
                    if let Some(value) = token.strip_prefix("replication:") {
                        replication = Some(value.to_owned());
                    } else if let Some(value) = token.strip_prefix("done:") {
                        resync_done = value.trim_end_matches('%').parse().ok();
                    }
                }
            }
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

    let name = name.ok_or_else(|| parse_error("no resource line"))?;
    let role = role.ok_or_else(|| parse_error("resource line carries no role"))?;
    let local_disk = local_disk.ok_or_else(|| parse_error("no local disk: line"))?;
    Ok(ResourceStatus {
        name,
        role,
        local_disk,
        connected,
        connection,
        peer_role,
        peer_disk,
        replication,
        resync_done,
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
        // Verified grammar: resource line, disk line, peer-role line
        // (connection named by the PEER NODE NAME), peer-disk line,
        // trailing blank line.
        let stdout = "vol-r0 role:Secondary\n  \
                      disk:UpToDate\n  \
                      node-b role:Secondary\n    \
                      peer-disk:UpToDate\n\n";
        let status = parse_drbdsetup_status(stdout).expect("parse");
        assert_eq!(status.name, "vol-r0");
        assert_eq!(status.role, Role::Secondary);
        assert_eq!(status.local_disk, DiskState::UpToDate);
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
                      disk:UpToDate\n  \
                      node-b connection:WFConnection\n\n";
        let status = parse_drbdsetup_status(stdout).expect("parse");
        assert_eq!(status.role, Role::Primary);
        assert!(!status.connected);
        assert_eq!(status.connection, Some(ConnectionState::WFConnection));
        assert_eq!(status.peer_role, None);
        assert_eq!(status.peer_disk, None);
    }

    #[test]
    fn parses_resync_progress_on_the_peer_disk_line() {
        let stdout = "vol-r0 role:Primary\n  \
                      disk:UpToDate\n  \
                      node-b role:Secondary\n    \
                      peer-disk:Inconsistent replication:SyncTarget done:12.50%\n\n";
        let status = parse_drbdsetup_status(stdout).expect("parse");
        assert_eq!(status.peer_disk, Some(DiskState::Inconsistent));
        assert_eq!(status.replication.as_deref(), Some("SyncTarget"));
        assert_eq!(status.resync_done, Some(12.5));
    }

    #[test]
    fn rejects_malformed_and_multi_volume_output() {
        assert!(parse_drbdsetup_status("garbage\n").is_err());
        assert!(parse_drbdsetup_status("vol-r0 role:SideWays\n  disk:UpToDate\n").is_err());
        assert!(parse_drbdsetup_status("vol-r0 role:Primary\n").is_err()); // no disk line
        // Single-volume grammar has no `volume:0` prefix; a multi-volume
        // shape must fail loudly instead of being mis-parsed.
        assert!(parse_drbdsetup_status("vol-r0 role:Primary\n  volume:0 disk:UpToDate\n").is_err());
    }

    #[test]
    fn unknown_disk_states_are_kept_verbatim() {
        let stdout = "vol-r0 role:Secondary\n  disk:SomeFutureState\n\n";
        let status = parse_drbdsetup_status(stdout).expect("parse");
        assert_eq!(
            status.local_disk,
            DiskState::Other("SomeFutureState".to_owned())
        );
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
}
