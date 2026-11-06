//! LVM durable-state decoder.
//!
//! Both the direct serde decode and the production `LvmState::load`
//! path (schema probe, version refusal, custom `VolumeEntry` /
//! `VolumeRuntime` decoders) are exercised over arbitrary bytes.
#![no_main]

use libfuzzer_sys::fuzz_target;
use volvisor_lvm::state::LvmState;

fuzz_target!(|data: &[u8]| {
    // Bounded here so a pathological fuzz input cannot spend the whole
    // budget filling the temporary filesystem. Real state files are
    // small; the crash paths that matter are structural.
    if data.len() > 1 << 20 {
        return;
    }
    let _ = serde_json::from_slice::<LvmState>(data);
    let dir = tempfile::tempdir().expect("fuzz tempdir");
    let path = dir.path().join("state.json");
    std::fs::write(&path, data).expect("write fuzz state");
    let _ = LvmState::load(&path);
});
