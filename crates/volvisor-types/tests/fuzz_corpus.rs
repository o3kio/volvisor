//! Seed-corpus regression tests for the `request_validation` fuzz
//! target (issue #49).
//!
//! The committed corpus runs in the normal workspace suite. A
//! `valid-` seed must parse as at least one public request envelope,
//! pass `validate()` and survive `request_hash()`; an `invalid-` seed
//! must never take that path.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::{Path, PathBuf};

use volvisor_types::VolumeId;
use volvisor_types::admin::{ClaimDeviceRequest, ReleaseDeviceRequest};
use volvisor_types::lending::{LendDeviceRequest, ReclaimDeviceRequest, ResolveQuarantineRequest};
use volvisor_types::request::{
    AdoptVolumeRequest, AttachVolumeRequest, CreateVolumeRequest, DeleteVolumeRequest,
    DetachVolumeRequest, GrowVolumeRequest, MoveVolumeBackingRequest, ReportAttachmentStateRequest,
};

fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/corpus/request_validation")
}

fn entries() -> Vec<(String, Vec<u8>)> {
    let dir = corpus_dir();
    let mut entries: Vec<(String, Vec<u8>)> = std::fs::read_dir(&dir)
        .unwrap_or_else(|error| panic!("read {} corpus: {error}", dir.display()))
        .filter_map(|entry| {
            let entry = entry.expect("corpus entry");
            if !entry.file_type().expect("corpus entry type").is_file() {
                return None;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            // `cargo fuzz` writes hash-named coverage inputs into the
            // same directory. Only the committed `valid-`/`invalid-`
            // seeds carry a naming contract; scratch inputs are triaged
            // (promoted or deleted) by the operator.
            if !(name.starts_with("valid-") || name.starts_with("invalid-")) {
                return None;
            }
            let bytes = std::fs::read(entry.path()).expect("read corpus entry");
            Some((name, bytes))
        })
        .collect();
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    assert!(
        !entries.is_empty(),
        "request_validation corpus is not empty"
    );
    assert!(
        entries.iter().any(|(name, _)| name.starts_with("valid-")),
        "request_validation corpus keeps at least one valid seed"
    );
    assert!(
        entries.iter().any(|(name, _)| name.starts_with("invalid-")),
        "request_validation corpus keeps at least one invalid seed"
    );
    entries
}

/// True when the bytes parse and validate as any public request envelope.
fn any_valid_request(data: &[u8]) -> bool {
    if let Ok(request) = serde_json::from_slice::<CreateVolumeRequest>(data) {
        if request.validate().is_ok() {
            let _ = request.request_hash();
            return true;
        }
    }
    if let Ok(request) = serde_json::from_slice::<AttachVolumeRequest>(data) {
        if request.validate().is_ok() {
            let _ = request.request_hash();
            return true;
        }
    }
    if let Ok(request) = serde_json::from_slice::<DetachVolumeRequest>(data) {
        if request.validate().is_ok() {
            let _ = request.request_hash();
            return true;
        }
    }
    if let Ok(request) = serde_json::from_slice::<ReportAttachmentStateRequest>(data) {
        if request.validate().is_ok() {
            let _ = request.request_hash();
            return true;
        }
    }
    if let Ok(request) = serde_json::from_slice::<GrowVolumeRequest>(data) {
        if request.validate(0).is_ok() {
            let _ = request.request_hash();
            return true;
        }
    }
    if let Ok(request) = serde_json::from_slice::<MoveVolumeBackingRequest>(data) {
        if request.validate_envelope().is_ok() {
            let _ = request.request_hash();
            return true;
        }
    }
    if let Ok(request) = serde_json::from_slice::<DeleteVolumeRequest>(data) {
        if request.validate().is_ok() {
            let _ = request.request_hash();
            return true;
        }
    }
    if let Ok(request) = serde_json::from_slice::<AdoptVolumeRequest>(data) {
        if request.validate().is_ok() {
            if let Ok(volume_id) = VolumeId::new("vol-fuzz-adopt") {
                let _ = request.request_hash(&volume_id);
            }
            return true;
        }
    }
    if let Ok(request) = serde_json::from_slice::<ClaimDeviceRequest>(data) {
        if request.validate().is_ok() {
            if let Ok(device_id) = volvisor_types::DeviceId::new("dev-fuzz") {
                let _ = request.request_hash(&device_id);
            }
            return true;
        }
    }
    if let Ok(request) = serde_json::from_slice::<ReleaseDeviceRequest>(data) {
        if request.validate().is_ok() {
            if let Ok(device_id) = volvisor_types::DeviceId::new("dev-fuzz") {
                let _ = request.request_hash(&device_id);
            }
            return true;
        }
    }
    if let Ok(request) = serde_json::from_slice::<LendDeviceRequest>(data) {
        if request.validate().is_ok() {
            if let Ok(device_id) = volvisor_types::DeviceId::new("dev-fuzz") {
                let _ = request.request_hash(&device_id);
            }
            return true;
        }
    }
    if let Ok(request) = serde_json::from_slice::<ReclaimDeviceRequest>(data) {
        if request.validate().is_ok() {
            if let Ok(device_id) = volvisor_types::DeviceId::new("dev-fuzz") {
                let _ = request.request_hash(&device_id);
            }
            return true;
        }
    }
    if let Ok(request) = serde_json::from_slice::<ResolveQuarantineRequest>(data) {
        if request.validate().is_ok() {
            if let Ok(device_id) = volvisor_types::DeviceId::new("dev-fuzz") {
                let _ = request.request_hash(&device_id);
            }
            return true;
        }
    }
    false
}

#[test]
fn the_request_validation_corpus_accepts_only_valid_envelopes() {
    for (name, bytes) in entries() {
        let valid = any_valid_request(&bytes);
        if name.starts_with("valid-") {
            assert!(valid, "{name}: valid seed was refused");
        } else {
            assert!(!valid, "{name}: invalid seed validated");
        }
    }
}
