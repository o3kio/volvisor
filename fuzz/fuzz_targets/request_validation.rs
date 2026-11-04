//! Request deserialization + validation paths.
//!
//! Arbitrary bytes are parsed as the public request envelopes. A
//! successful parse must survive `validate()` (typed refusal is fine —
//! a panic, overflow or unbounded allocation is not).
#![no_main]

use libfuzzer_sys::fuzz_target;
use volvisor_types::VolumeId;
use volvisor_types::admin::{ClaimDeviceRequest, ReleaseDeviceRequest};
use volvisor_types::lending::{LendDeviceRequest, ReclaimDeviceRequest, ResolveQuarantineRequest};
use volvisor_types::request::{
    AdoptVolumeRequest, AttachVolumeRequest, CreateVolumeRequest, DeleteVolumeRequest,
    DetachVolumeRequest, GrowVolumeRequest, MoveVolumeBackingRequest, ReportAttachmentStateRequest,
};

fuzz_target!(|data: &[u8]| {
    if let Ok(request) = serde_json::from_slice::<CreateVolumeRequest>(data) {
        let _ = request.validate();
        let _ = request.request_hash();
    }
    if let Ok(request) = serde_json::from_slice::<AttachVolumeRequest>(data) {
        let _ = request.validate();
        let _ = request.request_hash();
    }
    if let Ok(request) = serde_json::from_slice::<DetachVolumeRequest>(data) {
        let _ = request.validate();
        let _ = request.request_hash();
    }
    if let Ok(request) = serde_json::from_slice::<ReportAttachmentStateRequest>(data) {
        let _ = request.validate();
        let _ = request.request_hash();
    }
    if let Ok(request) = serde_json::from_slice::<GrowVolumeRequest>(data) {
        // The envelope check runs before the journal; the state-dependent
        // variant additionally needs a current size.
        let _ = request.validate_envelope();
        let _ = request.validate(0);
        let _ = request.request_hash();
    }
    if let Ok(request) = serde_json::from_slice::<MoveVolumeBackingRequest>(data) {
        let _ = request.validate_envelope();
        let _ = request.request_hash();
    }
    if let Ok(request) = serde_json::from_slice::<DeleteVolumeRequest>(data) {
        let _ = request.validate();
        let _ = request.request_hash();
    }
    if let Ok(request) = serde_json::from_slice::<AdoptVolumeRequest>(data) {
        let _ = request.validate();
        if let Ok(volume_id) = VolumeId::new("vol-fuzz-adopt") {
            let _ = request.request_hash(&volume_id);
        }
    }
    if let Ok(request) = serde_json::from_slice::<ClaimDeviceRequest>(data) {
        let _ = request.validate();
        if let Ok(device_id) = volvisor_types::DeviceId::new("dev-fuzz") {
            let _ = request.request_hash(&device_id);
        }
    }
    if let Ok(request) = serde_json::from_slice::<ReleaseDeviceRequest>(data) {
        let _ = request.validate();
        if let Ok(device_id) = volvisor_types::DeviceId::new("dev-fuzz") {
            let _ = request.request_hash(&device_id);
        }
    }
    if let Ok(request) = serde_json::from_slice::<LendDeviceRequest>(data) {
        let _ = request.validate();
        if let Ok(device_id) = volvisor_types::DeviceId::new("dev-fuzz") {
            let _ = request.request_hash(&device_id);
        }
    }
    if let Ok(request) = serde_json::from_slice::<ReclaimDeviceRequest>(data) {
        let _ = request.validate();
        if let Ok(device_id) = volvisor_types::DeviceId::new("dev-fuzz") {
            let _ = request.request_hash(&device_id);
        }
    }
    if let Ok(request) = serde_json::from_slice::<ResolveQuarantineRequest>(data) {
        let _ = request.validate();
        if let Ok(device_id) = volvisor_types::DeviceId::new("dev-fuzz") {
            let _ = request.request_hash(&device_id);
        }
    }
});
