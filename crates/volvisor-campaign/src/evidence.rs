//! The evidence emitter (P5 plan §6): every scenario emits a JSON
//! record under `target/campaign-evidence/<run-id>/` (git-ignored),
//! with per-scenario log capture — the daemon journals, the
//! migration records and the witness journal copied at scenario end
//! (nearline §10's "full logs"; the record references them by
//! path) — and [`render_report`] writes `REPORT.md`, the campaign's
//! shippable artifact, from the run directory's records.
//!
//! ## The claim discipline (§0/§6, verbatim)
//!
//! Tier S proves the implemented logic's behavior under the bounded
//! injected fault space (§0/§3.2); it proves nothing about real
//! media, real DRBD, or a real VMM; production support is not
//! claimed. Every record and the report carry it.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Instant;

use serde_json::{Value, json};

/// The synthetic-host markers the LIVE Tier R seam refuses (round-1
/// review, MAJOR-1 — defense in depth): strings a constructed
/// [`crate::tier_r::RealHost`] carries in the unit tests. A record
/// mentioning one is a fabricated environmental claim, and the
/// evidence tree (the shippable artifact) must never hold one —
/// the scaffold's unit tests persist their claimed-gate records to
/// a staging directory ([`Evidence::finish_tier_r_at`]), never to
/// `run_dir()`.
const SYNTHETIC_HOST_MARKERS: &[&str] = &["DRBDADM_BUILTIN"];

/// The scenario record's log-source directories (copied, never
/// moved — the rig keeps serving from the originals).
pub struct LogSources<'a> {
    /// The source daemon's journal directory (`journal.log`,
    /// `migrations/`, `peer-preparations/`).
    pub a_journal: &'a Path,
    /// The destination daemon's journal directory.
    pub b_journal: &'a Path,
    /// The witness's durable directory.
    pub witness: &'a Path,
}

/// The workspace root (the crate lives one level under `crates/`).
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .to_path_buf()
}

/// The run id: one per test binary invocation (parallel scenarios
/// write distinct record files into the same run directory).
fn run_id() -> &'static str {
    static RUN: OnceLock<String> = OnceLock::new();
    RUN.get_or_init(|| format!("run-{}", std::process::id()))
}

/// This run's evidence directory: `target/campaign-evidence/<run>`
/// under the workspace root (the `/target` gitignore covers it).
#[must_use]
pub fn run_dir() -> PathBuf {
    workspace_root()
        .join("target")
        .join("campaign-evidence")
        .join(run_id())
}

/// The workspace's git commit, read hermetically from `.git`
/// (`.git/HEAD` → the ref file it names; `unknown` when packed,
/// detached or absent — no `git` subprocess, no network).
fn git_commit() -> String {
    let git = workspace_root().join(".git");
    let head = std::fs::read_to_string(git.join("HEAD")).unwrap_or_default();
    let Some(reference) = head.trim().strip_prefix("ref: ") else {
        let sha = head.trim();
        let looks_like_sha =
            !sha.is_empty() && sha.chars().all(|character| character.is_ascii_hexdigit());
        return if looks_like_sha {
            sha.to_owned()
        } else {
            "unknown".to_owned()
        };
    };
    std::fs::read_to_string(git.join(reference))
        .map_or_else(|_| "unknown".to_owned(), |sha| sha.trim().to_owned())
}

/// The kernel release (`uname -r`'s source; `unknown` off Linux).
fn kernel_release() -> String {
    std::fs::read_to_string("/proc/sys/kernel/osrelease").map_or_else(
        |_| "unknown".to_owned(),
        |release| release.trim().to_owned(),
    )
}

/// Recursively copy `from` into `to` (best effort with explicit
/// failure — a missing log source is a rig bug, not a skip).
fn copy_tree(from: &Path, to: &Path) {
    let metadata = std::fs::metadata(from)
        .unwrap_or_else(|error| panic!("evidence source {} is missing: {error}", from.display()));
    if metadata.is_file() {
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent).expect("evidence dir");
        }
        std::fs::copy(from, to).expect("copy evidence file");
        return;
    }
    for entry in std::fs::read_dir(from).expect("read evidence dir") {
        let entry = entry.expect("evidence entry");
        copy_tree(&entry.path(), &to.join(entry.file_name()));
    }
}

/// One scenario's evidence record, built through the scenario and
/// finished with the log capture (§6's schema).
pub struct Evidence {
    scenario: String,
    /// The record's tier (§0): `S` for the simulation rows, `R` for
    /// the env-gated real-host scaffolds (whose records are gate
    /// statements, not observations — see [`Self::finish_tier_r`]).
    tier: &'static str,
    fault: Option<(String, String)>,
    oracle: Option<Value>,
    invariants: Vec<(String, String)>,
    outcome: Option<String>,
    started: Instant,
}

impl Evidence {
    /// Begin the record for `scenario` (its name in the run
    /// directory; distinct per scenario). Tier S (§0) — the default
    /// tier of every driven scenario.
    #[must_use]
    pub fn new(scenario: &str) -> Self {
        Self {
            scenario: scenario.to_owned(),
            tier: "S",
            fault: None,
            oracle: None,
            invariants: Vec::new(),
            outcome: None,
            started: Instant::now(),
        }
    }

    /// Begin a Tier R record for `scenario` (§0/§6): the real-host
    /// scaffolds emit gate statements — explicit `skipped` (or
    /// `blocked`) records with their reasons — never silent
    /// absence. Finished with [`Self::finish_tier_r`] (no rig, no
    /// log capture).
    #[must_use]
    pub fn new_tier_r(scenario: &str) -> Self {
        Self {
            tier: "R",
            ..Self::new(scenario)
        }
    }

    /// The injected fault (§6: kind + the exact armed point).
    pub fn fault(&mut self, kind: &str, at: &str) -> &mut Self {
        self.fault = Some((kind.to_owned(), at.to_owned()));
        self
    }

    /// The oracle section (§6): the honest byte-level verdict. See
    /// [`oracle_value`] for the canonical shape.
    pub fn oracle(&mut self, value: Value) -> &mut Self {
        self.oracle = Some(value);
        self
    }

    /// One invariant verdict (`name` → pass/fail with detail).
    pub fn invariant(&mut self, name: &str, verdict: &str) -> &mut Self {
        self.invariants.push((name.to_owned(), verdict.to_owned()));
        self
    }

    /// The scenario's terminal outcome (e.g. `recovered: COMPLETE`).
    pub fn outcome(&mut self, outcome: &str) -> &mut Self {
        self.outcome = Some(outcome.to_owned());
        self
    }

    /// Finish: copy the log sources under `logs/<scenario>/`, write
    /// the record, refresh `REPORT.md`, and return the record's path.
    pub fn finish(self, sources: &LogSources<'_>) -> PathBuf {
        let dir = run_dir();
        let scenario_logs = dir.join("logs").join(&self.scenario);
        copy_tree(sources.a_journal, &scenario_logs.join("a"));
        copy_tree(sources.b_journal, &scenario_logs.join("b"));
        copy_tree(sources.witness, &scenario_logs.join("witness"));

        let record = json!({
            "scenario": self.scenario,
            "tier": self.tier,
            "commit": git_commit(),
            "kernel": kernel_release(),
            "components": {
                "volvisord": env!("CARGO_PKG_VERSION"),
                "drbd-tooling": "fake",
                "vmm": "fake",
                "witness": "in-process",
            },
            "fault": self.fault.as_ref().map(|(kind, at)| json!({
                "kind": kind,
                "at": at,
            })),
            "oracle": self.oracle,
            "invariants": self.invariants.iter()
                .map(|(name, verdict)| json!({name: verdict}))
                .collect::<Vec<_>>(),
            "logs": {
                "journal": scenario_logs.join("a").join("journal.log"),
                "migration_records": scenario_logs.join("a").join("migrations"),
                "peer_preparations": scenario_logs.join("b").join("peer-preparations"),
                "destination_journal": scenario_logs.join("b").join("journal.log"),
                "witness": scenario_logs.join("witness"),
            },
            "outcome": self.outcome,
            "duration_ms": u64::try_from(self.started.elapsed().as_millis())
                .expect("scenario duration fits a u64"),
        });
        let path = dir.join(format!("{}.json", self.scenario));
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("evidence dir");
        }
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&record).expect("record JSON"),
        )
        .expect("write record");
        render_report(&dir);
        path
    }

    /// Finish a Tier R record WITHOUT a rig (§0/§6): no log capture
    /// (no rig exists to capture from), `components` null (the
    /// record is a gate statement, not an observation of the
    /// environment — claiming component versions for a scenario
    /// that did not run would be fabrication), and the outcome,
    /// reason and documented body carried verbatim. The §6 shapes:
    ///
    /// - `("skipped", reason, body)` — the default gate's explicit
    ///   skip ("not run, no hardware" ≠ "not implemented");
    /// - `("blocked", reason, body)` — the tier was claimed and the
    ///   toolchain answered, but the real-host drive is not
    ///   implemented (the recorded follow-up).
    ///
    /// Writes into the LIVE run directory (§6's shippable tree) and
    /// refreshes the run's `REPORT.md` like any finish. The live
    /// seam carries the defense-in-depth guard: a record whose text
    /// carries a synthetic-host marker (`SYNTHETIC_HOST_MARKERS`)
    /// must never reach the evidence tree — a unit test exercising
    /// the claimed-gate arms persists to a staging directory via
    /// [`Self::finish_tier_r_at`] instead, never here.
    ///
    /// # Panics
    ///
    /// When the record's text carries a synthetic-host marker —
    /// the live tree must never hold a fabricated environmental
    /// claim (round-1 review, MAJOR-1).
    pub fn finish_tier_r(self, outcome: &str, reason: &str, would_run: &str) -> PathBuf {
        for marker in SYNTHETIC_HOST_MARKERS {
            for text in [outcome, reason, would_run] {
                assert!(
                    !text.contains(marker),
                    "a Tier R record carrying the synthetic-host marker {marker:?} must never \
                     reach the live evidence tree (the unit tests persist to a staging \
                     directory via finish_tier_r_at — the live seam is for real environments \
                     only)"
                );
            }
        }
        let dir = run_dir();
        let path = self.write_tier_r_record(&dir, outcome, reason, would_run);
        render_report(&dir);
        path
    }

    /// Finish a Tier R record into an EXPLICIT directory — the
    /// staging path for the scaffold's unit tests (the
    /// claimed-with-hardware arm must exercise the real record
    /// shape without ever writing a constructed-host claim into
    /// the live tree, [`Self::finish_tier_r`]'s guard). No report
    /// refresh: a staging directory is not a run directory.
    pub fn finish_tier_r_at(
        self,
        dir: &Path,
        outcome: &str,
        reason: &str,
        would_run: &str,
    ) -> PathBuf {
        self.write_tier_r_record(dir, outcome, reason, would_run)
    }

    /// The §6 Tier R record writer (shared by the live and staging
    /// seams): the canonical schema with `components` null (a gate
    /// statement, not an observation).
    fn write_tier_r_record(
        &self,
        dir: &Path,
        outcome: &str,
        reason: &str,
        would_run: &str,
    ) -> PathBuf {
        let record = json!({
            "scenario": self.scenario,
            "tier": self.tier,
            "commit": git_commit(),
            "kernel": kernel_release(),
            "components": null,
            "fault": null,
            "oracle": null,
            "invariants": [],
            "logs": null,
            "outcome": outcome,
            "reason": reason,
            "would_run": would_run,
            "duration_ms": u64::try_from(self.started.elapsed().as_millis())
                .expect("scenario duration fits a u64"),
        });
        let path = dir.join(format!("{}.json", self.scenario));
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("evidence dir");
        }
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&record).expect("record JSON"),
        )
        .expect("write record");
        path
    }
}

/// The canonical oracle section (§6): acknowledged count, the
/// byte-verified count at the named side, corruption as the distinct
/// class, the honest tail, the boundary's provenance (always
/// `data-path` — the coordinator's records are never the boundary's
/// source, §2.3 rule 1) and the boundary cross-check's inputs.
#[must_use]
pub fn oracle_value(
    acknowledged: u64,
    verified: u64,
    corrupted: u64,
    tail: u64,
    verified_side: &str,
    boundary_seq: Option<u64>,
    boundary_source: &str,
    stop_reason: &str,
    barrier_at: Option<u64>,
    boundary_skew: Option<i64>,
) -> Value {
    json!({
        "acknowledged": acknowledged,
        "verified": verified,
        "verified_side": verified_side,
        "corrupted": corrupted,
        "tail": tail,
        "boundary_seq": boundary_seq,
        "boundary_source": boundary_source,
        "stop_reason": stop_reason,
        "barrier_durable_at": barrier_at,
        "boundary_skew_ticks": boundary_skew,
    })
}

/// Render `REPORT.md` from one run directory's records (§6): the
/// per-finish refresh, in its final stage-D form — the §10 coverage
/// matrix (the §9 mapping: Tier S rows with their verdicts, Tier
/// R-only classes with their recorded gates), the §3.2 budget
/// adherence, the recorded findings and the §11 completion-gate
/// checklist. See [`crate::summary`] (the builder) and the
/// `campaign-summary` binary (the standalone path). A single run
/// directory holds only its own test binary's records, so absent
/// rows read `MISSING`, never a silent omission; idempotent — every
/// finished scenario refreshes it, and parallel finishes race
/// benignly (the last write wins and includes every record on
/// disk).
pub fn render_report(dir: &Path) {
    crate::summary::render_report(dir);
}
