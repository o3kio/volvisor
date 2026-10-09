//! Permissive parsing of `ceph`/`rbd` CLI JSON output.
//!
//! CLI JSON output drifts across tool versions: sizes may appear as JSON
//! numbers or as strings, `rbd showmapped --format json` prints an object
//! with a `devices` array (a bare array and a legacy keyed object appear
//! in older builds), and fields may be absent. Parsing here is therefore permissive — every field is an
//! `Option` with `#[serde(default)]`, unknown fields are ignored — and
//! callers treat a missing field as "unknown", mapping it to honest typed
//! errors instead of defaults that pretend knowledge. A payload that is
//! not JSON at all is a typed `INTERNAL` error naming the command.

use serde::Deserialize;
use volvisor_types::domain::Health;
use volvisor_types::{ApiError, ApiErrorCode};

/// A JSON scalar that is either a number or a numeric string.
///
/// Mirrors the LVM report module's type of the same name (the two crates
/// deliberately do not share it; keep them in sync stylistically).
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum FlexibleNumber {
    /// A JSON number.
    Number(u64),
    /// A string holding a decimal number.
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

/// An `INTERNAL` parse error naming the command whose output was malformed.
fn parse_error(command: &str, detail: impl Into<String>) -> ApiError {
    ApiError::new(
        ApiErrorCode::Internal,
        format!("failed to parse `{command}` output: {}", detail.into()),
    )
}

/// Parse the output of `ceph fsid`.
///
/// The classic CLI prints the bare UUID on stdout; some builds emit
/// `{"fsid": "..."}` JSON. Both forms are accepted permissively (trimmed
/// non-JSON text, or the JSON `fsid` field); empty output is an honest
/// `INTERNAL` error, never a fabricated identity.
pub fn parse_fsid(stdout: &str) -> Result<String, ApiError> {
    let trimmed = stdout.trim();
    if trimmed.starts_with('{') {
        #[derive(Deserialize)]
        struct FsidJson {
            #[serde(default)]
            fsid: Option<String>,
        }
        let parsed: FsidJson = serde_json::from_str(trimmed)
            .map_err(|e| parse_error("ceph fsid", format!("JSON decode: {e}")))?;
        let fsid = parsed
            .fsid
            .filter(|fsid| !fsid.trim().is_empty())
            .ok_or_else(|| parse_error("ceph fsid", "JSON carried no fsid field"))?;
        Ok(fsid.trim().to_owned())
    } else if trimmed.is_empty() {
        Err(parse_error("ceph fsid", "no output"))
    } else {
        Ok(trimmed.to_owned())
    }
}

/// The `ceph health detail --format json` payload (permissive subset).
#[derive(Clone, Debug, Default, Deserialize)]
pub struct HealthDetail {
    /// Cluster status string (`HEALTH_OK`, `HEALTH_WARN`, `HEALTH_ERR`).
    #[serde(default)]
    pub status: Option<String>,
}

impl HealthDetail {
    /// The cluster status mapped onto the Volvisor health axis.
    ///
    /// `HEALTH_OK`→`Healthy`, `HEALTH_WARN`→`Degraded`,
    /// `HEALTH_ERR`→`Unhealthy`; an absent or unrecognized status maps to
    /// `Unknown` (never a false healthy), per the observability
    /// truthfulness invariant.
    #[must_use]
    pub fn health(&self) -> Health {
        match self.status.as_deref() {
            Some("HEALTH_OK") => Health::Healthy,
            Some("HEALTH_WARN") => Health::Degraded,
            Some("HEALTH_ERR") => Health::Unhealthy,
            _ => Health::Unknown,
        }
    }
}

/// Parse `ceph health detail --format json` output.
pub fn parse_health_detail(stdout: &str) -> Result<HealthDetail, ApiError> {
    serde_json::from_str(stdout)
        .map_err(|e| parse_error("ceph health detail", format!("JSON decode: {e}")))
}

/// The `ceph df --format json` payload (permissive subset).
#[derive(Clone, Debug, Default, Deserialize)]
pub struct CephDf {
    /// Pools known to the cluster.
    #[serde(default)]
    pub pools: Vec<CephDfPool>,
}

/// One `ceph df` pool entry.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct CephDfPool {
    /// Pool name.
    #[serde(default)]
    pub name: Option<String>,
    /// Pool statistics (fields all optional).
    #[serde(default)]
    pub stats: CephDfStats,
}

/// The per-pool statistics this provider consumes.
///
/// Real `ceph df --format json` per-pool stats carry `stored`,
/// `objects`, `kb_used`, `bytes_used`, `percent_used` and `max_avail` —
/// **no** replication `size`/`min_size` (those are policy facts owned by
/// `ceph osd pool get`, see [`parse_pool_policy_value`]); fields for
/// them are deliberately absent here so a replication fact can never be
/// hallucinated from a capacity report.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct CephDfStats {
    /// Bytes stored in the pool (raw usage basis as reported).
    #[serde(default)]
    pub bytes_used: Option<FlexibleNumber>,
    /// Bytes still allocatable in the pool.
    #[serde(default)]
    pub max_avail: Option<FlexibleNumber>,
}

impl CephDfPool {
    /// Bytes used, when reported and parseable.
    #[must_use]
    pub fn bytes_used(&self) -> Option<u64> {
        self.stats
            .bytes_used
            .as_ref()
            .and_then(FlexibleNumber::to_u64)
    }

    /// Bytes still allocatable, when reported and parseable.
    #[must_use]
    pub fn max_avail(&self) -> Option<u64> {
        self.stats
            .max_avail
            .as_ref()
            .and_then(FlexibleNumber::to_u64)
    }
}

/// Parse `ceph df --format json` output.
pub fn parse_ceph_df(stdout: &str) -> Result<CephDf, ApiError> {
    serde_json::from_str(stdout).map_err(|e| parse_error("ceph df", format!("JSON decode: {e}")))
}

/// The `rbd info --format json` payload (permissive subset).
#[derive(Clone, Debug, Default, Deserialize)]
pub struct RbdInfo {
    /// Image size in bytes.
    #[serde(default)]
    pub size: Option<FlexibleNumber>,
    /// Enabled image features (string names, numbers or absent).
    #[serde(default)]
    pub features: Option<serde_json::Value>,
}

impl RbdInfo {
    /// Size in bytes, when reported and parseable.
    #[must_use]
    pub fn size_bytes(&self) -> Option<u64> {
        self.size.as_ref().and_then(FlexibleNumber::to_u64)
    }

    /// The named image features.
    ///
    /// Only string entries are named (a numeric feature bitmask cannot be
    /// decoded without the version-specific table); numbers and a missing
    /// field yield an empty list, never an error.
    #[must_use]
    pub fn feature_names(&self) -> Vec<String> {
        self.features
            .as_ref()
            .and_then(serde_json::Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Parse `rbd info --format json` output.
pub fn parse_rbd_info(stdout: &str) -> Result<RbdInfo, ApiError> {
    serde_json::from_str(stdout).map_err(|e| parse_error("rbd info", format!("JSON decode: {e}")))
}

/// One entry of `rbd showmapped --format json` output.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct MappedDevice {
    /// Pool of the mapped image.
    #[serde(default)]
    pub pool: Option<String>,
    /// Image name of the mapped image.
    #[serde(default)]
    pub name: Option<String>,
    /// The host device path (`/dev/rbdN`).
    #[serde(default)]
    pub device: Option<String>,
}

impl MappedDevice {
    /// The `pool/image` this mapping serves, when both fields are present.
    #[must_use]
    pub fn pool_slash_image(&self) -> Option<String> {
        match (&self.pool, &self.name) {
            (Some(pool), Some(name)) => Some(format!("{pool}/{name}")),
            _ => None,
        }
    }
}

/// Parse `rbd showmapped --format json` output.
///
/// Three real output shapes are accepted, anything else is a typed
/// `INTERNAL` error (a mapping table is never silently emptied — an
/// unparseable report must fail the caller loudly, because attach
/// verification and detach both depend on it):
///
/// - the shape modern `rbd` prints: an object with a `devices` array,
///   e.g. `{"devices":[{"id":"0","pool":"rbd","name":"img","snap":"-",
///   "device":"/dev/rbd0"}]}`;
/// - a bare JSON array of device entries;
/// - the legacy shape: an object keyed by device-mapper id whose values
///   are the device entries (the key itself is not meaningful).
///
/// The legitimately empty outputs (`{}`, `{"devices":[]}` and `[]`) mean
/// "no mappings". Entry fields stay permissive (`Option` +
/// `#[serde(default)]`, unknown fields ignored), but an entry that does
/// not decode as an object at all is an error, not a skip.
pub fn parse_showmapped(stdout: &str) -> Result<Vec<MappedDevice>, ApiError> {
    let value: serde_json::Value = serde_json::from_str(stdout)
        .map_err(|e| parse_error("rbd showmapped", format!("JSON decode: {e}")))?;
    let entries: Vec<serde_json::Value> = match &value {
        serde_json::Value::Object(map) => match map.get("devices") {
            // Real shape: {"devices": [...]}.
            Some(devices) => devices.as_array().cloned().ok_or_else(|| {
                parse_error("rbd showmapped", "the devices field is not an array")
            })?,
            // Legacy shape: {"0": {...}, "3": {...}}.
            None => map.values().cloned().collect(),
        },
        // Bare array shape.
        serde_json::Value::Array(entries) => entries.clone(),
        other => {
            return Err(parse_error(
                "rbd showmapped",
                format!(
                    "expected an object with a devices array, an array, or a legacy keyed \
                     object; got {}",
                    type_name_of(other)
                ),
            ));
        }
    };
    entries
        .iter()
        .map(|entry| {
            serde_json::from_value(entry.clone())
                .map_err(|e| parse_error("rbd showmapped", format!("entry decode: {e}")))
        })
        .collect()
}

/// The JSON type name of `value` for parse-error details.
fn type_name_of(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::Object(_) => "an object",
    }
}

/// Parse one numeric field of `ceph osd pool get <pool> <key> --format json`.
///
/// Real output is a small object such as `{"size":"3"}` (the value is a
/// string in some builds, a number in others, and fields like `pool` or
/// `key` may ride along). Parsing is permissive about the value's JSON
/// type but strict about shape: anything that is not an object carrying
/// the requested key as a plain decimal is a typed `INTERNAL` error — a
/// pool policy fact is never guessed (this is the *true* replication
/// policy query; `ceph df` does not report replication at all).
pub fn parse_pool_policy_value(stdout: &str, key: &str) -> Result<u64, ApiError> {
    let value: serde_json::Value = serde_json::from_str(stdout)
        .map_err(|e| parse_error("ceph osd pool get", format!("JSON decode: {e}")))?;
    let field = value.get(key).ok_or_else(|| {
        parse_error(
            "ceph osd pool get",
            format!("the output carries no {key} field"),
        )
    })?;
    serde_json::from_value::<FlexibleNumber>(field.clone())
        .ok()
        .and_then(|number| number.to_u64())
        .ok_or_else(|| {
            parse_error(
                "ceph osd pool get",
                format!("the {key} field is not a plain decimal"),
            )
        })
}

/// Parse `rbd ls --pool <pool> --format json` output (an array of names).
///
/// Non-string entries are skipped permissively; a non-array payload is an
/// honest `INTERNAL` error.
pub fn parse_image_list(stdout: &str) -> Result<Vec<String>, ApiError> {
    let value: serde_json::Value = serde_json::from_str(stdout)
        .map_err(|e| parse_error("rbd ls", format!("JSON decode: {e}")))?;
    let Some(entries) = value.as_array() else {
        return Err(parse_error(
            "rbd ls",
            "expected a JSON array of image names",
        ));
    };
    Ok(entries
        .iter()
        .filter_map(serde_json::Value::as_str)
        .map(str::to_owned)
        .collect())
}

/// Parse `rbd trash ls --pool <pool> --format json` output.
///
/// The CLI prints an array of objects with a `name` field; plain-string
/// entries are accepted permissively as well.
pub fn parse_trash_list(stdout: &str) -> Result<Vec<String>, ApiError> {
    let value: serde_json::Value = serde_json::from_str(stdout)
        .map_err(|e| parse_error("rbd trash ls", format!("JSON decode: {e}")))?;
    let Some(entries) = value.as_array() else {
        return Err(parse_error("rbd trash ls", "expected a JSON array"));
    };
    Ok(entries
        .iter()
        .filter_map(|entry| {
            entry.as_str().map(str::to_owned).or_else(|| {
                entry
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fsid_accepts_plain_text_and_json_forms() {
        assert_eq!(
            parse_fsid("  f340f0d0-cccc-4bbb-8aaa-000000000001\n").expect("plain"),
            "f340f0d0-cccc-4bbb-8aaa-000000000001"
        );
        assert_eq!(
            parse_fsid(r#"{"fsid": "f340f0d0-cccc-4bbb-8aaa-000000000001"}"#).expect("json"),
            "f340f0d0-cccc-4bbb-8aaa-000000000001"
        );
        // Absent or empty output is an honest INTERNAL error.
        let err = parse_fsid("   ").expect_err("empty fsid");
        assert_eq!(err.code, ApiErrorCode::Internal);
        let err = parse_fsid(r#"{"other": 1}"#).expect_err("fsid field missing");
        assert_eq!(err.code, ApiErrorCode::Internal);
    }

    #[test]
    fn health_detail_maps_statuses_honestly() {
        let parse = |stdout: &str| parse_health_detail(stdout).expect("parse").health();
        assert_eq!(parse(r#"{"status": "HEALTH_OK"}"#), Health::Healthy);
        assert_eq!(parse(r#"{"status": "HEALTH_WARN"}"#), Health::Degraded);
        assert_eq!(parse(r#"{"status": "HEALTH_ERR"}"#), Health::Unhealthy);
        // Unknown and absent statuses never degrade into healthy.
        assert_eq!(parse(r#"{"status": "HEALTH_FUTURE"}"#), Health::Unknown);
        assert_eq!(parse("{}"), Health::Unknown);
    }

    #[test]
    fn ceph_df_parses_pool_stats_permissively() {
        // The real per-pool stats shape: stored/objects/kb_used/
        // bytes_used/percent_used/max_avail — no replication fields
        // (those come from `ceph osd pool get`).
        let stdout = r#"{
            "pools": [
                {"name": "rbd", "stats": {"stored": 0, "objects": 0,
                 "kb_used": 1, "bytes_used": 1024, "percent_used": 0.01,
                 "max_avail": "2048"}},
                {"name": "other", "stats": {}}
            ]
        }"#;
        let df = parse_ceph_df(stdout).expect("parse");
        assert_eq!(df.pools.len(), 2);
        let pool = &df.pools[0];
        assert_eq!(pool.name.as_deref(), Some("rbd"));
        assert_eq!(pool.bytes_used(), Some(1024));
        assert_eq!(pool.max_avail(), Some(2048));
        // Absent stats stay unknown, never zero.
        let other = &df.pools[1];
        assert_eq!(other.bytes_used(), None);
        assert_eq!(other.max_avail(), None);
    }

    #[test]
    fn rbd_info_parses_sizes_and_features() {
        let info =
            parse_rbd_info(r#"{"size": 1073741824, "features": ["exclusive-lock", "layering"]}"#)
                .expect("parse");
        assert_eq!(info.size_bytes(), Some(1 << 30));
        assert_eq!(
            info.feature_names(),
            vec!["exclusive-lock".to_owned(), "layering".to_owned()]
        );

        // Numeric feature bitmasks and string sizes are accepted; unknown
        // fields are ignored.
        let info =
            parse_rbd_info(r#"{"size": "512", "features": [5], "order": 22}"#).expect("parse");
        assert_eq!(info.size_bytes(), Some(512));
        assert_eq!(info.feature_names(), Vec::<String>::new());
    }

    #[test]
    fn showmapped_parses_the_real_devices_array_shape_verbatim() {
        // Verbatim real `rbd showmapped --format json` output.
        let stdout =
            r#"{"devices":[{"id":"0","pool":"rbd","name":"img","snap":"-","device":"/dev/rbd0"}]}"#;
        let devices = parse_showmapped(stdout).expect("parse");
        assert_eq!(devices.len(), 1);
        assert_eq!(
            devices[0].pool_slash_image().as_deref(),
            Some("rbd/img"),
            "the real shape must parse — attach verification and detach \
             depend on it"
        );
        assert_eq!(devices[0].device.as_deref(), Some("/dev/rbd0"));
    }

    #[test]
    fn showmapped_parses_the_bare_array_and_legacy_keyed_shapes() {
        // A bare JSON array of entries.
        let devices = parse_showmapped(
            r#"[{"pool": "volvisortest", "name": "vol-a-00000000",
                 "device": "/dev/rbd0", "snap": "-"}]"#,
        )
        .expect("parse");
        assert_eq!(
            devices[0].pool_slash_image().as_deref(),
            Some("volvisortest/vol-a-00000000")
        );

        // The legacy shape: an object keyed by device-mapper id.
        let stdout = r#"{
            "0": {"pool": "volvisortest", "name": "vol-a-00000000",
                  "device": "/dev/rbd0", "snap": "-", "client": "-"},
            "3": {"pool": "other", "device": "/dev/rbd3"}
        }"#;
        let devices = parse_showmapped(stdout).expect("parse");
        assert_eq!(devices.len(), 2);
        assert_eq!(
            devices[0].pool_slash_image().as_deref(),
            Some("volvisortest/vol-a-00000000")
        );
        assert_eq!(devices[0].device.as_deref(), Some("/dev/rbd0"));
        assert_eq!(devices[1].pool_slash_image(), None);
    }

    #[test]
    fn showmapped_empty_outputs_mean_no_mappings() {
        // The legitimately empty outputs: {} / {"devices":[]} / [].
        for stdout in ["{}", r#"{"devices": []}"#, "[]"] {
            assert!(
                parse_showmapped(stdout)
                    .expect("an empty mapping table parses")
                    .is_empty(),
                "{stdout} must mean no mappings"
            );
        }
    }

    #[test]
    fn showmapped_rejects_shapes_matching_no_real_form() {
        // Strict philosophy (mirroring parse_image_list): a payload that
        // matches none of the known real forms is a typed INTERNAL
        // error, never a silent empty mapping table.
        for stdout in [
            r#"{"devices": 3}"#,
            r#"{"devices": [3]}"#,
            r#"{"0": 3}"#,
            r#"["/dev/rbd0"]"#,
            "3",
            r#""text""#,
            "null",
        ] {
            let err = parse_showmapped(stdout)
                .expect_err(&format!("{stdout} must not parse as mappings"));
            assert_eq!(err.code, ApiErrorCode::Internal, "{stdout}: {err}");
        }
    }

    #[test]
    fn pool_policy_values_parse_permissively_but_never_guess() {
        // Real shapes: a bare object, a string value, a numeric value,
        // and extra riding fields.
        assert_eq!(
            parse_pool_policy_value(r#"{"size":"3"}"#, "size").expect("parse"),
            3
        );
        assert_eq!(
            parse_pool_policy_value(r#"{"size": 3}"#, "size").expect("parse"),
            3
        );
        assert_eq!(
            parse_pool_policy_value(
                r#"{"pool": "rbd", "key": "min_size", "min_size": "2"}"#,
                "min_size"
            )
            .expect("parse"),
            2
        );
        // Garbage is a typed error, never a fabricated policy fact.
        for stdout in [
            "oops",
            "{}",
            r#"{"size": "three"}"#,
            r#"{"size": true}"#,
            "3",
        ] {
            let err = parse_pool_policy_value(stdout, "size")
                .expect_err(&format!("{stdout} must not parse as a policy value"));
            assert_eq!(err.code, ApiErrorCode::Internal, "{stdout}: {err}");
            assert!(err.detail.contains("ceph osd pool get"), "{err}");
        }
    }

    #[test]
    fn image_and_trash_lists_parse_permissively() {
        assert_eq!(
            parse_image_list(r#"["vol-a-00000000", "foreign"]"#).expect("parse"),
            vec!["vol-a-00000000".to_owned(), "foreign".to_owned()]
        );
        // Non-string entries are skipped, not fatal.
        assert_eq!(
            parse_image_list(r#"[1, "vol-a-00000000", null]"#).expect("parse"),
            vec!["vol-a-00000000".to_owned()]
        );
        assert_eq!(
            parse_trash_list(r#"[{"name": "vol-b-11111111", "id": "7f2a"}]"#).expect("parse"),
            vec!["vol-b-11111111".to_owned()]
        );
        assert_eq!(
            parse_trash_list(r#"["vol-c-22222222"]"#).expect("parse"),
            vec!["vol-c-22222222".to_owned()]
        );

        let err = parse_image_list("{}").expect_err("not an array");
        assert_eq!(err.code, ApiErrorCode::Internal);
    }

    #[test]
    fn non_json_output_is_a_typed_internal_error_naming_the_command() {
        let checks: Vec<(Result<(), ApiError>, &str)> = vec![
            (
                parse_health_detail("oops").map(|_| ()),
                "ceph health detail",
            ),
            (parse_ceph_df("oops").map(|_| ()), "ceph df"),
            (parse_rbd_info("oops").map(|_| ()), "rbd info"),
            (parse_showmapped("oops").map(|_| ()), "rbd showmapped"),
            (parse_image_list("oops").map(|_| ()), "rbd ls"),
            (parse_trash_list("oops").map(|_| ()), "rbd trash ls"),
        ];
        for (result, command) in checks {
            let err = result.expect_err("non-JSON must fail loudly");
            assert_eq!(err.code, ApiErrorCode::Internal);
            assert!(err.detail.contains(command), "{err}");
        }
    }
}
