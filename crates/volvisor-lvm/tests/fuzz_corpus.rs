//! Seed-corpus regression tests for the `lvm_state` fuzz target
//! (issue #49).
//!
//! The committed corpus runs in the normal workspace suite: valid
//! seeds must load (including the N-1 and pre-versioning shapes),
//! invalid seeds must be refused, and a valid state must survive the
//! production save/load round trip.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::{Path, PathBuf};

use volvisor_lvm::state::LvmState;

fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/corpus/lvm_state")
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
    assert!(!entries.is_empty(), "lvm_state corpus is not empty");
    assert!(
        entries.iter().any(|(name, _)| name.starts_with("valid-")),
        "lvm_state corpus keeps at least one valid seed"
    );
    assert!(
        entries.iter().any(|(name, _)| name.starts_with("invalid-")),
        "lvm_state corpus keeps at least one invalid seed"
    );
    entries
}

#[test]
fn the_lvm_state_corpus_loads_valid_states_and_refuses_invalid_ones() {
    let dir = tempfile::tempdir().expect("tempdir");
    for (name, bytes) in entries() {
        let path = dir.path().join("state.json");
        std::fs::write(&path, &bytes).expect("write corpus state");

        // The target exercises both the direct decode and the production
        // load path; neither may panic.
        let _ = serde_json::from_slice::<LvmState>(&bytes);
        let loaded = LvmState::load(&path);

        if name.starts_with("valid-") {
            let mut state = loaded
                .unwrap_or_else(|error| panic!("{name}: valid seed refused: {}", error.detail));
            // The production save path canonicalizes the schema version and
            // the retired-attachment shape; the reloaded state must match.
            let saved = dir.path().join("saved.json");
            state.save(&saved).expect("valid state saves");
            let reloaded = LvmState::load(&saved).expect("saved state reloads");
            assert_eq!(state, reloaded, "{name}: save/load round trip diverged");
        } else {
            let error = loaded.expect_err("invalid seed must be refused");
            assert!(
                !error.detail.is_empty(),
                "{name}: refusal must carry a typed detail"
            );
            if name == "invalid-future-version.json" {
                assert!(
                    error.detail.contains("schema") || error.detail.contains("version"),
                    "{name}: future version must be a typed version refusal: {}",
                    error.detail
                );
            }
        }
    }
}
