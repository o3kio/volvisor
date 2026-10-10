//! The Tier R scenarios (P5 plan §0/§6, §9's Tier R paragraph,
//! stage D): one test per scenario, driving the scaffold through
//! [`volvisor_campaign::tier_r::drive`].
//!
//! In the default environment (no `VOLVISOR_CAMPAIGN_TIER`) every
//! test passes WITH its explicit `skipped` evidence record — the
//! coverage matrix shows the gate, never a hole (§0: "not run, no
//! hardware" is distinct from "not implemented"). Setting
//! `VOLVISOR_CAMPAIGN_TIER=R` claims real-host execution: these
//! tests then run the fail-closed toolchain detection and FAIL
//! LOUDLY (a claimed tier the environment does not back — or the
//! scaffold cannot yet honor — is a red test, never a silent pass;
//! see `tier_r`'s unit tests for both claimed arms).
//!
//! The scaffold's drive logic itself (both loud-failure arms) is
//! unit-tested in the crate; these integration tests exercise the
//! default path end-to-end against the REAL run directory: each
//! skip record lands under `target/campaign-evidence/<run>/` and
//! the per-run `REPORT.md` refresh includes it.

// Test target (the e2e precedent): invariant assertions may
// expect/unwrap; the `clippy::panic` allow mirrors the lib's
// documented discipline (its panics ARE the assertions).
#![allow(clippy::expect_used, clippy::unwrap_used)]
#![allow(clippy::panic)]

use serde_json::Value;

use volvisor_campaign::tier_r;

/// Assert one scenario's skip record: the §6 shape, verbatim in
/// intent — `tier R`, `outcome skipped`, the reason and the
/// documented body (the portability contract §0 demands).
fn assert_skip_record(path: &std::path::Path, scenario: &tier_r::Scenario) -> Value {
    let body = std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("the skip record for {} landed: {error}", scenario.name));
    let record: Value = serde_json::from_str(&body)
        .unwrap_or_else(|error| panic!("the skip record for {} is JSON: {error}", scenario.name));
    assert_eq!(
        record["tier"], "R",
        "the {} record states its tier",
        scenario.name
    );
    assert_eq!(
        record["outcome"], "skipped",
        "the {} record is an explicit skip",
        scenario.name
    );
    assert_eq!(
        record["reason"], scenario.skip_reason,
        "the {} record carries its reason",
        scenario.name
    );
    assert_eq!(
        record["would_run"], scenario.body,
        "the {} record documents what it would run",
        scenario.name
    );
    assert!(
        record["components"].is_null(),
        "the {} record's components field is null: a skip is a gate statement, not an \
         observation of the environment (claiming component versions for a scenario \
         that did not run would be fabrication, §0)",
        scenario.name
    );
    record
}

#[test]
fn tier_r_same_families_on_real_hosts() {
    let scenario = &tier_r::SCENARIOS[0];
    let path = tier_r::drive(scenario);
    assert_skip_record(&path, scenario);
}

#[test]
fn tier_r_power_cut() {
    let scenario = &tier_r::SCENARIOS[1];
    let path = tier_r::drive(scenario);
    assert_skip_record(&path, scenario);
}

#[test]
fn tier_r_ssd_loss() {
    let scenario = &tier_r::SCENARIOS[2];
    let path = tier_r::drive(scenario);
    assert_skip_record(&path, scenario);
}

#[test]
fn tier_r_media_flush_fua() {
    let scenario = &tier_r::SCENARIOS[3];
    let path = tier_r::drive(scenario);
    assert_skip_record(&path, scenario);
}

#[test]
fn tier_r_saturation_multitb() {
    let scenario = &tier_r::SCENARIOS[4];
    let path = tier_r::drive(scenario);
    assert_skip_record(&path, scenario);
}

#[test]
fn tier_r_mirror_leg_loss() {
    let scenario = &tier_r::SCENARIOS[5];
    let record = {
        let path = tier_r::drive(scenario);
        assert_skip_record(&path, scenario)
    };
    // The mirror gate is double-gated: hardware alone does not open
    // it — no mirror implementation exists to fault (§1).
    assert!(
        record["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("no local mirror implementation exists")),
        "the mirror-leg-loss reason names the implementation gate, not just the \
         hardware gate: {}",
        record["reason"]
    );
}

#[test]
fn tier_r_durable_dirty_bitmap_meta() {
    let scenario = &tier_r::SCENARIOS[6];
    let record = {
        let path = tier_r::drive(scenario);
        assert_skip_record(&path, scenario)
    };
    // The bitmap boundary's reason names the durable boundary §1
    // records (Tier S covers the in-flight resync window only).
    assert!(
        record["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("persists no bitmap")),
        "the bitmap-boundary reason names the persisted-bitmap gap: {}",
        record["reason"]
    );
}

#[test]
fn tier_r_kill_timing_races() {
    let scenario = &tier_r::SCENARIOS[7];
    let path = tier_r::drive(scenario);
    assert_skip_record(&path, scenario);
}

/// The scenario table is the renderer's Tier R half (the coverage
/// matrix's gate rows come from it): every entry is unique, names
/// its class, and carries both the reason and the documented body —
/// a scenario without its body would be a hole wearing a gate's
/// clothes (§0's portability demand).
#[test]
fn the_scenario_table_is_well_formed() {
    let mut names: Vec<&str> = tier_r::SCENARIOS
        .iter()
        .map(|scenario| scenario.name)
        .collect();
    let count = names.len();
    names.sort_unstable();
    names.dedup();
    assert_eq!(count, names.len(), "a Tier R scenario name repeats");
    assert_eq!(count, 8, "§9's Tier R paragraph enumerates eight gates");
    for scenario in tier_r::SCENARIOS {
        assert!(!scenario.class.is_empty(), "the class is named");
        assert!(!scenario.skip_reason.is_empty(), "the reason is named");
        assert!(
            scenario.body.contains("prove") || scenario.body.contains("re-drive"),
            "the body documents what it would run: {}",
            scenario.body
        );
    }
}
