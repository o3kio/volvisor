//! The campaign summary renderer (P5 plan §6, stage D): the final
//! `REPORT.md` — the §10 coverage matrix (fault class × scenario ×
//! verdict, with per-family durations and budget adherence), the
//! recorded findings and the §11 completion-gate checklist — built
//! from the run directories' records ALONE (§6: the report must be
//! reproducible from the records; the renderer reads nothing but the
//! record files — no live rig, no test state, no clock).
//!
//! Three entry points share one builder:
//!
//! - [`render_report`]: one run directory — the per-finish refresh
//!   every scenario's `Evidence::finish` triggers. A single run
//!   directory holds only its own test binary's records (each
//!   campaign test binary is its own process, hence its own run
//!   directory), so a partial view reads `MISSING` for absent rows,
//!   never a silent omission — the file describes the run as far as
//!   it has progressed.
//! - [`build_campaign_report`]: many run directories merged — the
//!   shippable artifact. Records dedupe by scenario (the latest run
//!   wins; directories are processed in ascending modified-time
//!   order), and mixed commits across the constituent runs are
//!   flagged in the provenance, never averaged away.
//! - the `campaign-summary` binary: the standalone path over the
//!   same builder (`--check` makes the §11 gates a process exit
//!   code).
//!
//! ## The claim discipline (§0/§6, verbatim)
//!
//! Tier S proves the implemented logic's behavior under the bounded
//! injected fault space (§0/§3.2); it proves nothing about real
//! media, real DRBD, or a real VMM; production support is not
//! claimed. The report carries it as its header, and the Tier R
//! rows carry the open hardware gate.

use std::path::{Path, PathBuf};

use serde_json::Value;

/// One formatted report line (`writeln!` over the report string —
/// its `fmt::Write` impl is infallible; the Result discard keeps
/// that explicit).
macro_rules! line {
    ($report:expr, $($argument:tt)*) => {{
        use std::fmt::Write as _;
        let _ = writeln!($report, $($argument)*);
    }};
}

use crate::matrix::{self, Family};
use crate::tier_r;

/// The suite's whole-campaign bound (§3.2: "the whole campaign
/// inside the existing suite budget ~60s"), in milliseconds. The
/// renderer checks the SERIALIZED sum of the top-level scenario
/// durations against it — an upper bound on the suite's wall time
/// (the binaries and the cells run with bounded parallelism, so
/// wall time is at most the sum), which keeps the check honest
/// without a wall-clock harness.
pub const SUITE_BOUND_MS: u64 = 60_000;

// ---------------------------------------------------------------------------
// The §9 mapping (static, reviewable — CG1's executable form)
// ---------------------------------------------------------------------------

/// How one §9 row's records are selected from the run directories.
enum Coverage {
    /// The exact scenario record names (rows with fixed scenarios).
    Names(&'static [&'static str]),
    /// Every generated cell of one kill-matrix family (rows 4-7:
    /// the expected set is [`matrix::cells`] — the matrix is the
    /// campaign's single enumeration, §3.2).
    MatrixFamily(Family),
}

/// One §9 Tier S row's mapping into the coverage matrix. §9 states
/// the table "so CG1 is reviewable now, not only in the report" —
/// this is its executable form: the renderer realizes exactly this
/// mapping, and the §9 table in the plan remains the review
/// surface.
struct RowSpec {
    /// §9's row number.
    row: u8,
    /// §9's family label (short form).
    family: &'static str,
    /// The nearline §10 / SPEC-0002 fault class the row covers.
    class: &'static str,
    /// The row's scenario records.
    coverage: Coverage,
    /// The §3.2 per-family budget in milliseconds, when the plan
    /// states one for this row's family (the kill families and the
    /// oracle families are ≤ 5s; the storm ≤ 10s; the injection
    /// rows carry no stated bound and show durations only).
    budget_ms: Option<u64>,
}

/// The §9 Tier S rows (1-15), in row order.
const ROWS: &[RowSpec] = &[
    RowSpec {
        row: 1,
        family: "oracle across a happy-path migration",
        class: "write-trace oracle, planned migration",
        coverage: Coverage::Names(&["row-1/happy-path"]),
        budget_ms: Some(5_000),
    },
    RowSpec {
        row: 2,
        family: "oracle across an aborted migration",
        class: "write-trace oracle, tail under abort",
        coverage: Coverage::Names(&["row-2/abort-shaped-lag"]),
        budget_ms: Some(5_000),
    },
    RowSpec {
        row: 3,
        family: "oracle across kill-and-recover migrations",
        class: "SIGKILL during WAL/meta commit",
        coverage: Coverage::Names(&[
            "kill-matrix/transfer/after-intent",
            "kill-matrix/transfer/before-outcome",
            "kill-matrix/transfer/after-outcome",
            "kill-matrix/peer-grant/after-intent",
            "kill-matrix/peer-grant/before-outcome",
        ]),
        budget_ms: Some(5_000),
    },
    RowSpec {
        row: 4,
        family: "kill matrix: volume mutations",
        class: "SIGKILL during ACK/WAL commit",
        coverage: Coverage::MatrixFamily(Family::VolumeMutations),
        budget_ms: Some(5_000),
    },
    RowSpec {
        row: 5,
        family: "kill matrix: consumer mobility routes",
        class: "SIGKILL during WAL commit",
        coverage: Coverage::MatrixFamily(Family::ConsumerMobility),
        budget_ms: Some(5_000),
    },
    RowSpec {
        row: 6,
        family: "kill matrix: peer routes",
        class: "SIGKILL during peer stream/replay",
        coverage: Coverage::MatrixFamily(Family::PeerRoutes),
        budget_ms: Some(5_000),
    },
    RowSpec {
        row: 7,
        family: "kill matrix: witness journal",
        class: "SIGKILL during meta commit, quorum loss window",
        coverage: Coverage::MatrixFamily(Family::WitnessJournal),
        budget_ms: Some(5_000),
    },
    RowSpec {
        row: 8,
        family: "stale source write after fence",
        class: "source-after-fence stale writes",
        coverage: Coverage::Names(&["row-8/stale-source-write-after-fence"]),
        budget_ms: None,
    },
    RowSpec {
        row: 9,
        family: "wrong-lineage injection at the target",
        class: "wrong-epoch data injection (lineage-shaped)",
        coverage: Coverage::Names(&["row-9/wrong-lineage-data-at-target"]),
        budget_ms: None,
    },
    RowSpec {
        row: 10,
        family: "forged barrier proofs",
        class: "wrong-epoch barrier injection",
        coverage: Coverage::Names(&["row-10/forged-barrier-proofs"]),
        budget_ms: None,
    },
    RowSpec {
        row: 11,
        family: "witness divergence",
        class: "control-plane disconnect / stale authority",
        coverage: Coverage::Names(&["row-11/witness-divergence-journal-rollback"]),
        budget_ms: None,
    },
    RowSpec {
        row: 12,
        family: "concurrent multi-volume cut, resync under foreground, \
                 source-VMM death mid-cut, kills during an in-flight resync",
        class: "multi-disk final cut; resync while foreground continues; \
                VMM crash; SIGKILL during the dirty-bitmap window (logical)",
        coverage: Coverage::Names(&[
            "row-12/multi-volume-cut/converges",
            "row-12/multi-volume-cut/one-fails-promote",
            "row-12/multi-volume-cut/source-killed-mid-drive",
            "row-12/multi-volume-cut/witness-restart-mid-drive",
            "row-12/resync-under-foreground",
            "row-12/source-vmm-death-mid-cut",
            "row-12/kill-during-divergence-window",
        ]),
        budget_ms: None,
    },
    RowSpec {
        row: 13,
        family: "replication partition mid-migration",
        class: "storage network disconnect",
        coverage: Coverage::Names(&["row-13/replication-partition-mid-migration"]),
        budget_ms: None,
    },
    RowSpec {
        row: 14,
        family: "abort storm (25 cycles, rotating pre-cut faults)",
        class: "repeated migration aborts",
        coverage: Coverage::Names(&["row-14/abort-storm"]),
        budget_ms: Some(10_000),
    },
    RowSpec {
        row: 15,
        family: "evidence bundle + summary render",
        class: "exact versions, independent harness, full logs",
        coverage: Coverage::Names(&["row-15/summary-and-completion-gates"]),
        budget_ms: None,
    },
];

/// One §9 row's expected scenario names, in matrix order.
fn expected_names(spec: &RowSpec) -> Vec<String> {
    match &spec.coverage {
        Coverage::Names(names) => names.iter().map(ToString::to_string).collect(),
        Coverage::MatrixFamily(family) => matrix::family_cells(*family)
            .iter()
            .map(matrix::Cell::scenario)
            .collect(),
    }
}

// ---------------------------------------------------------------------------
// The merged campaign view
// ---------------------------------------------------------------------------

/// One constituent run directory's provenance.
struct RunInfo {
    /// The directory's name (the run id).
    name: String,
    /// The commit the run's records carry (the first record's;
    /// `unknown` for an empty run).
    commit: String,
}

/// The merged campaign: every record from every run directory,
/// deduped by scenario (the latest run wins), with per-run
/// provenance.
struct Campaign {
    records: Vec<Value>,
    runs: Vec<RunInfo>,
}

/// Collect one directory's records (the §6 layout: scenario-nested
/// JSON files; the `logs/` capture is skipped — logs are referenced
/// by path, never inlined).
fn collect_dir(dir: &Path, records: &mut Vec<Value>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if entry.file_name() != "logs" {
                collect_dir(&path, records);
            }
        } else if path
            .extension()
            .is_some_and(|extension| extension == "json")
        {
            if let Ok(body) = std::fs::read_to_string(&path) {
                if let Ok(record) = serde_json::from_str::<Value>(&body) {
                    records.push(record);
                }
            }
        }
    }
}

/// The merged view over the given run directories: records collected
/// per directory (in ascending modified-time order, so a scenario
/// finished again in a later run supersedes the earlier record),
/// deduped by scenario name with the latest occurrence kept.
fn merge(dirs: &[PathBuf]) -> Campaign {
    let mut ordered: Vec<&PathBuf> = dirs.iter().collect();
    ordered.sort_by_key(|dir| {
        std::fs::metadata(dir)
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |duration| duration.as_millis())
    });
    let mut runs = Vec::new();
    let mut records: Vec<Value> = Vec::new();
    for dir in ordered {
        let mut run_records = Vec::new();
        collect_dir(dir, &mut run_records);
        let commit = run_records
            .first()
            .and_then(|record| record["commit"].as_str())
            .unwrap_or("unknown")
            .to_owned();
        runs.push(RunInfo {
            name: dir.file_name().map_or_else(
                || "?".to_owned(),
                |name| name.to_string_lossy().into_owned(),
            ),
            commit,
        });
        // The latest run wins per scenario: drop any earlier record
        // of a scenario this run re-emitted.
        records.retain(|record| {
            let scenario = record["scenario"].as_str().unwrap_or_default();
            !run_records
                .iter()
                .any(|fresh| fresh["scenario"].as_str() == Some(scenario))
        });
        records.extend(run_records);
    }
    records.sort_by(|left, right| {
        left["scenario"]
            .as_str()
            .unwrap_or_default()
            .cmp(right["scenario"].as_str().unwrap_or_default())
    });
    Campaign { records, runs }
}

/// The record for exactly `scenario`, when present.
fn record_for<'a>(records: &'a [Value], scenario: &str) -> Option<&'a Value> {
    records
        .iter()
        .find(|record| record["scenario"].as_str() == Some(scenario))
}

// ---------------------------------------------------------------------------
// The coverage matrix
// ---------------------------------------------------------------------------

/// One coverage-matrix row's computed verdict.
enum RowVerdict {
    /// Every expected record is present with an outcome.
    Covered {
        /// The record count (expected == present).
        count: usize,
        /// The primary record's outcome text.
        outcome: String,
        /// The row's maximum record duration, in milliseconds.
        duration_ms: u64,
    },
    /// Some expected records are absent — named, never silent.
    Missing(Vec<String>),
    /// Present but without an outcome (an unfinished record is an
    /// honest gap, not a pass).
    Unfinished(Vec<String>),
}

/// The row verdict over `records`.
fn row_verdict(spec: &RowSpec, records: &[Value]) -> RowVerdict {
    let expected = expected_names(spec);
    let missing: Vec<String> = expected
        .iter()
        .filter(|scenario| record_for(records, scenario).is_none())
        .cloned()
        .collect();
    if !missing.is_empty() {
        return RowVerdict::Missing(missing);
    }
    let unfinished: Vec<String> = expected
        .iter()
        .filter(|scenario| {
            record_for(records, scenario).is_some_and(|record| record["outcome"].is_null())
        })
        .cloned()
        .collect();
    if !unfinished.is_empty() {
        return RowVerdict::Unfinished(unfinished);
    }
    let duration_ms = expected
        .iter()
        .filter_map(|scenario| record_for(records, scenario))
        .filter_map(|record| record["duration_ms"].as_u64())
        .max()
        .unwrap_or(0);
    let outcome = expected
        .iter()
        .find_map(|scenario| record_for(records, scenario))
        .and_then(|record| record["outcome"].as_str())
        .unwrap_or("?")
        .to_owned();
    RowVerdict::Covered {
        count: expected.len(),
        outcome,
        duration_ms,
    }
}

// ---------------------------------------------------------------------------
// Budgets (§3.2)
// ---------------------------------------------------------------------------

/// One budget line: a family or row's measured wall time against
/// its §3.2 bound.
struct BudgetLine {
    /// The line's label.
    label: String,
    /// The §3.2 bound, in milliseconds.
    bound_ms: u64,
    /// The measured duration, in milliseconds.
    measured_ms: u64,
}

impl BudgetLine {
    /// Whether the measurement is inside the bound.
    fn adheres(&self) -> bool {
        self.measured_ms <= self.bound_ms
    }
}

/// The budget lines computable from `records` (§3.2's stated bounds
/// only — the plan bounds the kill families, the oracle families,
/// the storm and the suite; the injection rows carry no stated
/// bound and show durations in the matrix alone). For the
/// kill-matrix families the measured value is the FAMILY wall —
/// the `matrix/<family>/_family` record's duration, which
/// `run_family` records over the whole concurrent cell batch —
/// never the max single-cell duration (a cell that ran alone
/// understates the family ~the batch's width; round-1 review,
/// MINOR-2). When the aggregate record is absent the max-cell
/// duration is the fallback (a partial run keeps its table row;
/// CG3 flags the missing aggregate separately).
fn budget_lines(records: &[Value]) -> Vec<BudgetLine> {
    let mut lines = Vec::new();
    for spec in ROWS {
        let Some(bound_ms) = spec.budget_ms else {
            continue;
        };
        let expected = expected_names(spec);
        let max_cell_ms = || {
            expected
                .iter()
                .filter_map(|scenario| record_for(records, scenario))
                .filter_map(|record| record["duration_ms"].as_u64())
                .max()
                .unwrap_or(0)
        };
        let measured_ms = match spec.coverage {
            Coverage::MatrixFamily(family) => {
                record_for(records, &format!("matrix/{}/_family", family.name()))
                    .and_then(|record| record["duration_ms"].as_u64())
                    .unwrap_or_else(max_cell_ms)
            }
            Coverage::Names(_) => max_cell_ms(),
        };
        let cells = match spec.coverage {
            Coverage::MatrixFamily(family) => {
                format!(" ({} cells)", matrix::family_cells(family).len())
            }
            Coverage::Names(_) => String::new(),
        };
        lines.push(BudgetLine {
            label: format!("row {} {}{}", spec.row, spec.family, cells),
            bound_ms,
            measured_ms,
        });
    }
    lines
}

/// The serialized sum of the campaign's top-level scenario
/// durations (families, rows and Tier R scaffolds) — an upper bound
/// on the suite's wall time (bounded parallelism), checked against
/// [`SUITE_BOUND_MS`]. Top-level only: the matrix families
/// aggregate through their `_family` record (the cells run inside
/// it), so the cells' own durations are not summed.
fn suite_serialized_ms(records: &[Value]) -> u64 {
    records
        .iter()
        .filter(|record| {
            let scenario = record["scenario"].as_str().unwrap_or_default();
            !scenario.starts_with("matrix/") || scenario.ends_with("/_family")
        })
        .filter_map(|record| record["duration_ms"].as_u64())
        .sum()
}

// ---------------------------------------------------------------------------
// The completion gates (§11, CG1-CG5)
// ---------------------------------------------------------------------------

/// One completion gate's computed status.
pub struct GateStatus {
    /// The gate's id (CG1-CG5).
    pub gate: &'static str,
    /// Whether the gate holds over the records it was computed
    /// from.
    pub complete: bool,
    /// The checkable detail: what was verified, or exactly what is
    /// missing (scenario names, budgets, violations).
    pub detail: String,
}

/// The typed-marker vocabulary CG4 accepts (§11: "a typed refusal,
/// an honest UNSAFE/IN_DOUBT classification, or a pass — never a
/// silent corruption or a generic success").
const TYPED_MARKERS: [&str; 4] = ["refus", "UNSAFE", "IN_DOUBT", "pass"];

/// Compute the §11 completion gates over `records` — the report's
/// CG1-CG5 checklist and the `--check` exit path. Every gate is
/// computed from the records alone (§6's reproducibility); a gate
/// that cannot be verified from what is present reads `incomplete`
/// with the exact gap named, never a pass by absence.
///
/// # What the gates certify (the comprehensive review's U6, stated
/// honestly)
///
/// CG2 and CG3 are record-SHAPE gates: they verify that every
/// expected record EXISTS, carries the canonical non-null fields
/// (the oracle sections with their byte-verdict counts and
/// cross-checks, the kill-matrix cells with their invariant sets),
/// that the stated budgets adhere and that no recorded verdict
/// contradicts the discipline (a negative boundary skew, an
/// unmarked injection outcome). They do NOT re-run the scenarios or
/// re-verify the bytes: the behavioral verification lives in the
/// tests that EMIT the records (the scenario rows' own assertions);
/// the gates certify the evidence bundle's coherence — that what
/// the campaign claims to have proven is all present, well-formed
/// and honest in shape. A record whose fields lie (a hand-tuned
/// count, a fabricated verdict) is beyond the gates' letter; the
/// defense there is the record's provenance (one commit, the
/// rendering pipeline) and the tests themselves.
#[must_use]
pub fn completion_gates(records: &[Value]) -> Vec<GateStatus> {
    vec![
        cg1_class_coverage(records),
        cg2_oracle_verdicts(records),
        cg3_kill_matrix_and_budgets(records),
        cg4_injection_outcomes(records),
        cg5_claim_discipline(records),
    ]
}

/// The §11 completion gates over the given run directories — over
/// exactly what [`build_campaign_report`] renders (collect, merge,
/// compute): the `campaign-summary --check` exit path.
#[must_use]
pub fn completion_gates_over(dirs: &[PathBuf]) -> Vec<GateStatus> {
    completion_gates(&merge(dirs).records)
}

/// CG1: every nearline §10 / SPEC-0002 fault class is covered by a
/// Tier S scenario family (the §9 mapping) or recorded as Tier
/// R-gated / out-of-scope with its reason.
fn cg1_class_coverage(records: &[Value]) -> GateStatus {
    let mut missing: Vec<String> = Vec::new();
    for spec in ROWS {
        if let RowVerdict::Missing(absent) = row_verdict(spec, records) {
            missing.push(format!("row {} ({})", spec.row, absent.join(", ")));
        }
    }
    for scenario in tier_r::SCENARIOS {
        if record_for(records, &format!("tier-r/{}", scenario.name)).is_none() {
            missing.push(format!("tier-r/{} (no gate record)", scenario.name));
        }
    }
    // The single-commit certification (the comprehensive review's
    // U3): the gates certify ONE campaign at ONE commit. The `--all`
    // merge is latest-wins per scenario (a feature — a partial
    // re-run heals an older tree), but a WINNING set that still
    // mixes commits is a frankenstein: a scenario the newest run
    // does not re-emit survives from an older commit, and no gate
    // can honestly certify over records from different revisions.
    // A mixed-commit tree reads INCOMPLETE here, with the distinct
    // commits named — the merge itself stays, only the
    // certification tightens.
    let mut commits: Vec<&str> = records
        .iter()
        .map(|record| record["commit"].as_str().unwrap_or("unknown"))
        .collect();
    commits.sort_unstable();
    commits.dedup();
    if commits.len() > 1 {
        missing.push(format!(
            "the winning records mix {} commits ({}) — the certification covers one \
             campaign at one commit; re-run the full suite at a single revision and \
             certify with --all",
            commits.len(),
            commits.join(", ")
        ));
    }
    GateStatus {
        gate: "CG1",
        complete: missing.is_empty(),
        detail: if missing.is_empty() {
            format!(
                "every §9 Tier S row (1-15) has its records and every Tier R-only class \
                 carries its gate record ({} skipped records), all at one commit ({}); \
                 the §9 mapping is realized in the coverage matrix",
                tier_r::SCENARIOS.len(),
                records
                    .first()
                    .and_then(|record| record["commit"].as_str())
                    .unwrap_or("unknown")
            )
        } else {
            format!(
                "not every class is covered or gated: {}",
                missing.join("; ")
            )
        },
    }
}

/// One oracle-bearing record's oracle sections: a single object for
/// the single-volume rows, an array for the multi-volume rows
/// (§9 row 12 — one section per volume, each the same canonical
/// shape).
fn oracle_sections(record: &Value) -> Vec<&Value> {
    match &record["oracle"] {
        Value::Array(sections) => sections.iter().collect(),
        Value::Object(_) => vec![&record["oracle"]],
        _ => Vec::new(),
    }
}

/// CG2: the oracle's verdicts appear in the records —
/// acknowledged-prefix preservation across a complete migration,
/// honest (nonzero) tail quantification on an abort, and the
/// boundary derived from the data path with the coordinator
/// cross-check present. The multi-volume rows carry one oracle
/// section per volume; every section is checked.
fn cg2_oracle_verdicts(records: &[Value]) -> GateStatus {
    let mut problems: Vec<String> = Vec::new();
    // The oracle sections, flattened with their record's scenario.
    let oracle_records: Vec<&Value> = records
        .iter()
        .filter(|record| !record["oracle"].is_null())
        .collect();
    let sections: Vec<(String, &Value)> = oracle_records
        .iter()
        .flat_map(|record| {
            let scenario = record["scenario"].as_str().unwrap_or("?").to_owned();
            oracle_sections(record)
                .into_iter()
                .map(move |section| (scenario.clone(), section))
        })
        .collect();
    let complete_prefix = sections.iter().any(|(_, oracle)| {
        oracle["acknowledged"].as_u64().unwrap_or(0) > 0
            && oracle["verified"] == oracle["acknowledged"]
            && oracle["corrupted"].as_u64() == Some(0)
    });
    if !complete_prefix {
        problems.push("no complete-migration record proves the acknowledged prefix".to_owned());
    }
    let honest_tail = sections
        .iter()
        .any(|(_, oracle)| oracle["tail"].as_u64().unwrap_or(0) > 0);
    if !honest_tail {
        problems.push("no abort record quantifies a nonzero tail".to_owned());
    }
    let bad_boundary: Vec<String> = sections
        .iter()
        .filter(|(_, oracle)| oracle["boundary_source"].as_str() != Some("data-path"))
        .map(|(scenario, _)| scenario.clone())
        .collect();
    if !bad_boundary.is_empty() {
        problems.push(format!(
            "records derive the boundary from something other than the data path: {}",
            bad_boundary.join(", ")
        ));
    }
    let cross_checked = sections
        .iter()
        .any(|(_, oracle)| !oracle["boundary_skew_ticks"].is_null());
    if !cross_checked {
        problems.push("no record carries the barrier/boundary cross-check".to_owned());
    }
    // S6 (the comprehensive review): a section that records a
    // barrier over acknowledged writes MUST also carry the
    // cross-check — a barrier without its skew is a gap, never a
    // pass. Sections with no acknowledged writes are exempt (there
    // is nothing to cross-check against).
    let un_cross_checked: Vec<String> = sections
        .iter()
        .filter(|(_, oracle)| {
            !oracle["barrier_durable_at"].is_null()
                && oracle["acknowledged"].as_u64().unwrap_or(0) > 0
                && oracle["boundary_skew_ticks"].is_null()
        })
        .map(|(scenario, _)| scenario.clone())
        .collect();
    if !un_cross_checked.is_empty() {
        problems.push(format!(
            "records carry a barrier over acknowledged writes without the boundary \
             cross-check: {}",
            un_cross_checked.join(", ")
        ));
    }
    // S1 (the comprehensive review): the cross-check must be LIVE,
    // not a recorded tautology — a NEGATIVE skew (a barrier stamped
    // before the last acknowledged write it should have covered) is
    // the §2.3 rule-1 violation, and it fails the gate wherever it
    // appears. Under the old frozen-clock design every past-cap
    // record read a mechanical 0 and this clause could never fire;
    // with the monotonic stamp clock it is checkable per record.
    let negative_skew: Vec<String> = sections
        .iter()
        .filter(|(_, oracle)| {
            oracle["boundary_skew_ticks"]
                .as_i64()
                .is_some_and(|skew| skew < 0)
        })
        .map(|(scenario, _)| scenario.clone())
        .collect();
    if !negative_skew.is_empty() {
        problems.push(format!(
            "records carry a barrier stamped BEFORE the last acknowledged write it should \
             have covered (negative boundary skew): {}",
            negative_skew.join(", ")
        ));
    }
    GateStatus {
        gate: "CG2",
        complete: problems.is_empty(),
        detail: if problems.is_empty() {
            format!(
                "{} oracle sections across {} records: the complete-migration prefix is \
                 byte-verified, aborts quantify their tails, every boundary is data-path \
                 derived and every barrier-bearing section carries its cross-check",
                sections.len(),
                oracle_records.len()
            )
        } else {
            problems.join("; ")
        },
    }
}

/// CG3: the kill matrix covers every journaled mutation stage at
/// the §3.2 bounds — every generated cell has its record (with its
/// invariant set), and every stated budget adheres.
fn cg3_kill_matrix_and_budgets(records: &[Value]) -> GateStatus {
    let mut problems: Vec<String> = Vec::new();
    for cell in matrix::cells() {
        let scenario = cell.scenario();
        match record_for(records, &scenario) {
            None => problems.push(format!("cell {scenario} has no record")),
            Some(record) => {
                if record["invariants"].as_array().is_none_or(Vec::is_empty) {
                    problems.push(format!("cell {scenario} records no invariants"));
                }
            }
        }
    }
    for family in [
        Family::VolumeMutations,
        Family::ConsumerMobility,
        Family::PeerRoutes,
        Family::WitnessJournal,
    ] {
        let scenario = format!("matrix/{}/_family", family.name());
        if record_for(records, &scenario).is_none() {
            problems.push(format!("family {scenario} has no aggregate record"));
        }
    }
    let over: Vec<String> = budget_lines(records)
        .into_iter()
        .filter(|line| !line.adheres())
        .map(|line| {
            format!(
                "{}: {} ms over its {} ms bound",
                line.label, line.measured_ms, line.bound_ms
            )
        })
        .collect();
    problems.extend(over);
    if suite_serialized_ms(records) > SUITE_BOUND_MS {
        problems.push(format!(
            "the serialized campaign sum is {} ms over the {} ms suite bound",
            suite_serialized_ms(records),
            SUITE_BOUND_MS
        ));
    }
    GateStatus {
        gate: "CG3",
        complete: problems.is_empty(),
        detail: if problems.is_empty() {
            format!(
                "all {} generated cells across the four families have records with their \
                 invariant sets, every stated §3.2 budget adheres and the serialized sum \
                 ({} ms) is inside the {} ms suite bound",
                matrix::cells().len(),
                suite_serialized_ms(records),
                SUITE_BOUND_MS
            )
        } else {
            problems.join("; ")
        },
    }
}

/// The §9 rows whose scenarios are the §5 adversarial injections
/// (CG4's scope) — including row 14, the abort storm (§5.7's
/// rotating pre-cut faults; round-1 review, MINOR-3).
const INJECTION_ROWS: [u8; 7] = [8, 9, 10, 11, 12, 13, 14];

/// CG4: every adversarial injection ends in a typed refusal, an
/// honest UNSAFE/IN_DOUBT classification, or a pass — checked at
/// the report level by the OUTCOME text alone: the marker
/// vocabulary is matched against `record["outcome"]`, never the
/// whole serialized record (every record's invariants contain
/// "pass", so the wider match was vacuous; round-1 review,
/// MINOR-3). The scenarios themselves assert the exact wire-level
/// refusals; the gate makes the vocabulary checkable from the
/// artifacts.
fn cg4_injection_outcomes(records: &[Value]) -> GateStatus {
    let mut problems: Vec<String> = Vec::new();
    for row in INJECTION_ROWS {
        let Some(spec) = ROWS.iter().find(|spec| spec.row == row) else {
            continue;
        };
        for scenario in expected_names(spec) {
            match record_for(records, &scenario) {
                None => problems.push(format!("{scenario} has no record")),
                Some(record) => match record["outcome"].as_str() {
                    None => problems.push(format!("{scenario} records no outcome")),
                    Some(outcome) => {
                        let lowercase = outcome.to_lowercase();
                        if !TYPED_MARKERS
                            .iter()
                            .any(|marker| lowercase.contains(&marker.to_lowercase()))
                        {
                            problems.push(format!(
                                "{scenario}'s outcome carries none of the typed markers \
                                 (refusal/UNSAFE/IN_DOUBT/pass): {outcome:?}"
                            ));
                        }
                    }
                },
            }
        }
    }
    GateStatus {
        gate: "CG4",
        complete: problems.is_empty(),
        detail: if problems.is_empty() {
            "every §5 injection record (§9 rows 8-14) ends in a typed refusal, an \
             UNSAFE/IN_DOUBT classification or a pass — the outcome text alone carries \
             the marker"
                .to_owned()
        } else {
            problems.join("; ")
        },
    }
}

/// The forbidden claim phrases (CG5, the comprehensive review's
/// U1): plan §11's letter forbids claiming "production support
/// **or real-host durability**" from Tier S evidence, so the scan
/// matches this vocabulary, case-insensitively. Scoped to TIER S
/// records: a Tier R record legitimately discusses real hosts —
/// that is its job, stating the gate that was NOT run (and its
/// outcome vocabulary is separately gated; see
/// [`cg5_claim_discipline`]'s Tier R clause below and the Tier R
/// outcome gate).
const CLAIM_PHRASES: [&str; 5] = [
    "production support",
    "real-host",
    "real drbd",
    "real vmm",
    "proven on hardware",
];

/// CG5: the claim discipline holds everywhere — no record claims
/// production support OR real-host durability from Tier S evidence
/// (the discipline sentence lives in the report header, never in a
/// record), and the open hardware gate is stated, not silent (every
/// Tier R scenario carries its record).
///
/// The claim vocabulary (the comprehensive review's U1): the plan's
/// §11 letter forbids claiming "production support **or real-host
/// durability**" — the scan matches a small vocabulary of such
/// phrases, case-insensitively, over TIER S records only, so a
/// record claiming "real-host durability proven… on real DRBD
/// media, real VMM" fails the gate exactly like a "production
/// support" claim. Tier R records are exempt from the phrase scan
/// (their job is to name the real-host gates honestly; their
/// outcome vocabulary is gated separately in this same gate — an
/// outcome outside {skipped, blocked} is a loud failure). The
/// vocabulary is deliberately explicit: adding a phrase is a
/// conscious gate decision, never an accident.
fn cg5_claim_discipline(records: &[Value]) -> GateStatus {
    let mut problems: Vec<String> = Vec::new();
    for record in records {
        // Tier S records only: a Tier R record naming the real-host
        // gate is the discipline WORKING, not violating it.
        if record["tier"].as_str() != Some("S") {
            continue;
        }
        let text = serde_json::to_string(record)
            .unwrap_or_default()
            .to_lowercase();
        let claim = CLAIM_PHRASES.iter().find(|phrase| text.contains(*phrase));
        if let Some(claim) = claim {
            let scenario = record["scenario"].as_str().unwrap_or("?");
            problems.push(format!(
                "record {scenario} carries a forbidden claim phrase ({claim:?}: the claim \
                 discipline lives in the report header, never in a record)"
            ));
        }
    }
    // The Tier R outcome vocabulary (the comprehensive review's U2,
    // gate side): a Tier R record is a gate statement — skipped or
    // blocked, never a pass. A hand-authored "pass" record fails
    // the gate loudly instead of rendering as a matrix pass.
    let claimed: Vec<String> = records
        .iter()
        .filter(|record| {
            record["tier"].as_str() == Some("R")
                && !matches!(
                    record["outcome"]
                        .as_str()
                        .map(str::to_ascii_lowercase)
                        .as_deref(),
                    Some("skipped" | "blocked")
                )
        })
        .map(|record| record["scenario"].as_str().unwrap_or("?").to_owned())
        .collect();
    if !claimed.is_empty() {
        problems.push(format!(
            "Tier R records claim an outcome outside the gate vocabulary \
             (skipped/blocked) — a Tier R record is a gate statement, never a pass: {}",
            claimed.join(", ")
        ));
    }
    let ungated: Vec<String> = tier_r::SCENARIOS
        .iter()
        .map(|scenario| format!("tier-r/{}", scenario.name))
        .filter(|scenario| record_for(records, scenario).is_none())
        .collect();
    if !ungated.is_empty() {
        problems.push(format!(
            "the hardware gate is silent for: {} (§0: not run must be recorded, not absent)",
            ungated.join(", ")
        ));
    }
    GateStatus {
        gate: "CG5",
        complete: problems.is_empty(),
        detail: if problems.is_empty() {
            format!(
                "no record claims production support or real-host durability from Tier S \
                 evidence, every Tier R scenario states its gate as a gate statement \
                 ({} records); the report header carries the discipline verbatim and \
                 the not-delivered §10 rows are enumerated in the nearline §10 note",
                tier_r::SCENARIOS.len()
            )
        } else {
            problems.join("; ")
        },
    }
}

// ---------------------------------------------------------------------------
// The report
// ---------------------------------------------------------------------------

/// The recorded-findings blocks (§6: the report is the only place
/// campaign results are summarized; §7: PR descriptions cite it).
/// Each is a finding the campaign recorded along the way — never a
/// weakened scenario.
const FINDINGS: &[(&str, &str)] = &[
    (
        "The grant_set wedge (product defect, recorded never weakened)",
        "A witness kill inside the grant commit (the grant_set mid-commit windows, \
         §9 row 7) parks the migration PERMANENTLY: the destination's peer-grant op \
         journaled its failure and the ops pipeline replays recorded failures forever — \
         the retry task re-serves the failure at its 5 s tick (a bounded spin, never a \
         silent resume). The park is safe (fail-closed: no dual writer, the source stays \
         fenced) but recovery requires operator action. The campaign records the defect \
         and reuses the park as a deterministic injection window (rows 8 and 12b).",
    ),
    (
        "The IN_DOUBT contract nuance (stage-B round-2 review)",
        "Nearline §6 reserves IN_DOUBT for unresolvable stalls (a failed barrier void, \
         a dead destination VMM) and directs that a resolvable post-authorization stall \
         be reported as the canonical state plus a stall detail. The wedge above parks at \
         destination_authorized with the stall detail — contract-shaped — but the stall \
         never resolves without the operator, which strains the 'resolvable' reading: \
         the record says 'stalled' forever while the migration is permanently parked. \
         Whether a permanently stalled record should surface IN_DOUBT is a recorded \
         contract question (its own plan/PR per §8), not silently decided. Row 12b's \
         failed promote, by contrast, parks IN_DOUBT through the observe mapping of \
         SOURCE_REVOKED ('source revoked; destination grant not yet authorized').",
    ),
    (
        "The row-9 lineage gap (found by this campaign, fixed)",
        "The replica-level lineage gate did not exist before stage C: a target holding \
         foreign data under the right epoch passed prepare and the cut would have \
         delivered it. The row-9 injection found the gap; the fix (in the stage-C base \
         commit) refuses FOREIGN_DEVICE_STATE — the same-id re-drive and the fresh-id \
         re-issue both refuse typed, and the record never reads COMPLETE over foreign \
         data.",
    ),
    (
        "The F1 defense-in-depth note",
        "The lineage gate is one-shot at prepare: the target's replica lineage is \
         verified before the cut and is not re-checked at the barrier. A foreign \
         injection landing after prepare (mid-drive) is caught only by the \
         epoch/fencing disciplines, not by lineage re-verification. A barrier-time \
         re-check is a recorded follow-up (defense in depth), not a shipped guarantee.",
    ),
    (
        "Design property: the live-lease fence is the protection (row 8)",
        "The out-of-band writer's divergence is invisible to the adopt classification \
         BY CONSTRUCTION — volvisor cannot see an actor below its enforcement. The \
         protection is the fence: the live lease the witness holds, which the survivor's \
         adopt refuses UNSAFE over and which keeps the stale source from ever re-entering \
         the data path. The row proves the fence holds; it does not, and cannot, prove \
         the classification sees the divergence.",
    ),
    (
        "Design property: the resume gate is epoch-wide (row 10)",
        "The witness's void enumeration is migration-keyed (an abort voids only its own \
         barrier), but the resume gate is EPOCH-wide: any unvoided barrier of the writer \
         epoch parks another migration's abort with OPERATION_IN_DOUBT ('never a silent \
         resume'). Recovery is the recording holder's void — the one legitimate cleanup \
         path — plus the retry pass. Both levels are pinned by row 10, never weakened.",
    ),
];

/// Build the final report text for the given run directories (§6):
/// the claim-discipline header, the provenance, the §10 coverage
/// matrix (the §9 mapping — Tier S rows with their verdicts, Tier
/// R-only classes with their recorded gates), the §3.2 budget
/// adherence, the recorded findings and the §11 completion-gate
/// checklist. Deterministic: the same directories' records render
/// the same text (no clock, no counters).
#[must_use]
pub fn build_campaign_report(dirs: &[PathBuf]) -> String {
    let campaign = merge(dirs);
    let records = &campaign.records;

    let mut report = String::new();
    report.push_str("# Volvisor aggressive failure campaign — evidence report\n\n");
    // The claim discipline, verbatim (§6) — the report's header.
    report.push_str(
        "> Tier S proves the implemented logic's behavior under the bounded\n\
         > injected fault space (§0/§3.2); it proves nothing about real media,\n\
         > real DRBD, or a real VMM; production support is not claimed.\n\n",
    );
    render_provenance(&mut report, &campaign);
    render_coverage_matrix(&mut report, records);
    render_budgets(&mut report, records);
    render_findings(&mut report);
    render_gates(&mut report, records);
    report.push('\n');
    report
}

/// The provenance: the constituent runs, their commits and their
/// kernels — flagged when mixed, never averaged away (round-1
/// review, NOTE-2: divergent kernels get the same MIXED flag as
/// divergent commits; a report over a stale multi-host tree says
/// so instead of silently showing one kernel).
fn render_provenance(report: &mut String, campaign: &Campaign) {
    let records = &campaign.records;
    let mut commits: Vec<&str> = campaign
        .runs
        .iter()
        .map(|run| run.commit.as_str())
        .collect();
    commits.sort_unstable();
    commits.dedup();
    let tier_r_count = records
        .iter()
        .filter(|record| record["tier"].as_str() == Some("R"))
        .count();
    let commit_cell = if commits.len() == 1 {
        format!("`{}`", commits[0])
    } else {
        format!(
            "MIXED ({})",
            commits
                .iter()
                .map(|commit| format!("`{commit}`"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    let mut kernels: Vec<&str> = records
        .iter()
        .filter_map(|record| record["kernel"].as_str())
        .collect();
    kernels.sort_unstable();
    kernels.dedup();
    let kernel_cell = match kernels.len() {
        0 => "unknown".to_owned(),
        1 => kernels[0].to_owned(),
        _ => format!(
            "MIXED ({})",
            kernels
                .iter()
                .map(|kernel| format!("`{kernel}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };
    line!(
        report,
        "- Runs: {}\n- Commit: {}\n- Kernel: {}\n- Records: {} (Tier S: {}, Tier R: {})",
        campaign
            .runs
            .iter()
            .map(|run| format!("`{}`", run.name))
            .collect::<Vec<_>>()
            .join(", "),
        commit_cell,
        kernel_cell,
        records.len(),
        records.len() - tier_r_count,
        tier_r_count,
    );
}

/// The coverage matrix (§6: fault class × scenario × verdict): the
/// §9 Tier S rows with their verdicts, then every Tier R-only class
/// with its recorded gate (never a hole).
fn render_coverage_matrix(report: &mut String, records: &[Value]) {
    report.push_str("\n## Coverage matrix (nearline §10 classes → §9 rows)\n\n");
    report.push_str(
        "| Row | Tier | Family (§9) | Nearline §10 class | Records | Verdict | Max record |\n",
    );
    report.push_str("|---|---|---|---|---|---|---|\n");
    for spec in ROWS {
        let expected = expected_names(spec);
        let verdict = row_verdict(spec, records);
        let (records_cell, verdict_cell, duration_cell) = match verdict {
            RowVerdict::Covered {
                count,
                outcome,
                duration_ms,
            } => (count.to_string(), outcome, format!("{duration_ms} ms")),
            RowVerdict::Missing(absent) => (
                format!("{}/{}", expected.len() - absent.len(), expected.len()),
                format!("**MISSING**: {}", absent.join(", ")),
                "—".to_owned(),
            ),
            RowVerdict::Unfinished(names) => (
                format!("{}/{}", expected.len(), expected.len()),
                format!("**UNFINISHED** (no outcome): {}", names.join(", ")),
                "—".to_owned(),
            ),
        };
        line!(
            report,
            "| {} | S | {} | {} | {} | {} | {} |",
            spec.row,
            spec.family,
            spec.class,
            records_cell,
            verdict_cell,
            duration_cell,
        );
    }
    for scenario in tier_r::SCENARIOS {
        let name = format!("tier-r/{}", scenario.name);
        let (verdict_cell, duration_cell) = match record_for(records, &name) {
            Some(record) => (
                format!(
                    "{}: {}",
                    record["outcome"].as_str().unwrap_or("?"),
                    record["reason"].as_str().unwrap_or("?")
                ),
                record["duration_ms"]
                    .as_u64()
                    .map_or_else(|| "—".to_owned(), |ms| format!("{ms} ms")),
            ),
            None => (
                "**MISSING** (no gate record — a hole, not a gate)".to_owned(),
                "—".to_owned(),
            ),
        };
        line!(
            report,
            "| — | R | {} | {} | `{}` | {} | {} |",
            scenario.family,
            scenario.class,
            name,
            verdict_cell,
            duration_cell,
        );
    }
}

/// Budget adherence (§3.2's stated bounds only — the plan bounds the
/// kill families, the oracle families, the storm and the suite; the
/// injection rows carry no stated bound).
fn render_budgets(report: &mut String, records: &[Value]) {
    report.push_str("\n## Budget adherence (§3.2)\n\n");
    report.push_str("| Family / row | Bound | Measured | Verdict |\n|---|---|---|---|\n");
    for line in budget_lines(records) {
        line!(
            report,
            "| {} | {} ms | {} ms | {} |",
            line.label,
            line.bound_ms,
            line.measured_ms,
            if line.adheres() { "adhere" } else { "**OVER**" },
        );
    }
    let suite_ms = suite_serialized_ms(records);
    line!(
        report,
        "| suite (serialized sum; wall ≤ sum under bounded parallelism) | {} ms | {} ms | {} |",
        SUITE_BOUND_MS,
        suite_ms,
        if suite_ms <= SUITE_BOUND_MS {
            "adhere"
        } else {
            "**OVER**"
        },
    );
}

/// The recorded findings (§6): what the campaign found along the
/// way — recorded, never weakened.
fn render_findings(report: &mut String) {
    report.push_str("\n## Recorded findings\n\n");
    for (title, body) in FINDINGS {
        line!(report, "- **{title}**: {body}");
    }
}

/// The completion gates (§11), computed from the records alone.
fn render_gates(report: &mut String, records: &[Value]) {
    report.push_str("\n## Completion gates (§11)\n\n");
    for gate in completion_gates(records) {
        line!(
            report,
            "- **{}**: {} — {}",
            gate.gate,
            if gate.complete {
                "pass"
            } else {
                "**incomplete**"
            },
            gate.detail,
        );
    }
    line!(
        report,
        "CG2/CG3 are record-shape gates: they verify every expected record exists with \
         its canonical fields, budgets and markers — the behavioral verification lives \
         in the scenario tests that emit the records, not in this summary"
    );
}

/// Render one run directory's final-form report and write it to
/// `<dir>/REPORT.md` (the per-finish refresh, §6). Idempotent and
/// deterministic; parallel finishes race benignly (the last write
/// wins and includes every record on disk).
pub fn render_report(dir: &Path) {
    let report = build_campaign_report(&[dir.to_path_buf()]);
    let path = dir.join("REPORT.md");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("evidence dir");
    }
    std::fs::write(&path, report).expect("write report");
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Every §9 row 1-15 appears exactly once in the mapping (the
    /// executable §9 table is complete — CG1's static half).
    #[test]
    fn the_row_mapping_is_complete_and_unique() {
        let mut rows: Vec<u8> = ROWS.iter().map(|spec| spec.row).collect();
        rows.sort_unstable();
        let expected: Vec<u8> = (1..=15).collect();
        assert_eq!(rows, expected, "the §9 rows 1-15 map exactly once each");
    }

    /// No scenario name is claimed by two rows (a record
    /// double-counted would inflate coverage).
    #[test]
    fn no_scenario_is_claimed_twice() {
        let mut names: Vec<String> = ROWS.iter().flat_map(expected_names).collect();
        names.sort();
        let before = names.len();
        names.dedup();
        assert_eq!(
            before,
            names.len(),
            "a scenario name appears in two §9 rows"
        );
    }

    /// The Tier R table's scenario names are unique and never
    /// collide with a Tier S row's records.
    #[test]
    fn the_tier_r_table_is_disjoint() {
        let mut tier_names: Vec<&str> = tier_r::SCENARIOS
            .iter()
            .map(|scenario| scenario.name)
            .collect();
        tier_names.sort_unstable();
        let before = tier_names.len();
        tier_names.dedup();
        assert_eq!(before, tier_names.len(), "a Tier R scenario name repeats");
        for scenario in tier_r::SCENARIOS {
            let name = format!("tier-r/{}", scenario.name);
            assert!(
                ROWS.iter().flat_map(expected_names).all(|row| row != name),
                "the Tier R scenario {name} collides with a Tier S row"
            );
        }
    }

    /// CG5's scan flags a planted production-support mention (the
    /// gate is checkable, not decorative).
    #[test]
    fn cg5_flags_a_planted_claim() {
        let planted = json!({
            "scenario": "planted/claim",
            "tier": "S",
            "outcome": "pass: production support is proven",
        });
        let gates = completion_gates(&[planted]);
        let cg5 = &gates[4];
        assert!(!cg5.complete, "the planted claim must fail CG5");
        assert!(cg5.detail.contains("planted/claim"));
    }

    /// CG5's vocabulary (the comprehensive review's U1): the plan
    /// §11 letter forbids claiming "production support **or
    /// real-host durability**" — EVERY phrase in the vocabulary
    /// fails the gate when planted in a Tier S record, and a Tier R
    /// record naming real hosts is exempt (that is the discipline
    /// working, not violating).
    #[test]
    fn cg5_flags_every_claim_phrase_and_exempts_tier_r() {
        let claims = [
            "pass: production support",
            "pass: real-host durability proven",
            "pass: verified on real drbd media",
            "pass: verified on a real vmm",
            "pass: proven on hardware",
        ];
        for claim in claims {
            let planted = json!({
                "scenario": "planted/vocabulary",
                "tier": "S",
                "outcome": claim,
            });
            let gates = completion_gates(&[planted]);
            assert!(
                !gates[4].complete,
                "the planted claim {claim:?} must fail CG5: {}",
                gates[4].detail
            );
            assert!(
                gates[4].detail.contains("forbidden claim phrase"),
                "the failure is the claim phrase, not the absent rows: {}",
                gates[4].detail
            );
        }

        // The Tier R exemption: a gate record naming real hosts (the
        // scenario names and skip reasons do, by design) is not a
        // claim — only its OUTCOME vocabulary is gated (see the U2
        // test below).
        let honest = json!({
            "scenario": "tier-r/same-families-on-real-hosts",
            "tier": "R",
            "outcome": "skipped",
            "reason": "no real DRBD/CH hardware in this environment",
        });
        let gates = completion_gates(&[honest]);
        assert!(
            !gates[4].detail.contains("forbidden claim phrase"),
            "the Tier R gate statement is exempt from the phrase scan: {}",
            gates[4].detail
        );
    }

    /// CG5's Tier R outcome vocabulary (the comprehensive review's
    /// U2, gate side): a Tier R record is a gate statement — an
    /// outcome outside {skipped, blocked} (a hand-authored "pass"
    /// over the full matrix, for example) fails the gate loudly
    /// instead of rendering as a matrix pass.
    #[test]
    fn cg5_fails_a_tier_r_record_that_claims_a_pass() {
        let fabricated = json!({
            "scenario": "tier-r/same-families-on-real-hosts",
            "tier": "R",
            "outcome": "pass: full §9 matrix against real DRBD 9",
            "reason": "claimed",
            "would_run": "the full §9 matrix",
        });
        let gates = completion_gates(&[fabricated]);
        assert!(
            !gates[4].complete,
            "the fabricated Tier R pass must fail CG5: {}",
            gates[4].detail
        );
        assert!(
            gates[4].detail.contains("outside the gate vocabulary"),
            "the failure is the outcome vocabulary: {}",
            gates[4].detail
        );

        // The honest vocabulary passes the clause (blocked, like
        // skipped, is a gate statement).
        let blocked = json!({
            "scenario": "tier-r/same-families-on-real-hosts",
            "tier": "R",
            "outcome": "blocked",
            "reason": "the host cannot run DRBD 9",
        });
        let gates = completion_gates(&[blocked]);
        assert!(
            !gates[4].detail.contains("outside the gate vocabulary"),
            "blocked is a gate statement: {}",
            gates[4].detail
        );
    }

    /// A record-free view is honestly incomplete, never a pass by
    /// absence: CG1 names the gap, CG3 names the cells.
    #[test]
    fn an_empty_campaign_fails_every_relevant_gate() {
        let gates = completion_gates(&[]);
        assert!(!gates[0].complete, "CG1 fails on no records");
        assert!(gates[0].detail.contains("row 1"));
        assert!(!gates[1].complete, "CG2 fails on no records");
        assert!(!gates[2].complete, "CG3 fails on no records");
        assert!(!gates[3].complete, "CG4 fails on no records");
        assert!(
            !gates[4].complete,
            "CG5 fails on no records (the gate is silent)"
        );
    }

    /// CG2 must read THROUGH the array shape (round-1 review,
    /// MINOR-4): a multi-volume record whose oracle is an ARRAY of
    /// per-volume sections is here the SOLE oracle-bearing record,
    /// so every CG2 predicate (the complete prefix, the honest
    /// tail, the data-path boundary, the cross-check) is satisfied
    /// only through the flattening — a regression that ignores
    /// `Value::Array` fails every clause and this test with it.
    #[test]
    fn cg2_reads_every_section_of_an_array_oracle() {
        let record = json!({
            "scenario": "row-12/multi-volume-cut/converges",
            "tier": "S",
            "oracle": [
                {
                    "acknowledged": 10, "verified": 10, "corrupted": 0, "tail": 0,
                    "boundary_source": "data-path", "boundary_skew_ticks": 0
                },
                {
                    "acknowledged": 8, "verified": 8, "corrupted": 0, "tail": 3,
                    "boundary_source": "data-path", "boundary_skew_ticks": 1
                }
            ],
        });
        let gates = completion_gates(&[record]);
        assert!(
            gates[1].complete,
            "CG2 passes over the array alone (every predicate reads a section): {}",
            gates[1].detail
        );
        assert!(
            gates[1]
                .detail
                .contains("2 oracle sections across 1 records"),
            "the flattened section count appears in the detail: {}",
            gates[1].detail
        );
    }

    /// CG2's S6 clause: a section that records a barrier over
    /// acknowledged writes MUST also carry the cross-check — a
    /// barrier without its skew is a gap, never a pass (the
    /// comprehensive review: rows 8-14 and the kill cells hardcoded
    /// `skew: None` and the gate could not see the omission).
    #[test]
    fn cg2_fails_a_barrier_without_its_cross_check() {
        let gapped = json!({
            "scenario": "row-12/multi-volume-cut/converges",
            "tier": "S",
            "oracle": {
                "acknowledged": 27, "verified": 27, "corrupted": 0, "tail": 0,
                "boundary_source": "data-path",
                "barrier_durable_at": 1010,
                "boundary_skew_ticks": null
            },
        });
        let gates = completion_gates(&[gapped]);
        assert!(!gates[1].complete, "the barrier without a skew fails CG2");
        assert!(
            gates[1]
                .detail
                .contains("barrier over acknowledged writes without the boundary cross-check"),
            "the failure is the gap, not the absent rows: {}",
            gates[1].detail
        );

        // The exempt shape: a section with NO acknowledged writes has
        // nothing to cross-check against — the barrier alone is not a
        // gap.
        let empty_journal = json!({
            "scenario": "row-12/multi-volume-cut/converges",
            "tier": "S",
            "oracle": {
                "acknowledged": 0, "verified": 0, "corrupted": 0, "tail": 0,
                "boundary_source": "data-path",
                "barrier_durable_at": 1010,
                "boundary_skew_ticks": null
            },
        });
        let gates = completion_gates(&[empty_journal]);
        assert!(
            !gates[1].detail.contains("without the boundary cross-check"),
            "the empty journal is exempt: {}",
            gates[1].detail
        );
    }

    /// CG2's S1 clause: the cross-check must be LIVE, not a recorded
    /// tautology — a NEGATIVE skew (a barrier stamped before the
    /// last acknowledged write it should have covered) is the §2.3
    /// rule-1 violation and fails the gate. Under the old
    /// frozen-clock design every past-cap record read a mechanical 0
    /// and this clause could never fire.
    #[test]
    fn cg2_fails_a_negative_boundary_skew() {
        let early_barrier = json!({
            "scenario": "row-1/happy-path-cut",
            "tier": "S",
            "oracle": {
                "acknowledged": 10, "verified": 10, "corrupted": 0, "tail": 0,
                "boundary_source": "data-path",
                "barrier_durable_at": 1005,
                "boundary_skew_ticks": -5
            },
        });
        let gates = completion_gates(&[early_barrier]);
        assert!(!gates[1].complete, "the negative skew fails CG2");
        assert!(
            gates[1].detail.contains("(negative boundary skew)"),
            "the failure names the rule-1 violation: {}",
            gates[1].detail
        );
    }

    /// CG1's single-commit certification (the comprehensive review's
    /// U3): the `--all` merge is latest-wins per scenario (a
    /// feature), but the GATES must refuse to certify a winning set
    /// that mixes commits — a scenario the newest run does not
    /// re-emit survives from an older revision, and no gate can
    /// honestly certify over that frankenstein. Mixed commits read
    /// INCOMPLETE with the commits named.
    #[test]
    fn cg1_fails_a_mixed_commit_winning_set() {
        // Two records, two commits: the older one survives the
        // latest-wins merge because the newer run does not re-emit
        // its scenario — exactly the frankenstein shape.
        let older = json!({
            "scenario": "row-2/abort-shaped-lag",
            "tier": "S",
            "commit": "1402a80the-older-revision",
            "outcome": "aborted: the tail is reported",
        });
        let newer = json!({
            "scenario": "row-1/happy-path-cut",
            "tier": "S",
            "commit": "a99390athe-newer-revision",
            "outcome": "complete: the prefix is intact",
        });
        let gates = completion_gates(&[older, newer.clone()]);
        assert!(
            !gates[0].complete,
            "the mixed-commit set must fail CG1: {}",
            gates[0].detail
        );
        assert!(
            gates[0].detail.contains("mix 2 commits"),
            "the failure names the mixed commits: {}",
            gates[0].detail
        );

        // The same records at ONE commit pass the clause (the gate
        // may still be incomplete over the other rows' absence —
        // never over the commit coherence).
        let same = json!({
            "scenario": "row-2/abort-shaped-lag",
            "tier": "S",
            "commit": "a99390athe-newer-revision",
            "outcome": "aborted: the tail is reported",
        });
        let gates = completion_gates(&[same, newer]);
        assert!(
            !gates[0].detail.contains("mix"),
            "one commit is coherent: {}",
            gates[0].detail
        );
    }

    /// CG4's marker vocabulary is matched against the OUTCOME text
    /// alone (round-1 review, MINOR-3): a record whose invariants
    /// say "pass" (as every record's do) but whose outcome carries
    /// no typed marker must FAIL the gate — the whole-record match
    /// was vacuous.
    #[test]
    fn cg4_reads_the_outcome_text_alone() {
        let vacuous = json!({
            "scenario": "row-9/wrong-lineage-data-at-target",
            "tier": "S",
            "outcome": "delivered: the foreign data crossed",
            "invariants": [{"replay_idempotency": "pass"}],
        });
        let gates = completion_gates(&[vacuous]);
        assert!(!gates[3].complete, "the marker-less outcome fails CG4");
        assert!(
            gates[3]
                .detail
                .contains("carries none of the typed markers"),
            "the failure is the marker check, not just the absent rows: {}",
            gates[3].detail
        );

        // The positive control: the same record with a typed
        // outcome passes the marker check (CG4 may still be
        // incomplete over the other rows' absence — never over this
        // record's vocabulary).
        let typed = json!({
            "scenario": "row-9/wrong-lineage-data-at-target",
            "tier": "S",
            "outcome": "refused: FOREIGN_DEVICE_STATE",
            "invariants": [{"replay_idempotency": "pass"}],
        });
        let gates = completion_gates(&[typed]);
        assert!(
            !gates[3]
                .detail
                .contains("carries none of the typed markers"),
            "the typed outcome satisfies the marker check: {}",
            gates[3].detail
        );
    }

    /// The family budget is measured on the FAMILY wall (round-1
    /// review, MINOR-2): the `matrix/<family>/_family` record's
    /// duration, never the max single-cell duration — a 30-cell
    /// family whose aggregate says 240 ms must measure 240 ms even
    /// when every cell's own record says 20 ms.
    #[test]
    fn the_family_budget_measures_the_family_wall() {
        let family = json!({
            "scenario": "matrix/volume-mutations/_family",
            "tier": "S",
            "outcome": null,
            "duration_ms": 240,
        });
        let mut records = vec![family.clone()];
        for cell in matrix::family_cells(Family::VolumeMutations) {
            let mut cell_record = json!({
                "scenario": cell.scenario(),
                "tier": "S",
                "outcome": "recovered: typed",
                "duration_ms": 20,
                "invariants": [{"fixture": "pass"}],
            });
            cell_record["fault"] = json!({
                "kind": cell.hook.fault_kind(),
                "at": cell.hook.location(),
            });
            records.push(cell_record);
        }
        let lines = budget_lines(&records);
        let volume_mutations = lines
            .iter()
            .find(|line| line.label.starts_with("row 4 "))
            .expect("the volume-mutations budget line");
        assert_eq!(
            volume_mutations.measured_ms, 240,
            "the family wall (240 ms) is the measurement, not the max cell (20 ms)"
        );
        // The fallback: without the aggregate record the max-cell
        // duration keeps the table row (CG3 flags the missing
        // aggregate separately).
        records.retain(|record| record["scenario"] != "matrix/volume-mutations/_family");
        let lines = budget_lines(&records);
        let volume_mutations = lines
            .iter()
            .find(|line| line.label.starts_with("row 4 "))
            .expect("the fallback budget line");
        assert_eq!(
            volume_mutations.measured_ms, 20,
            "the max-cell fallback renders when the aggregate is absent"
        );
    }
}
