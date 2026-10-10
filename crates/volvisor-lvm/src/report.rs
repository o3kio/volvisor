//! Permissive parsing of CLI JSON reports (`lsblk`, LVM `vgs`/`lvs`/`pvs`).
//!
//! CLI JSON output drifts across tool versions: sizes appear as JSON
//! numbers (`lsblk --bytes`) or as strings (`lvm --units b --nosuffix`
//! still emits strings), and columns may be absent. Parsing here is
//! therefore permissive — every field is an `Option` with
//! `#[serde(default)]` — and callers treat a missing field as "unknown",
//! mapping it to honest typed errors instead of defaults that pretend
//! knowledge.

use serde::Deserialize;
use serde::de::DeserializeOwned;
use volvisor_types::{ApiError, ApiErrorCode};

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
    /// allocation up to whole extents; larger extents on big arrays
    /// make the rounding coarser).
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
    /// The `lv_attr` state string (10 characters; permissive — only
    /// diagnostics read it, never a decision).
    #[serde(default)]
    pub lv_attr: Option<String>,
    /// Mirror/raid sync percentage (`copy_percent`). Empty on LVM
    /// 2.03.x while a `pvmove` runs (the progress lives on the
    /// hidden `pvmove` segment, not the LV row) — parsed
    /// permissively, never required.
    #[serde(default)]
    pub copy_percent: Option<String>,
    /// The LV's backing devices, comma-separated `pv(extent)`
    /// entries (e.g. `/dev/sda(0),/dev/sdb(12)`). While a `pvmove`
    /// is active the entries reference the temporary mirror segment
    /// (e.g. `pvmove0(0)`) instead of the real PVs — that reference
    /// is the reliable "a move is in progress" observation on LVM
    /// 2.03.x.
    #[serde(default)]
    pub devices: Option<String>,
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

    /// The distinct backing PV names of this row's device list, in
    /// report order, with the `(extent)` suffixes stripped.
    ///
    /// `pvmove` mirror segments (`pvmove0`, ...) are **kept** as
    /// names: callers distinguish "still moving" (a `pvmove*` name
    /// present) from "moved" (only real PV names, none of them the
    /// source) by inspection.
    #[must_use]
    pub fn device_pvs(&self) -> Vec<&str> {
        let Some(devices) = self.devices.as_deref() else {
            return Vec::new();
        };
        let mut names: Vec<&str> = Vec::new();
        for entry in devices.split(',') {
            let name = entry.split('(').next().unwrap_or(entry).trim();
            if !name.is_empty() && !names.contains(&name) {
                names.push(name);
            }
        }
        names
    }

    /// Whether a `pvmove` mirror segment is active for this LV (the
    /// device list references a `pvmove*` segment).
    ///
    /// This is the honest "moving" observation on LVM 2.03.x: the
    /// LV-row `copy_percent` stays empty during a background move
    /// and the `lv_attr` change is a subtle case shift, but the
    /// devices column verifiably references the temporary segment
    /// until the move lands.
    #[must_use]
    pub fn move_segment_active(&self) -> bool {
        self.device_pvs()
            .iter()
            .any(|name| name.starts_with("pvmove"))
    }

    /// The sync percentage when reported as a plain number (empty
    /// during background moves on LVM 2.03.x — diagnostics only).
    #[must_use]
    /// The sync percentage, truncated toward zero (LVM prints two
    /// decimals, e.g. `12.34` → 12 — the conservative read: a
    /// supervision loop is never more done than reported). An empty
    /// column — the LV row during a background move, the shape
    /// verified against LVM 2.03.16 (the progress lives on the
    /// hidden mirror segment) — is `None`.
    pub fn copy_percent(&self) -> Option<u64> {
        let text = self.copy_percent.as_deref()?.trim();
        if text.is_empty() {
            return None;
        }
        let (whole, _frac) = text.split_once('.')?;
        whole.parse().ok()
    }
}

/// One `pvs` report row.
#[derive(Clone, Debug, Deserialize)]
pub struct PvRow {
    /// Physical volume device path.
    #[serde(default)]
    pub pv_name: Option<String>,
    /// The volume group this PV belongs to (empty/absent when the PV
    /// is not in any VG).
    #[serde(default)]
    pub vg_name: Option<String>,
    /// Total PV size in bytes.
    #[serde(default)]
    pub pv_size: Option<FlexibleNumber>,
    /// Free space on this PV in bytes (the same-VG move's target
    /// capacity check).
    #[serde(default)]
    pub pv_free: Option<FlexibleNumber>,
}

impl PvRow {
    /// Free bytes on this PV, when reported and parseable.
    #[must_use]
    pub fn free_bytes(&self) -> Option<u64> {
        self.pv_free.as_ref().and_then(FlexibleNumber::to_u64)
    }

    /// Total bytes of this PV, when reported and parseable.
    #[must_use]
    pub fn size_bytes(&self) -> Option<u64> {
        self.pv_size.as_ref().and_then(FlexibleNumber::to_u64)
    }
}

/// Root of `lsblk --json --bytes` output.
#[derive(Clone, Debug, Deserialize)]
pub struct LsblkOutput {
    /// Top-level block devices (partitions are nested children, ignored).
    #[serde(default)]
    pub blockdevices: Vec<LsblkDevice>,
}

/// One `lsblk` device entry.
#[derive(Clone, Debug, Deserialize)]
pub struct LsblkDevice {
    /// Kernel device name (e.g. `sda`, `nvme0n1`).
    #[serde(default)]
    pub name: Option<String>,
    /// Device type (`disk`, `loop`, `ram`, `part`, ...).
    #[serde(rename = "type", default)]
    pub device_type: Option<String>,
    /// Size in bytes.
    #[serde(default)]
    pub size: Option<FlexibleNumber>,
    /// Device serial, when the kernel exposes one.
    #[serde(default)]
    pub serial: Option<String>,
    /// Device WWN (NVMe NGUID/EUI64 or SCSI WWN), when present.
    #[serde(default)]
    pub wwn: Option<String>,
    /// Device model string.
    #[serde(default)]
    pub model: Option<String>,
}

impl LsblkDevice {
    /// Size in bytes, when reported and parseable.
    #[must_use]
    pub fn size_bytes(&self) -> Option<u64> {
        self.size.as_ref().and_then(FlexibleNumber::to_u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flexible_number_accepts_numbers_and_strings() {
        assert_eq!(FlexibleNumber::Number(42).to_u64(), Some(42));
        assert_eq!(
            FlexibleNumber::Text(" 1073741824 ".to_owned()).to_u64(),
            Some(1_073_741_824)
        );
        assert_eq!(
            FlexibleNumber::Text("not-a-number".to_owned()).to_u64(),
            None
        );
    }

    #[test]
    fn lvm_report_parses_nested_sections() {
        let stdout = r#"{
            "report": [
                {"vg": [{"vg_name": "vg0", "vg_free": "1024", "vg_size": "2048"}]},
                {"vg": [{"vg_name": "vg1", "vg_free": 10, "vg_size": 20}]}
            ]
        }"#;
        let rows: Vec<VgRow> = parse_report(stdout, "vg").expect("parse");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].vg_name.as_deref(), Some("vg0"));
        assert_eq!(rows[0].free_bytes(), Some(1024));
        assert_eq!(rows[1].free_bytes(), Some(10));

        // A report without the requested key yields no rows, not an error.
        let empty: Vec<LvRow> = parse_report(stdout, "lv").expect("parse");
        assert!(empty.is_empty());
    }

    #[test]
    fn lvm_report_rejects_non_json_loudly() {
        let err = parse_report::<LvRow>("not json", "lv").expect_err("must fail");
        assert_eq!(err.code, ApiErrorCode::Internal);
    }

    #[test]
    fn vg_rows_parse_the_extent_size_permissively() {
        // Present (string-style, as LVM emits it even with --units b).
        let stdout = r#"{"report":[{"vg":[
            {"vg_name":"vg0","vg_free":"1024","vg_extent_size":"33554432"}
        ]}]}"#;
        let rows: Vec<VgRow> = parse_report(stdout, "vg").expect("parse");
        assert_eq!(rows[0].extent_bytes(), Some(33_554_432));

        // Numeric style is accepted too.
        let stdout = r#"{"report":[{"vg":[
            {"vg_name":"vg0","vg_free":"1024","vg_extent_size":4194304}
        ]}]}"#;
        let rows: Vec<VgRow> = parse_report(stdout, "vg").expect("parse");
        assert_eq!(rows[0].extent_bytes(), Some(4_194_304));

        // Absent or unparseable -> None (the caller applies its own
        // 4-MiB default; a fabricated value here would be dishonest).
        let stdout = r#"{"report":[{"vg":[
            {"vg_name":"vg0","vg_free":"1024"}
        ]}]}"#;
        let rows: Vec<VgRow> = parse_report(stdout, "vg").expect("parse");
        assert_eq!(rows[0].extent_bytes(), None);
    }

    #[test]
    fn lsblk_output_parses_missing_optional_columns() {
        let stdout = r#"{"blockdevices":[
            {"name":"sda","type":"disk","size":1000204886016,
             "serial":"S6PXND0R123456","wwn":"0x5000c50015ead127","model":"SSD"},
            {"name":"loop0","type":"loop","size":100}
        ]}"#;
        let parsed: LsblkOutput = serde_json::from_str(stdout).expect("parse");
        assert_eq!(parsed.blockdevices.len(), 2);
        assert_eq!(parsed.blockdevices[0].size_bytes(), Some(1_000_204_886_016));
        assert_eq!(
            parsed.blockdevices[0].wwn.as_deref(),
            Some("0x5000c50015ead127")
        );
        assert_eq!(parsed.blockdevices[1].model, None);
    }

    /// The move-observation columns, shaped on real LVM 2.03.16 JSON
    /// (verified against a loop-device VG): a mid-move LV references
    /// the `pvmove` segment in `devices` with an empty
    /// `copy_percent`; a settled LV lists its real PVs.
    #[test]
    fn lv_row_parses_move_observation_columns() {
        let moving = r#"{"report":[{"lv":[
            {"vg_name":"testvg","lv_name":"biglv","lv_attr":"-wI-a-----",
             "copy_percent":"","devices":"pvmove0(0)"}
        ]}]}"#;
        let rows: Vec<LvRow> = parse_report(moving, "lv").expect("parse");
        assert!(rows[0].move_segment_active(), "the mirror segment is named");
        assert_eq!(rows[0].device_pvs(), vec!["pvmove0"]);
        assert_eq!(rows[0].copy_percent(), None, "empty string parses to None");

        let settled = r#"{"report":[{"lv":[
            {"vg_name":"testvg","lv_name":"biglv","lv_attr":"-wi-a-----",
             "copy_percent":"100.00","devices":"/dev/loop1(0)"}
        ]}]}"#;
        let rows: Vec<LvRow> = parse_report(settled, "lv").expect("parse");
        assert!(!rows[0].move_segment_active());
        assert_eq!(rows[0].device_pvs(), vec!["/dev/loop1"]);
        assert_eq!(rows[0].copy_percent(), Some(100));

        // A multi-PV spread lists every distinct PV once, extent
        // suffixes stripped — the single-source scope check reads
        // exactly this list.
        let spread = r#"{"report":[{"lv":[
            {"vg_name":"testvg","lv_name":"lv2","lv_attr":"-wi-a-----",
             "copy_percent":"","devices":"/dev/sda(0),/dev/sdb(12),/dev/sda(32)"}
        ]}]}"#;
        let rows: Vec<LvRow> = parse_report(spread, "lv").expect("parse");
        assert_eq!(rows[0].device_pvs(), vec!["/dev/sda", "/dev/sdb"]);
    }

    /// The pvs move-validation columns (real LVM 2.03.16 default
    /// column set): PV → VG membership plus per-PV free space.
    #[test]
    fn pv_row_parses_vg_and_free_columns() {
        let stdout = r#"{"report":[{"pv":[
            {"pv_name":"/dev/loop0","vg_name":"testvg","pv_size":"130023424",
             "pv_free":"113246208"},
            {"pv_name":"/dev/loop1","vg_name":"testvg","pv_size":"130023424",
             "pv_free":"130023424"}
        ]}]}"#;
        let rows: Vec<PvRow> = parse_report(stdout, "pv").expect("parse");
        assert_eq!(rows[0].vg_name.as_deref(), Some("testvg"));
        assert_eq!(rows[0].free_bytes(), Some(113_246_208));
        assert_eq!(rows[1].free_bytes(), Some(130_023_424));
        assert_eq!(rows[1].size_bytes(), Some(130_023_424));
    }
}
