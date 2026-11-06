//! Validates the committed operational trace evidence (issue #44).
//!
//! Every committed `docs/operations/traces/*.jsonl` file is parsed as one JSON
//! object per non-empty line. The test checks the JSON trace shape, RFC 3339
//! timestamps, bounded span names, operation-id correlation and the absence of
//! the test credential or secret-like fields. It is an ordinary test so the
//! committed evidence cannot silently rot; it does not generate a trace and it
//! does not require `migration.jsonl` to exist.
//!
//! Run explicitly with:
//!
//! ```text
//! cargo test -p volvisor-api --test trace_evidence --locked
//! ```

// Integration-test code: invariant assertions may use expect/unwrap.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Display;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

const TEST_ADMIN_TOKEN: &str = "trace-admin-token";
const KNOWN_SPAN_NAMES: &[&str] = &[
    "operation",
    "provider_call",
    "peer_call",
    "witness_inspect",
    "vmm_state",
    "vmm_call",
    "peer_grant",
];
/// Span names whose trace contract promises a non-empty `operation_id`.
const OPERATION_ID_SPANS: &[&str] = &["operation", "provider_call"];
const SECRET_KEY_MARKERS: &[&str] = &["authorization", "token", "secret", "password"];

type Check<T = ()> = Result<T, String>;

#[derive(Debug, Default)]
struct TraceFacts {
    operation_ids: BTreeMap<String, BTreeSet<String>>,
    audit_events: Vec<AuditEvent>,
}

#[derive(Debug)]
struct AuditEvent {
    route: String,
    operation_id: String,
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("repository root")
        .to_path_buf()
}

fn trace_files() -> Vec<PathBuf> {
    let directory = repo_root().join("docs/operations/traces");
    let entries = fs::read_dir(&directory).expect("trace directory");
    let mut files: Vec<PathBuf> = entries
        .map(|entry| entry.expect("trace directory entry").path())
        .filter(|path| path.extension().and_then(|extension| extension.to_str()) == Some("jsonl"))
        .collect();
    files.sort();
    assert!(
        !files.is_empty(),
        "{}: no committed .jsonl trace files",
        directory.display()
    );
    assert!(
        files
            .iter()
            .any(|path| path.file_name().and_then(|name| name.to_str()) == Some("attach.jsonl")),
        "{}: attach.jsonl is missing",
        directory.display()
    );
    files
}

fn invalid(path: &Path, line: usize, message: impl Display) -> String {
    format!("{}:{line}: {message}", path.display())
}

fn validate_file(path: &Path) -> Check<TraceFacts> {
    let text = fs::read_to_string(path).expect("trace file");
    let mut facts = TraceFacts::default();
    let mut events = 0;
    for (offset, raw) in text.lines().enumerate() {
        if raw.trim().is_empty() {
            continue;
        }
        events += 1;
        validate_line(path, offset + 1, raw, &mut facts)?;
    }
    if events == 0 {
        return Err(invalid(path, 1, "trace file has no events"));
    }
    Ok(facts)
}

fn validate_line(path: &Path, line: usize, raw: &str, facts: &mut TraceFacts) -> Check {
    if raw.contains(TEST_ADMIN_TOKEN) {
        return Err(invalid(path, line, "contains the test admin token value"));
    }

    let event: Value = serde_json::from_str(raw)
        .map_err(|error| invalid(path, line, format!("not one JSON object: {error}")))?;
    let object = event
        .as_object()
        .ok_or_else(|| invalid(path, line, "JSON value is not an object"))?;
    check_no_secret_fields(path, line, &event)?;

    let timestamp = object
        .get("timestamp")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(path, line, "missing string field timestamp"))?;
    if !is_rfc3339(timestamp) {
        return Err(invalid(
            path,
            line,
            format!("timestamp is not RFC 3339: {timestamp}"),
        ));
    }

    match object.get("level").and_then(Value::as_str) {
        Some(level) if !level.is_empty() => {}
        _ => return Err(invalid(path, line, "missing non-empty string field level")),
    }

    let fields = object
        .get("fields")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid(path, line, "missing object field fields"))?;
    match fields.get("message").and_then(Value::as_str) {
        Some(message) if !message.is_empty() => {}
        _ => {
            return Err(invalid(
                path,
                line,
                "missing non-empty string field fields.message",
            ));
        }
    }

    let mut spans = Vec::new();
    if let Some(span) = object.get("span") {
        spans.push(
            span.as_object()
                .ok_or_else(|| invalid(path, line, "span is not an object"))?,
        );
    }
    if let Some(entries) = object.get("spans") {
        let entries = entries
            .as_array()
            .ok_or_else(|| invalid(path, line, "spans is not an array"))?;
        for span in entries {
            spans.push(
                span.as_object()
                    .ok_or_else(|| invalid(path, line, "spans[] entry is not an object"))?,
            );
        }
    }

    for span in &spans {
        validate_span(path, line, span, facts)?;
    }
    validate_provider_nesting(path, line, &spans)?;
    collect_audit(path, line, fields, facts)?;

    Ok(())
}

fn validate_span(
    path: &Path,
    line: usize,
    span: &Map<String, Value>,
    facts: &mut TraceFacts,
) -> Check {
    let name = span
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| invalid(path, line, "span is missing a non-empty string name"))?;
    if !KNOWN_SPAN_NAMES.contains(&name) {
        return Err(invalid(path, line, format!("unknown span name: {name}")));
    }

    if OPERATION_ID_SPANS.contains(&name) {
        let operation_id = span
            .get("operation_id")
            .and_then(Value::as_str)
            .filter(|operation_id| !operation_id.trim().is_empty())
            .ok_or_else(|| {
                invalid(
                    path,
                    line,
                    format!("{name} span is missing a non-empty operation_id"),
                )
            })?;
        if name == "operation" {
            let kind = span
                .get("kind")
                .and_then(Value::as_str)
                .filter(|kind| !kind.is_empty())
                .ok_or_else(|| invalid(path, line, "operation span is missing a kind"))?;
            facts
                .operation_ids
                .entry(kind.to_owned())
                .or_default()
                .insert(operation_id.to_owned());
        }
    }
    Ok(())
}

fn validate_provider_nesting(path: &Path, line: usize, spans: &[&Map<String, Value>]) -> Check {
    let Some(provider_span) = spans
        .iter()
        .find(|span| span.get("name").and_then(Value::as_str) == Some("provider_call"))
    else {
        return Ok(());
    };
    let provider_id = provider_span
        .get("operation_id")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(path, line, "provider_call is missing operation_id"))?;
    let operation_span = spans
        .iter()
        .find(|span| span.get("name").and_then(Value::as_str) == Some("operation"))
        .ok_or_else(|| {
            invalid(
                path,
                line,
                "provider_call event does not include the enclosing operation in spans",
            )
        })?;
    let operation_id = operation_span
        .get("operation_id")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(path, line, "enclosing operation is missing operation_id"))?;
    if provider_id != operation_id {
        return Err(invalid(
            path,
            line,
            format!(
                "provider_call operation_id {provider_id} does not match enclosing operation {operation_id}"
            ),
        ));
    }
    Ok(())
}

fn collect_audit(
    path: &Path,
    line: usize,
    fields: &Map<String, Value>,
    facts: &mut TraceFacts,
) -> Check {
    if fields.get("kind").and_then(Value::as_str) != Some("audit") {
        return Ok(());
    }
    let route = fields
        .get("route")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(path, line, "audit event is missing route"))?;
    let operation_id = fields
        .get("operation_id")
        .and_then(Value::as_str)
        .filter(|operation_id| !operation_id.trim().is_empty())
        .ok_or_else(|| {
            invalid(
                path,
                line,
                "audit event is missing a non-empty operation_id",
            )
        })?;
    facts.audit_events.push(AuditEvent {
        route: route.to_owned(),
        operation_id: operation_id.to_owned(),
    });
    Ok(())
}

fn check_no_secret_fields(path: &Path, line: usize, value: &Value) -> Check {
    match value {
        Value::Object(object) => {
            for (key, child) in object {
                let lowered = key.to_ascii_lowercase();
                if SECRET_KEY_MARKERS
                    .iter()
                    .any(|marker| lowered.contains(marker))
                {
                    return Err(invalid(
                        path,
                        line,
                        format!("secret-like field present: {key}"),
                    ));
                }
                check_no_secret_fields(path, line, child)?;
            }
        }
        Value::Array(values) => {
            for child in values {
                check_no_secret_fields(path, line, child)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn validate_attach_correlation(facts: &TraceFacts) -> Check {
    let create_ids = facts
        .operation_ids
        .get("create_volume")
        .filter(|ids| !ids.is_empty())
        .ok_or_else(|| "attach.jsonl: no create_volume operation span".to_owned())?;
    let attach_ids = facts
        .operation_ids
        .get("attach_volume")
        .filter(|ids| !ids.is_empty())
        .ok_or_else(|| "attach.jsonl: no attach_volume operation span".to_owned())?;

    let has_create_audit = facts
        .audit_events
        .iter()
        .any(|event| event.route == "/v2/volumes" && create_ids.contains(&event.operation_id));
    if !has_create_audit {
        return Err(
            "attach.jsonl: no create audit event on /v2/volumes carries a create_volume operation id"
                .to_owned(),
        );
    }
    let has_attach_audit = facts.audit_events.iter().any(|event| {
        event.route == "/v2/volumes/{volume_id}/attach" && attach_ids.contains(&event.operation_id)
    });
    if !has_attach_audit {
        return Err(
            "attach.jsonl: no attach audit event on /v2/volumes/{volume_id}/attach carries an attach_volume operation id"
                .to_owned(),
        );
    }
    Ok(())
}

/// RFC 3339 timestamp parser: full-date `T` full-time (`Z` or numeric offset).
/// Fractional seconds and leap seconds are accepted; other shapes are not.
fn is_rfc3339(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() < 20 {
        return false;
    }
    if !bytes[..4].iter().all(u8::is_ascii_digit)
        || !bytes[5..7].iter().all(u8::is_ascii_digit)
        || !bytes[8..10].iter().all(u8::is_ascii_digit)
        || !bytes[11..13].iter().all(u8::is_ascii_digit)
        || !bytes[14..16].iter().all(u8::is_ascii_digit)
        || !bytes[17..19].iter().all(u8::is_ascii_digit)
    {
        return false;
    }
    if bytes[4] != b'-'
        || bytes[7] != b'-'
        || !matches!(bytes[10], b'T' | b't')
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return false;
    }

    let year = four_digit(bytes, 0).expect("checked digits");
    let month = two_digit(bytes, 5).expect("checked digits");
    let day = two_digit(bytes, 8).expect("checked digits");
    let hour = two_digit(bytes, 11).expect("checked digits");
    let minute = two_digit(bytes, 14).expect("checked digits");
    let second = two_digit(bytes, 17).expect("checked digits");

    // Python's datetime has no year 0, so the Python twin rejects it. Keep the
    // two evidence gates behaviorally identical rather than disagreeing on a
    // boundary no current trace reaches.
    if year == 0
        || !(1..=12).contains(&month)
        || day == 0
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return false;
    }

    let mut index = 19;
    if bytes[index] == b'.' {
        index += 1;
        let start = index;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        if index == start {
            return false;
        }
    }
    if index >= bytes.len() {
        return false;
    }

    match bytes[index] {
        b'Z' | b'z' => index + 1 == bytes.len(),
        b'+' | b'-' => {
            if index + 6 != bytes.len() || bytes[index + 3] != b':' {
                return false;
            }
            let Some(offset_hour) = two_digit(bytes, index + 1) else {
                return false;
            };
            let Some(offset_minute) = two_digit(bytes, index + 4) else {
                return false;
            };
            offset_hour <= 23 && offset_minute <= 59
        }
        _ => false,
    }
}

fn two_digit(bytes: &[u8], start: usize) -> Option<u32> {
    let tens = *bytes.get(start)?;
    let ones = *bytes.get(start + 1)?;
    if !tens.is_ascii_digit() || !ones.is_ascii_digit() {
        return None;
    }
    Some(u32::from(tens - b'0') * 10 + u32::from(ones - b'0'))
}

fn four_digit(bytes: &[u8], start: usize) -> Option<u32> {
    let mut value = 0;
    for offset in 0..4 {
        let digit = *bytes.get(start + offset)?;
        if !digit.is_ascii_digit() {
            return None;
        }
        value = value * 10 + u32::from(digit - b'0');
    }
    Some(value)
}

fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

fn is_leap_year(year: u32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

#[test]
fn committed_traces_are_structural_and_correlated() {
    for path in trace_files() {
        let facts = validate_file(&path).expect("trace validation");
        if path.file_name().and_then(|name| name.to_str()) == Some("attach.jsonl") {
            validate_attach_correlation(&facts).expect("attach trace correlation");
        }
    }
}

#[test]
fn rfc3339_parser_accepts_and_rejects_expected_shapes() {
    assert!(is_rfc3339("2026-11-03T11:57:59.900762Z"));
    assert!(is_rfc3339("2026-11-03T11:57:59Z"));
    assert!(is_rfc3339("2026-11-03t11:57:59+02:30"));
    assert!(is_rfc3339("2024-02-29T23:59:60-00:00"));
    assert!(!is_rfc3339("2026-11-03 11:57:59Z"));
    assert!(!is_rfc3339("2025-02-29T11:57:59Z"));
    assert!(!is_rfc3339("2026-11-03T24:00:00Z"));
    assert!(!is_rfc3339("2026-11-03T11:57:59"));
    assert!(!is_rfc3339("2026-11-03T11:57:59.Z"));
    assert!(!is_rfc3339("2026-11-03T11:57:59+2:00"));
    assert!(!is_rfc3339("0000-01-01T00:00:00Z"));
}

fn committed_events(name: &str) -> Vec<Value> {
    let path = repo_root().join("docs/operations/traces").join(name);
    fs::read_to_string(&path)
        .expect("committed trace")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("committed trace JSON"))
        .collect()
}

fn write_temp_trace(name: &str, events: &[Value]) -> PathBuf {
    let directory = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("trace-evidence");
    fs::create_dir_all(&directory).expect("trace temp directory");
    let path = directory.join(name);
    let mut text = String::new();
    for event in events {
        text.push_str(&serde_json::to_string(event).expect("serialize trace event"));
        text.push('\n');
    }
    fs::write(&path, text).expect("write temp trace");
    path
}

fn operation_event(name: &str, operation_id: &str) -> Value {
    json!({
        "timestamp": "2026-11-04T14:25:37Z",
        "level": "INFO",
        "fields": {"message": "operation completed", "kind": name},
        "span": {"name": "operation", "kind": name, "operation_id": operation_id},
    })
}

#[test]
fn negative_trace_shapes_are_rejected() {
    // The audit events are what prove the operation ids reached the audit
    // route; a structurally valid file without them must fail correlation.
    let without_audit: Vec<Value> = committed_events("attach.jsonl")
        .into_iter()
        .filter(|event| event["fields"]["kind"] != "audit")
        .collect();
    let path = write_temp_trace("attach-without-audit.jsonl", &without_audit);
    let facts = validate_file(&path).expect("span-only lines validate individually");
    assert!(validate_attach_correlation(&facts).is_err());

    // A secret-like field is rejected even when the rest of the line is valid.
    let mut leaked = operation_event("create_volume", "op-leak");
    leaked["fields"]["api_token"] = json!("redacted");
    let path = write_temp_trace("secret.jsonl", &[leaked]);
    let error = validate_file(&path).expect_err("secret-like field must be rejected");
    assert!(error.contains("secret-like field"), "{error}");

    // A provider_call must name the same operation as its enclosing span.
    let mut mismatched = operation_event("create_volume", "op-enclosing");
    mismatched["spans"] = json!([
        {"name": "operation", "kind": "create_volume", "operation_id": "op-enclosing"},
        {"name": "provider_call", "kind": "create_volume", "operation_id": "op-other"}
    ]);
    let path = write_temp_trace("provider-mismatch.jsonl", &[mismatched]);
    let error = validate_file(&path).expect_err("mismatched provider_call must be rejected");
    assert!(
        error.contains("does not match enclosing operation"),
        "{error}"
    );

    // An audit event with an empty operation id is malformed, not skippable.
    let mut empty_id = operation_event("create_volume", "op-1");
    empty_id["fields"] = json!({
        "message": "request", "kind": "audit", "route": "/v2/volumes",
        "operation_id": ""
    });
    let path = write_temp_trace("empty-audit-id.jsonl", &[empty_id]);
    let error = validate_file(&path).expect_err("empty audit operation_id must be rejected");
    assert!(error.contains("non-empty operation_id"), "{error}");
}
