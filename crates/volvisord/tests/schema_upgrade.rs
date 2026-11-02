//! End-to-end upgrade of a populated pre-versioning state directory.
//!
//! Every covered store is loaded from its real N-1/N-2 golden fixture into
//! one temporary "data directory", asserted field-for-field, upgraded on
//! write, and re-read. This is the compatibility test the normal workspace
//! test job runs (`docs/schema-compatibility.md`); no daemon, config or
//! network is started.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::too_many_lines,
    clippy::unwrap_used
)]

use std::fs;
use std::path::{Path, PathBuf};

use volvisor_api::peer::TargetPreparationStore;
use volvisor_ceph::state::CephState;
use volvisor_drbd::state::DrbdState;
use volvisor_handoff::{HandoffState, MigrationStore};
use volvisor_journal::{IntentAppend, JOURNAL_LOG_FILE, Journal};
use volvisor_lvm::state::LvmState;
use volvisor_types::{
    ApiErrorCode, AttachmentEvidence, AttachmentState, MigrationId, MoveVolumeBackingState,
    VolumeId, VolumeLifecycle,
};

fn fixture(name: &str) -> &'static [u8] {
    match name {
        "journal-v1-n1.log" => include_bytes!("../../../tests/fixtures/schema/journal-v1-n1.log"),
        other => panic!("unknown fixture {other}"),
    }
}

fn fixture_text(name: &str) -> &'static str {
    match name {
        "lvm-state-v1-n1.json" => {
            include_str!("../../../tests/fixtures/schema/lvm-state-v1-n1.json")
        }
        "lvm-state-v1-tombstones.json" => {
            include_str!("../../../tests/fixtures/schema/lvm-state-v1-tombstones.json")
        }
        "ceph-state-v1-n1.json" => {
            include_str!("../../../tests/fixtures/schema/ceph-state-v1-n1.json")
        }
        "drbd-state-v1-n1.json" => {
            include_str!("../../../tests/fixtures/schema/drbd-state-v1-n1.json")
        }
        "migration-record-v1-n1.json" => {
            include_str!("../../../tests/fixtures/schema/migration-record-v1-n1.json")
        }
        "migration-record-v1-n2.json" => {
            include_str!("../../../tests/fixtures/schema/migration-record-v1-n2.json")
        }
        "peer-preparation-v1-n1.json" => {
            include_str!("../../../tests/fixtures/schema/peer-preparation-v1-n1.json")
        }
        other => panic!("unknown fixture {other}"),
    }
}

fn write(dir: &Path, relative: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent");
    }
    fs::write(&path, bytes).expect("write fixture");
    path
}

fn volume_id(name: &str) -> VolumeId {
    VolumeId::new(name).expect("valid volume id")
}

#[test]
fn a_populated_pre_versioning_state_directory_upgrades_in_place() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();

    // The exact store files a real deployment carries.
    let journal_dir = root.join("journal");
    write(&journal_dir, JOURNAL_LOG_FILE, fixture("journal-v1-n1.log"));
    let lvm_path = write(
        root,
        "lvm-state.json",
        fixture_text("lvm-state-v1-n1.json").as_bytes(),
    );
    let ceph_path = write(
        root,
        "ceph-state.json",
        fixture_text("ceph-state-v1-n1.json").as_bytes(),
    );
    let drbd_path = write(
        root,
        "drbd-state.json",
        fixture_text("drbd-state-v1-n1.json").as_bytes(),
    );
    let migrations_dir = root.join("migrations");
    write(
        &migrations_dir,
        "mig-n1.json",
        fixture_text("migration-record-v1-n1.json").as_bytes(),
    );
    write(
        &migrations_dir,
        "mig-n2.json",
        fixture_text("migration-record-v1-n2.json").as_bytes(),
    );
    let preparations_dir = root.join("peer-preparations");
    write(
        &preparations_dir,
        "mig-peer-n1.json",
        fixture_text("peer-preparation-v1-n1.json").as_bytes(),
    );

    // --- Journal: replay the pre-`http_status` log and prove idempotency.
    let mut journal = Journal::open(&journal_dir).expect("journal opens");
    assert_eq!(journal.record_count(), 3);
    let op = volvisor_types::OperationId::new("op-upgrade-journal-n1").expect("valid op");
    let entry = journal.lookup(&op).expect("registry entry");
    let outcome = entry.outcome.clone().expect("outcome");
    assert!(outcome.success);
    assert_eq!(outcome.http_status, None);
    assert_eq!(
        journal
            .append_intent(
                op.clone(),
                entry.request_hash,
                "create_volume",
                serde_json::json!({
                    "volume_id": "vol-upgrade-journal",
                    "size_bytes": 1_048_576
                }),
            )
            .expect("idempotent replay"),
        IntentAppend::Replayed {
            success: true,
            response: serde_json::json!({
                "volume_id": "vol-upgrade-journal",
                "generation": 1
            }),
            http_status: None,
        }
    );
    assert_eq!(journal.record_count(), 3, "replay appends nothing");

    // --- LVM: attachment state, revision, generation and the move record.
    let mut lvm = LvmState::load(&lvm_path).expect("lvm opens");
    let lvm_volume = lvm
        .volume(&volume_id("vol-upgrade-n1"))
        .expect("lvm volume preserved");
    let lvm_attachment = lvm_volume
        .runtime
        .attachment
        .as_ref()
        .expect("lvm attachment");
    assert_eq!(lvm_attachment.state, AttachmentState::Active);
    assert_eq!(lvm_attachment.state_revision, 3);
    assert_eq!(lvm_attachment.generation, 5);
    assert_eq!(lvm_attachment.vmm_disk_id.as_deref(), Some("disk-n1"));
    assert_eq!(
        lvm_attachment.evidence,
        Some(AttachmentEvidence::GuestConfirmed)
    );
    assert_eq!(
        lvm.move_record(&volume_id("vol-upgrade-n1"))
            .expect("move record")
            .state,
        MoveVolumeBackingState::InDoubt
    );
    lvm.save(&lvm_path).expect("lvm upgrade-on-write");
    assert!(
        fs::read_to_string(&lvm_path)
            .expect("read lvm")
            .contains("\"schema_version\": 1")
    );
    assert_eq!(LvmState::load(&lvm_path).expect("lvm reload"), lvm);

    // --- Tombstones: retired attachment ids survive an integrated decode.
    let tombstones_path = write(
        root,
        "lvm-tombstones.json",
        fixture_text("lvm-state-v1-tombstones.json").as_bytes(),
    );
    let tombstones = LvmState::load(&tombstones_path).expect("tombstones open");
    let tombstone = tombstones
        .volume(&volume_id("vol-upgrade-tombstone"))
        .expect("tombstone volume");
    let retired: Vec<&str> = tombstone
        .runtime
        .retired_attachment_ids
        .iter()
        .map(AsRef::as_ref)
        .collect();
    assert_eq!(retired, vec!["att-retired-oldest", "att-retired-newer"]);

    // --- Ceph: the pre-`vmm_disk_id` record keeps its mapping.
    let mut ceph = CephState::load(&ceph_path).expect("ceph opens");
    let ceph_volume = ceph
        .volume(&volume_id("vol-ceph-n1"))
        .expect("ceph volume preserved");
    assert_eq!(ceph_volume.runtime.state, VolumeLifecycle::Attached);
    let ceph_attachment = ceph_volume
        .runtime
        .attachment
        .as_ref()
        .expect("ceph attachment");
    assert_eq!(ceph_attachment.device, "/dev/rbd3");
    assert_eq!(ceph_attachment.vmm_disk_id, None);
    ceph.save(&ceph_path).expect("ceph upgrade-on-write");
    assert!(
        fs::read_to_string(&ceph_path)
            .expect("read ceph")
            .contains("\"schema_version\": 1")
    );
    assert_eq!(CephState::load(&ceph_path).expect("ceph reload"), ceph);

    // --- DRBD: attachment, authority block and migration-cut marker.
    let mut drbd = DrbdState::load(&drbd_path).expect("drbd opens");
    let drbd_volume = drbd
        .volume(&volume_id("vol-drbd-n1"))
        .expect("drbd volume preserved");
    let drbd_attachment = drbd_volume
        .runtime
        .attachment
        .as_ref()
        .expect("drbd attachment");
    assert_eq!(drbd_attachment.generation, 3);
    assert_eq!(drbd_attachment.vmm_disk_id, None);
    let authority = drbd_volume.runtime.authority.as_ref().expect("authority");
    assert_eq!(authority.epoch.0, 11);
    assert_eq!(
        drbd_volume
            .runtime
            .migration
            .as_ref()
            .expect("cut marker")
            .migration_id
            .as_ref(),
        "mig-drbd-n1"
    );
    drbd.save(&drbd_path).expect("drbd upgrade-on-write");
    assert!(
        fs::read_to_string(&drbd_path)
            .expect("read drbd")
            .contains("\"schema_version\": 1")
    );
    assert_eq!(DrbdState::load(&drbd_path).expect("drbd reload"), drbd);

    // --- Migration records: N-1 and N-2 both load; writing upgrades them.
    let mut store = MigrationStore::open(&migrations_dir).expect("migration store opens");
    let n1 = MigrationId::new("mig-n1").expect("valid id");
    let n2 = MigrationId::new("mig-n2").expect("valid id");
    let n1_record = store.get(&n1).expect("N-1 record");
    assert!(matches!(n1_record.state, HandoffState::Aborted { .. }));
    assert!(n1_record.consumer_proof.is_some());
    assert_eq!(n1_record.cut_started_at, None);
    let n2_record = store.get(&n2).expect("N-2 record");
    assert_eq!(n2_record.state, HandoffState::Prepared);
    assert_eq!(n2_record.state_history.len(), 1);
    store.upsert(&n1_record).expect("upgrade N-1 record");
    store.upsert(&n2_record).expect("upgrade N-2 record");
    assert!(
        fs::read_to_string(migrations_dir.join("mig-n1.json"))
            .expect("read migration")
            .contains("\"schema_version\": 1")
    );
    let reopened = MigrationStore::open(&migrations_dir).expect("migration reopen");
    assert_eq!(reopened.get(&n1), Some(n1_record));
    assert_eq!(reopened.get(&n2), Some(n2_record));

    // --- Peer preparations: the pre-`grant_attempt` record decodes with
    // the documented default and upgrades on a real durable write.
    let preparations = TargetPreparationStore::open(&preparations_dir).expect("preparations open");
    let peer_id = MigrationId::new("mig-peer-n1").expect("valid id");
    let preparation = preparations
        .load(&peer_id)
        .expect("load preparation")
        .expect("preparation preserved");
    assert_eq!(preparation.grant_attempt, 0, "absent counter defaults to 0");
    assert_eq!(preparation.participants.len(), 1);
    assert_eq!(
        preparation.participants[0].device_path.as_deref(),
        Some("/dev/drbd9")
    );
    assert_eq!(
        preparations
            .begin_grant_attempt(&peer_id)
            .expect("upgrade-on-write"),
        1
    );
    assert!(
        fs::read_to_string(preparations_dir.join("mig-peer-n1.json"))
            .expect("read preparation")
            .contains("\"schema_version\": 1")
    );
    let reopened = TargetPreparationStore::open(&preparations_dir).expect("preparations reopen");
    assert_eq!(
        reopened
            .load(&peer_id)
            .expect("load after upgrade")
            .expect("preparation after upgrade")
            .grant_attempt,
        1
    );
}

#[test]
fn each_covered_store_refuses_a_newer_schema_on_open() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();

    // A newer file carries an unknown field as well as the version: the
    // version refusal must win (partial decode is forbidden).
    let future_lvm = fixture_text("lvm-state-v1-n1.json").replacen(
        '{',
        "{\n  \"schema_version\": 2,\n  \"future_field\": true,",
        1,
    );
    let path = write(root, "lvm-state.json", future_lvm.as_bytes());
    let err = LvmState::load(&path).expect_err("future lvm");
    assert_eq!(err.code, ApiErrorCode::Internal);
    assert!(err.is_unsupported_schema_version(), "{err}");
    assert_eq!(fs::read_to_string(&path).expect("unchanged"), future_lvm);

    let future_ceph = fixture_text("ceph-state-v1-n1.json").replacen(
        '{',
        "{\n  \"schema_version\": 2,\n  \"future_field\": true,",
        1,
    );
    let path = write(root, "ceph-state.json", future_ceph.as_bytes());
    let err = CephState::load(&path).expect_err("future ceph");
    assert!(err.is_unsupported_schema_version(), "{err}");

    let future_drbd = fixture_text("drbd-state-v1-n1.json").replacen(
        '{',
        "{\n  \"schema_version\": 2,\n  \"future_field\": true,",
        1,
    );
    let path = write(root, "drbd-state.json", future_drbd.as_bytes());
    let err = DrbdState::load(&path).expect_err("future drbd");
    assert!(err.is_unsupported_schema_version(), "{err}");

    let migrations = root.join("migrations");
    write(
        &migrations,
        "mig-n1.json",
        fixture_text("migration-record-v1-n1.json")
            .replacen(
                '{',
                "{\n  \"schema_version\": 2,\n  \"future_field\": true,",
                1,
            )
            .as_bytes(),
    );
    let err = MigrationStore::open(&migrations).expect_err("future migration");
    assert!(err.is_unsupported_schema_version(), "{err}");

    let preparations = root.join("peer-preparations");
    write(
        &preparations,
        "mig-peer-n1.json",
        fixture_text("peer-preparation-v1-n1.json")
            .replacen(
                '{',
                "{\n  \"schema_version\": 2,\n  \"future_field\": true,",
                1,
            )
            .as_bytes(),
    );
    let err = TargetPreparationStore::open(&preparations)
        .err()
        .expect("future preparation");
    assert!(err.is_unsupported_schema_version(), "{err}");
}
