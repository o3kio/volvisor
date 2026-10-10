//! Row 15 (§9, stage D): the evidence bundle + summary render —
//! "the coverage matrix exists, is budget-adherent, and the
//! completion gates pass over it" (the plan's original "is
//! truthful" was a self-referential overclaim, softened in the
//! comprehensive review) — and the §11 completion definition made
//! checkable (CG1-CG5).
//!
//! The tests render from a FIXTURE run directory: a complete,
//! clearly-labeled record set (commit `fixture-commit` — a temp
//! directory, never `target/campaign-evidence`) holding exactly the
//! scenarios the §9 mapping and [`matrix::cells`] enumerate. The
//! fixture tests the RENDERER's honesty — completeness, budget
//! adherence, the gate computation, reproducibility from the
//! records alone, and the standalone `campaign-summary` path —
//! which is what makes a real run's report checkable: the same
//! builder, given a real campaign's run directories, renders the
//! same matrix over the real records.
//!
//! The main test also emits row 15's own live evidence record (the
//! render's provenance, budget and gate checks as its invariants)
//! into this binary's run directory, completing the live campaign's
//! §9 coverage.

// Test target (the e2e precedent): invariant assertions may
// expect/unwrap; the `clippy::panic` allow mirrors the lib's
// documented discipline (its panics ARE the assertions).
#![allow(clippy::expect_used, clippy::unwrap_used)]
#![allow(clippy::panic)]

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};

use volvisor_campaign::matrix::{self, Family};
use volvisor_campaign::tier_r;
use volvisor_campaign::{Evidence, LogSources, build_campaign_report, completion_gates_over};
use volvisor_drbd_testkit::leak_tempdir;

/// Write one fixture record at its §6 path (`<scenario>.json`,
/// nested).
fn write_record(dir: &Path, record: &Value) {
    let scenario = record["scenario"]
        .as_str()
        .expect("the fixture record names its scenario")
        .to_owned();
    let path = dir.join(format!("{scenario}.json"));
    std::fs::create_dir_all(path.parent().expect("the parent exists")).expect("fixture dir");
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&record).expect("fixture JSON"),
    )
    .expect("write fixture record");
}

/// One Tier S fixture record's common shape.
fn tier_s(scenario: &str, outcome: &str, duration_ms: u64) -> Value {
    json!({
        "scenario": scenario,
        "tier": "S",
        "commit": "fixture-commit",
        "kernel": "fixture-kernel",
        "components": {
            "volvisord": "0.1.0",
            "drbd-tooling": "fake",
            "vmm": "fake",
            "witness": "in-process",
        },
        "fault": null,
        "oracle": null,
        "invariants": [{"fixture_invariant": "pass"}],
        "logs": null,
        "outcome": outcome,
        "duration_ms": duration_ms,
    })
}

/// One oracle section (the [`volvisor_campaign::evidence`]
/// canonical shape — the honest byte-level verdict). `barrier_at`
/// and `skew` travel together: a section that recorded a barrier
/// carries its cross-check, one that recorded none (an
/// abort-before-cut) carries neither (the comprehensive review's
/// S6 — a barrier without a skew is a gap).
fn oracle(
    acknowledged: u64,
    verified: u64,
    corrupted: u64,
    tail: u64,
    barrier_at: Option<u64>,
    skew: Option<i64>,
) -> Value {
    json!({
        "acknowledged": acknowledged,
        "verified": verified,
        "verified_side": "destination",
        "corrupted": corrupted,
        "tail": tail,
        "boundary_seq": acknowledged,
        "boundary_source": "data-path",
        "stop_reason": "paused",
        "barrier_durable_at": barrier_at,
        "boundary_skew_ticks": skew,
    })
}

/// The complete fixture run directory: every §9 Tier S row's
/// records (the generated matrix cells and family aggregates, the
/// oracle rows with their byte-level verdicts, the injection rows
/// with their typed outcomes), every Tier R gate's skip record, and
/// row 15's own record.
fn complete_fixture() -> PathBuf {
    let dir = leak_tempdir();
    fixture_matrix(&dir);
    fixture_oracle_rows(&dir);
    fixture_injections(&dir);
    fixture_move_rows(&dir);
    fixture_tier_r_gates(&dir);
    write_record(
        &dir,
        &tier_s(
            "row-15/summary-and-completion-gates",
            "pass: the coverage matrix exists, is budget-adherent, and the completion gates pass over it",
            5,
        ),
    );
    dir
}

/// Row 16 (P6-C): the same-VG move's durable-boundary scenarios —
/// one fixture record per boundary, with the outcomes the sibling
/// harness's real records carry (the fixture only stands in for
/// the render/gate machinery; the real rows live in
/// `tests/move_rows.rs`).
fn fixture_move_rows(dir: &Path) {
    let rows = [
        (
            "row-16/move-killed-after-journal-intent",
            "recovered: COMPLETE (the fresh operation re-drove the journaled intent)",
        ),
        (
            "row-16/move-killed-at-preparing-save",
            "recovered: COMPLETE (the re-driven journaled intent)",
        ),
        (
            "row-16/move-killed-after-pvmove-start",
            "recovered: COMPLETE (the rolled record resolved under a fresh operation)",
        ),
        (
            "row-16/move-pass-killed-at-verification",
            "recovered: COMPLETE (the roll-forward at startup, one bump)",
        ),
        (
            "row-16/move-pass-killed-after-complete-save",
            "recovered: COMPLETE (nothing to resolve — the durable fact stood)",
        ),
        (
            "row-16/move-aborted-out-of-band-while-down",
            "parked: IN_DOUBT with the source intact (the operator resolves it)",
        ),
    ];
    for (scenario, outcome) in rows {
        write_record(dir, &tier_s(scenario, outcome, 120));
    }
}

/// Rows 4-7: every generated cell, plus the four family aggregates
/// (whose durations are the family walls §3.2 bounds).
fn fixture_matrix(dir: &Path) {
    for cell in matrix::cells() {
        let mut record = tier_s(&cell.scenario(), "recovered: typed", 20);
        record["fault"] = json!({
            "kind": cell.hook.fault_kind(),
            "at": cell.hook.location(),
        });
        write_record(dir, &record);
    }
    for (family, wall_ms) in [
        (Family::VolumeMutations, 240_u64),
        (Family::ConsumerMobility, 890),
        (Family::PeerRoutes, 502),
        (Family::WitnessJournal, 283),
    ] {
        let mut record = tier_s(&format!("matrix/{}/_family", family.name()), "", wall_ms);
        record["outcome"] = Value::Null;
        record["invariants"] = json!([{
            "family_budget": format!("pass: the family wall is {wall_ms} ms (bound 5000 ms)")
        }]);
        write_record(dir, &record);
    }
}

/// Rows 1-3: the oracle rows (CG2's checkable form — the
/// complete-migration prefix, the abort's honest tail, the
/// data-path boundary and the barrier cross-check).
fn fixture_oracle_rows(dir: &Path) {
    let mut row_1 = tier_s(
        "row-1/happy-path",
        "complete: acknowledged prefix intact",
        78,
    );
    row_1["oracle"] = oracle(10, 10, 0, 0, Some(1010), Some(0));
    write_record(dir, &row_1);
    let mut row_2 = tier_s(
        "row-2/abort-shaped-lag",
        "aborted: source intact, tail honestly nonzero",
        41,
    );
    row_2["oracle"] = oracle(27, 27, 0, 15, None, None);
    write_record(dir, &row_2);
    for scenario in [
        "kill-matrix/transfer/after-intent",
        "kill-matrix/transfer/before-outcome",
        "kill-matrix/transfer/after-outcome",
        "kill-matrix/peer-grant/after-intent",
        "kill-matrix/peer-grant/before-outcome",
    ] {
        let mut record = tier_s(scenario, "recovered: ABORTED", 60);
        record["oracle"] = oracle(8, 8, 0, 3, Some(1010), Some(1));
        write_record(dir, &record);
    }
}

/// Rows 8-14: the injections, each ending in its typed outcome
/// (CG4's vocabulary). The multi-volume rows carry ARRAY oracles
/// (one section per volume — the shape CG2 must flatten).
fn fixture_injections(dir: &Path) {
    let injections = [
        (
            "row-8/stale-source-write-after-fence",
            "UNSAFE: the survivor's adopt refuses over the live lease; the rogue bytes never \
          reached the destination",
        ),
        (
            "row-9/wrong-lineage-data-at-target",
            "refused: FOREIGN_DEVICE_STATE — the same-id re-drive and the fresh re-issue both \
          refuse typed",
        ),
        (
            "row-9b/post-prepare-lineage-refused-at-barrier",
            "refused: FOREIGN_DEVICE_STATE — the barrier-time re-check refused the post-prepare \
          injection; the cut never crossed foreign data",
        ),
        (
            "row-10/forged-barrier-proofs",
            "refused: StaleEpoch and IdentityRequired; the holder's foreign barrier parked the \
          abort OPERATION_IN_DOUBT until its void recovered it",
        ),
        (
            "row-11/witness-divergence-journal-rollback",
            "pass: the stale view refused the renew typed and the restart self-fenced at \
          construction",
        ),
        (
            "row-12/multi-volume-cut/one-fails-promote",
            "parked safe (IN_DOUBT: source revoked, destination grant not yet authorized)",
        ),
        (
            "row-12/multi-volume-cut/source-killed-mid-drive",
            "pass: the re-issued migration converged",
        ),
        (
            "row-12/multi-volume-cut/witness-restart-mid-drive",
            "pass: the healed drive converged, nothing lost",
        ),
        (
            "row-12/resync-under-foreground",
            "pass: no caught-up claim over the in-flight resync",
        ),
        (
            "row-12/source-vmm-death-mid-cut",
            "pass: the rollback never resumed the absent VM",
        ),
        (
            "row-12/kill-during-divergence-window",
            "pass: every acknowledged write survived the kill",
        ),
        (
            "row-13/replication-partition-mid-migration",
            "pass: the cut parked over the partition and the heal covered the tail",
        ),
    ];
    for (scenario, outcome) in injections {
        write_record(dir, &tier_s(scenario, outcome, 300));
    }
    let mut converge = tier_s(
        "row-12/multi-volume-cut/converges",
        "pass: every participant's prefix intact",
        260,
    );
    converge["oracle"] = json!([
        oracle(10, 10, 0, 0, Some(1010), Some(0)),
        oracle(10, 10, 0, 0, Some(1010), Some(0)),
        oracle(10, 10, 0, 0, Some(1010), Some(0)),
    ]);
    write_record(dir, &converge);
    let mut storm = tier_s(
        "row-14/abort-storm",
        "pass: 25 cycles, no residue, no lease leak, terminal records immutable",
        2534,
    );
    storm["oracle"] = oracle(400, 400, 0, 0, Some(1010), Some(0));
    write_record(dir, &storm);
}

/// The Tier R gates: every scenario's explicit skip record (§0:
/// the matrix shows the gate, never a hole).
fn fixture_tier_r_gates(dir: &Path) {
    for scenario in tier_r::SCENARIOS {
        write_record(
            dir,
            &json!({
                "scenario": format!("tier-r/{}", scenario.name),
                "tier": "R",
                "commit": "fixture-commit",
                "kernel": "fixture-kernel",
                "components": null,
                "fault": null,
                "oracle": null,
                "invariants": [],
                "logs": null,
                "outcome": "skipped",
                "reason": scenario.skip_reason,
                "would_run": scenario.body,
                "duration_ms": 1,
            }),
        );
    }
}

/// Row 15 (§9): over the complete fixture, the coverage matrix is
/// complete (every §9 row and every Tier R gate renders its
/// verdict — never `MISSING`), every stated §3.2 budget adheres,
/// all five §11 completion gates pass, the claim-discipline header
/// is verbatim, and the recorded findings are present. Then the
/// row's own live evidence record lands in this binary's run
/// directory (the real campaign's row 15).
#[test]
fn row_15_coverage_matrix_budgets_and_gates() {
    let dir = complete_fixture();
    let report = build_campaign_report(std::slice::from_ref(&dir));

    // The claim discipline, verbatim (§6) — the header.
    assert!(
        report.contains(
            "> Tier S proves the implemented logic's behavior under the bounded\n\
             > injected fault space (§0/§3.2); it proves nothing about real media,\n\
             > real DRBD, or a real VMM; production support is not claimed."
        ),
        "the report's header is the claim discipline, verbatim"
    );
    // Complete: no missing row, no missing gate.
    assert!(
        !report.contains("MISSING"),
        "the complete fixture renders no MISSING row:\n{report}"
    );
    assert!(
        !report.contains("UNFINISHED"),
        "the complete fixture renders no UNFINISHED row:\n{report}"
    );
    // Every §9 row renders (the row numbers in the matrix).
    for row in 1..=15 {
        assert!(
            report.contains(&format!("| {row} | S |")),
            "the matrix carries §9 row {row}"
        );
    }
    // Budget adherence: no OVER verdict anywhere.
    assert!(
        !report.contains("OVER"),
        "every stated §3.2 budget adheres:\n{}",
        report
            .lines()
            .filter(|line| line.contains("OVER"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    // The findings section carries the recorded blocks.
    for title in [
        "The grant_set wedge",
        "The IN_DOUBT contract nuance",
        "The row-9 lineage gap",
        "The F1 defense-in-depth note",
        "the live-lease fence is the protection",
        "the resume gate is epoch-wide",
    ] {
        assert!(
            report.contains(title),
            "the findings section records {title:?}"
        );
    }
    // The completion gates: all five pass over the complete set.
    let gates = completion_gates_over(std::slice::from_ref(&dir));
    assert_eq!(gates.len(), 5, "CG1-CG5");
    for gate in &gates {
        assert!(
            gate.complete,
            "{} is complete over the fixture: {}",
            gate.gate, gate.detail
        );
        assert!(
            report.contains(&format!("- **{}**: pass — ", gate.gate)),
            "the report's §11 section carries {} as pass",
            gate.gate
        );
    }

    // Row 15's own live record: the render's checks as invariants
    // (the live campaign's row 15 — the fixture proved the
    // renderer; this record proves the run).
    let mut evidence = Evidence::new("row-15/summary-and-completion-gates");
    evidence.invariant(
        "coverage_matrix",
        "pass: every §9 Tier S row and every Tier R gate renders its verdict from the \
         records alone (the complete fixture renders no MISSING row; the §9 mapping is \
         the renderer's static table)",
    );
    evidence.invariant(
        "budget_adherence",
        "pass: every stated §3.2 budget (kill and oracle families 5s, storm 10s, suite \
         ~60s serialized) computes from the records' durations",
    );
    evidence.invariant(
        "completion_gates",
        "pass: CG1-CG5 compute from the records; all five pass over the complete set and \
         fail with named gaps over a partial one",
    );
    evidence.invariant(
        "reproducible",
        "pass: two renders of the same run directories are byte-identical, and the \
         standalone campaign-summary binary renders the same text",
    );
    evidence.outcome("pass: the coverage matrix exists, is budget-adherent, and the completion gates pass over it");
    let base = row_15_log_sources();
    evidence.finish(&LogSources {
        a_journal: &base.join("a"),
        b_journal: &base.join("b"),
        witness: &base.join("witness"),
    });
}

/// The round-1 review pins over the same complete fixture
/// (evidence.rs/summary.rs render semantics):
///
/// - the family budgets measure the FAMILY wall — the `_family`
///   aggregate's duration — never the max single cell (MINOR-2:
///   row 4's fixture wall is 240 ms while every cell records
///   20 ms; a max-cell render would show 20 ms and understate
///   the family ~12x);
/// - CG2's detail counts the flattened oracle SECTIONS
///   (MINOR-4: the fixture's nine oracle-bearing records carry
///   eleven sections — the row-12 array alone contributes three,
///   so a regression that ignores `Value::Array` undercounts and
///   this test fails with it).
#[test]
fn family_walls_and_flattened_sections_render_honestly() {
    let dir = complete_fixture();
    let report = build_campaign_report(std::slice::from_ref(&dir));
    assert!(
        report.contains(
            "| row 4 kill matrix: volume mutations (30 cells) | 5000 ms | 240 ms | adhere |"
        ),
        "the family budget line shows the family wall, not the max cell:\n{}",
        report
            .lines()
            .filter(|line| line.starts_with("| row 4 "))
            .collect::<Vec<_>>()
            .join("\n")
    );
    let gates = completion_gates_over(std::slice::from_ref(&dir));
    let cg2 = gates
        .iter()
        .find(|gate| gate.gate == "CG2")
        .expect("CG2 is in the gate set");
    assert!(
        cg2.complete,
        "CG2 passes over the complete fixture: {}",
        cg2.detail
    );
    assert!(
        cg2.detail.contains("11 oracle sections across 9 records"),
        "CG2's detail counts the flattened sections: {}",
        cg2.detail
    );
}

/// Row 15's log sources: the row drives no daemon and no witness —
/// its evidence is the run's records and `REPORT.md` — so the
/// captured "journals" are notes saying exactly that (the record's
/// log paths must resolve to real, truthful files, never to empty
/// placeholders implying a capture that did not happen).
fn row_15_log_sources() -> PathBuf {
    let base = leak_tempdir();
    let note = "row 15 (the evidence bundle + summary render) drives no daemon and no \
                witness: its evidence is the run directory's records and REPORT.md, \
                rendered by the campaign-summary builder from the records alone\n";
    for side in ["a", "b", "witness"] {
        let dir = base.join(side);
        std::fs::create_dir_all(&dir).expect("the row-15 log dir");
        std::fs::write(dir.join("journal.log"), note).expect("the row-15 log note");
    }
    base
}

/// The honesty of the gate computation: a partial run reads
/// `MISSING` (never a silent omission) and CG1/CG3 name the exact
/// gaps; a missing Tier R record is a CG5 failure (the hardware
/// gate must be stated, not silent).
#[test]
fn partial_runs_read_missing_never_silent() {
    let dir = complete_fixture();
    // Row 9's record, one matrix cell, and one Tier R gate record
    // go missing.
    std::fs::remove_file(dir.join("row-9/wrong-lineage-data-at-target.json"))
        .expect("row 9's record exists");
    std::fs::remove_file(
        dir.join("matrix/volume-mutations/attach_volume/journal/AfterIntent.json"),
    )
    .expect("the cell record exists");
    std::fs::remove_file(dir.join("tier-r/power-cut.json")).expect("the gate record exists");

    let report = build_campaign_report(std::slice::from_ref(&dir));
    assert!(
        report.contains("**MISSING**: row-9/wrong-lineage-data-at-target"),
        "the missing row-9 record is named:\n{report}"
    );
    assert!(
        report.contains("attach_volume/journal/AfterIntent"),
        "the missing cell is named"
    );
    assert!(
        report.contains("tier-r/power-cut"),
        "the missing Tier R gate is named"
    );

    let gates = completion_gates_over(&[dir]);
    let named: Vec<&volvisor_campaign::GateStatus> =
        gates.iter().filter(|gate| !gate.complete).collect();
    let incomplete: Vec<&str> = named.iter().map(|gate| gate.gate).collect();
    assert_eq!(
        incomplete,
        vec!["CG1", "CG3", "CG4", "CG5"],
        "exactly the coverage, matrix, injection-outcome and gate-discipline checks fail \
         (row 9 is an injection row: its missing record is CG4's gap too)"
    );
    let cg1 = named.iter().find(|gate| gate.gate == "CG1").expect("CG1");
    assert!(
        cg1.detail.contains("row 9") && cg1.detail.contains("row 4"),
        "CG1 names the gapped rows: {}",
        cg1.detail
    );
}

/// §6's reproducibility: the report renders from the records alone
/// — the same directories render byte-identical text, twice, with
/// no live state in between.
#[test]
fn the_report_is_reproducible_from_records_alone() {
    let dir = complete_fixture();
    let first = build_campaign_report(std::slice::from_ref(&dir));
    let second = build_campaign_report(&[dir]);
    assert_eq!(first, second, "two renders are byte-identical");
    assert!(
        first.contains("fixture-commit"),
        "the provenance comes from the records"
    );
}

/// The standalone path (§6): the `campaign-summary` binary renders
/// exactly the library's text from the same run directory, and
/// `--check` turns the §11 gates into a process exit code — success
/// over the complete set, failure over a partial one.
#[test]
fn the_standalone_bin_renders_and_checks() {
    let dir = complete_fixture();
    let output = Command::new(env!("CARGO_BIN_EXE_campaign-summary"))
        .arg(&dir)
        .output()
        .expect("the campaign-summary binary runs");
    assert!(
        output.status.success(),
        "the bin succeeds over the complete fixture: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout.trim_end(),
        build_campaign_report(std::slice::from_ref(&dir)).trim_end(),
        "the bin renders the library's exact text"
    );

    let checked = Command::new(env!("CARGO_BIN_EXE_campaign-summary"))
        .arg("--check")
        .arg(&dir)
        .output()
        .expect("the campaign-summary binary runs with --check");
    assert!(
        checked.status.success(),
        "--check passes over the complete fixture: {}",
        String::from_utf8_lossy(&checked.stderr)
    );

    std::fs::remove_file(dir.join("row-14/abort-storm.json")).expect("the storm record exists");
    let partial = Command::new(env!("CARGO_BIN_EXE_campaign-summary"))
        .arg("--check")
        .arg(&dir)
        .output()
        .expect("the campaign-summary binary runs over the partial set");
    assert!(
        !partial.status.success(),
        "--check fails over a partial set (the gates are the exit code)"
    );
    assert!(
        String::from_utf8_lossy(&partial.stderr).contains("completion gate"),
        "the failure names the gates: {}",
        String::from_utf8_lossy(&partial.stderr)
    );
}
