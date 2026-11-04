//! Journal frame chain / torn-tail parser.
//!
//! Feeds arbitrary bytes to both frame decoders replay relies on: the
//! `decode_at` chain walker and the production streaming `read_frame`
//! path. The invariant is structural: decode either stops at a
//! torn/corrupt boundary or returns a frame wholly inside the input, and
//! it never panics, over-allocates or loops.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let (frames, offset) = volvisor_journal::fuzz_decode_frame_chain(data);
    assert!(offset <= data.len(), "frame chain consumed past the input");
    // A valid frame chain always advances; a torn tail stops at the
    // last valid boundary (possibly zero).
    if frames == 0 {
        assert_eq!(offset, 0, "no frame decoded but bytes were consumed");
    }

    let (stream_frames, stream_offset) = volvisor_journal::fuzz_read_frame_chain(data);
    assert!(
        stream_offset <= data.len(),
        "streaming frame chain consumed past the input"
    );
    if stream_frames == 0 {
        assert_eq!(
            stream_offset, 0,
            "no streamed frame decoded but bytes were consumed"
        );
    }
    assert_eq!(
        (stream_frames, stream_offset),
        (frames, offset),
        "the production streaming decoder and the frame-chain walker disagree"
    );
});
