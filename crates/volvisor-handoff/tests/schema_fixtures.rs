//! Golden historical migration-record fixtures and the schema-compatibility
//! contract (`docs/schema-compatibility.md`).
//!
//! The N-1 format predates `barrier_lineage_refusal`; the N-2 format is the
//! initial P4b record. Both files are real historical shapes, not synthesized
//! variations of the current one.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::fs;
use std::path::Path;

use volvisor_handoff::{HandoffState, MIGRATION_RECORD_SCHEMA_VERSION, MigrationStore};
use volvisor_types::{ApiErrorCode, MigrationId};

const N1_FIXTURE: &str = include_str!("../../../tests/fixtures/schema/migration-record-v1-n1.json");
const N2_FIXTURE: &str = include_str!("../../../tests/fixtures/schema/migration-record-v1-n2.json");

fn write_record(dir: &Path, name: &str, text: &str) {
    fs::write(dir.join(name), text).expect("write fixture");
}

#[test]
fn n_minus_one_fixture_loads_consumer_proof_and_cut_timestamps() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_record(dir.path(), "mig-n1.json", N1_FIXTURE);
    let store = MigrationStore::open(dir.path()).expect("N-1 store opens");
    let record = store
        .get(&MigrationId::new("mig-n1").expect("valid id"))
        .expect("record preserved");
    assert_eq!(record.schema_version, MIGRATION_RECORD_SCHEMA_VERSION);
    assert!(matches!(record.state, HandoffState::Aborted { .. }));
    assert_eq!(record.state_history.len(), 2);
    assert!(record.cut.is_none());
    assert!(record.consumer_proof.is_some(), "B2 proof preserved");
    assert_eq!(record.cut_started_at, None);
    assert_eq!(record.cut_completed_at, None);
    assert!(
        record.barrier_lineage_refusal.is_none(),
        "the pre-P6-A record has no refusal marker"
    );
    assert_eq!(record.participants.len(), 1);
    assert_eq!(record.participants[0].expected_generation, 6);
}

#[test]
fn n_minus_two_fixture_loads_initial_record_shape() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_record(dir.path(), "mig-n2.json", N2_FIXTURE);
    let store = MigrationStore::open(dir.path()).expect("N-2 store opens");
    let record = store
        .get(&MigrationId::new("mig-n2").expect("valid id"))
        .expect("record preserved");
    assert_eq!(record.schema_version, MIGRATION_RECORD_SCHEMA_VERSION);
    assert_eq!(record.state, HandoffState::Prepared);
    assert_eq!(record.state_history.len(), 1);
    assert!(record.cut.is_none());
    assert!(record.consumer_proof.is_none());
    assert_eq!(record.cut_started_at, None);
    assert_eq!(record.cut_completed_at, None);
    assert!(record.barrier_lineage_refusal.is_none());
}

#[test]
fn old_record_upgrades_on_write_and_round_trips_losslessly() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_record(dir.path(), "mig-n1.json", N1_FIXTURE);
    let mut store = MigrationStore::open(dir.path()).expect("open");
    let id = MigrationId::new("mig-n1").expect("valid id");
    let record = store.get(&id).expect("record");
    store.upsert(&record).expect("rewrite at current version");

    let text = fs::read_to_string(dir.path().join("mig-n1.json")).expect("read upgraded");
    assert!(
        text.contains("\"schema_version\": 1"),
        "upgrade-on-write stamps the current schema version: {text}"
    );
    let reopened = MigrationStore::open(dir.path()).expect("reopen");
    assert_eq!(
        reopened.get(&id).expect("record after upgrade"),
        record,
        "no durable field is lost by load/rewrite"
    );
}

#[test]
fn future_schema_version_is_refused_typed_without_rewriting() {
    let dir = tempfile::tempdir().expect("tempdir");
    let future = N1_FIXTURE.replacen(
        '{',
        "{\n  \"schema_version\": 2,\n  \"future_field\": true,",
        1,
    );
    let path = dir.path().join("mig-n1.json");
    fs::write(&path, &future).expect("write fixture");
    let err = MigrationStore::open(dir.path()).expect_err("future version must be refused");
    assert_eq!(err.code, ApiErrorCode::Internal);
    assert!(err.is_unsupported_schema_version(), "{err}");
    assert!(err.detail.contains("schema_version 2"), "{err}");
    assert_eq!(
        fs::read_to_string(&path).expect("read refused file"),
        future,
        "a refused future-version record is never rewritten"
    );
}

#[test]
fn zero_and_unknown_fields_are_refused_at_open() {
    let dir = tempfile::tempdir().expect("tempdir");

    write_record(
        dir.path(),
        "mig-zero.json",
        &N1_FIXTURE
            .replacen('{', "{\n  \"schema_version\": 0,", 1)
            .replacen("mig-n1", "mig-zero", 1),
    );
    let err = MigrationStore::open(dir.path()).expect_err("zero version");
    assert_eq!(err.code, ApiErrorCode::Internal);
    assert!(!err.is_unsupported_schema_version());
    assert!(err.detail.contains("invalid schema_version 0"), "{err}");

    fs::remove_file(dir.path().join("mig-zero.json")).expect("remove");
    write_record(
        dir.path(),
        "mig-n1.json",
        &N1_FIXTURE.replacen('{', "{\n  \"future_field\": true,", 1),
    );
    let err = MigrationStore::open(dir.path()).expect_err("top-level unknown field");
    assert_eq!(err.code, ApiErrorCode::Internal);
    assert!(!err.is_unsupported_schema_version(), "{err}");

    write_record(
        dir.path(),
        "mig-n1.json",
        &N1_FIXTURE.replacen(
            "\"expected_generation\": 6,",
            "\"expected_generation\": 6,\n      \"future_field\": true,",
            1,
        ),
    );
    let err = MigrationStore::open(dir.path()).expect_err("nested unknown field");
    assert_eq!(err.code, ApiErrorCode::Internal);
    assert!(err.detail.contains("unknown field"), "{err}");
}
