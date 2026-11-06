//! Journal record envelope decoder.
//!
//! Exercises the version probe and strict `Envelope` decode replay
//! uses: unknown fields and unsupported versions are refused, while a
//! complete supported envelope decodes. Only panic/overflow is a bug.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = volvisor_journal::fuzz_decode_record_envelope(data);
});
