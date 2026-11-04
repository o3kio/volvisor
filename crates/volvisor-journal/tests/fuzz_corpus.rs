//! Seed-corpus regression tests for the `journal_frame` and
//! `journal_record` fuzz targets (issue #49).
//!
//! The corpus under `fuzz/corpus/` is committed and runs here in the
//! normal workspace suite, so a decoder regression is caught even
//! when the nightly `cargo fuzz` workflow has not run. File names are
//! the contract: `valid-` inputs must decode, `invalid-` inputs must
//! be refused without panicking.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::{Path, PathBuf};

fn corpus_dir(target: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fuzz/corpus")
        .join(target)
}

fn entries(target: &str) -> Vec<(String, Vec<u8>)> {
    let dir = corpus_dir(target);
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
    assert!(!entries.is_empty(), "{} corpus is not empty", dir.display());
    assert!(
        entries.iter().any(|(name, _)| name.starts_with("valid-")),
        "{} corpus keeps at least one valid seed",
        dir.display()
    );
    assert!(
        entries.iter().any(|(name, _)| name.starts_with("invalid-")),
        "{} corpus keeps at least one invalid seed",
        dir.display()
    );
    entries
}

#[test]
fn the_journal_frame_corpus_has_no_panic_and_valid_chains_decode_fully() {
    for (name, bytes) in entries("journal_frame") {
        let (frames, offset) = volvisor_journal::fuzz_decode_frame_chain(&bytes);
        assert!(
            offset <= bytes.len(),
            "{name}: frame chain consumed {} of {} bytes",
            offset,
            bytes.len()
        );
        let (stream_frames, stream_offset) = volvisor_journal::fuzz_read_frame_chain(&bytes);
        assert_eq!(
            (stream_frames, stream_offset),
            (frames, offset),
            "{name}: the production streaming decoder and the frame-chain walker disagree"
        );
        if name.starts_with("valid-") {
            assert!(frames >= 1, "{name}: a valid seed decoded no frame");
            assert_eq!(offset, bytes.len(), "{name}: valid chain stopped early");
        } else if name == "invalid-sequence-gap.bin" {
            // The first frame is sound; the sequence gap stops replay at
            // the end of that frame, not at the end of the input.
            assert_eq!(frames, 1, "{name}: exactly the valid prefix decodes");
            assert!(offset < bytes.len(), "{name}: replay stopped at the gap");
        } else {
            assert_eq!(frames, 0, "{name}: an invalid seed decoded a frame");
            assert_eq!(offset, 0, "{name}: an invalid seed consumed bytes");
        }
    }
}

#[test]
fn the_journal_record_corpus_decodes_only_supported_envelopes() {
    for (name, bytes) in entries("journal_record") {
        let decoded = volvisor_journal::fuzz_decode_record_envelope(&bytes);
        if name.starts_with("valid-") {
            assert!(decoded.is_ok(), "{name}: valid seed refused: {decoded:?}");
        } else {
            assert!(decoded.is_err(), "{name}: invalid seed decoded");
            if name == "invalid-future-version.json" {
                let error = decoded.expect_err("future version is refused");
                assert!(
                    error.contains("unsupported record_version"),
                    "{name}: future version must be typed, not a parse bug: {error}"
                );
            }
        }
    }
}
