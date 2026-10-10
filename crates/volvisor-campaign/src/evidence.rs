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

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Instant;

use serde_json::{Value, json};

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
    fault: Option<(String, String)>,
    oracle: Option<Value>,
    invariants: Vec<(String, String)>,
    outcome: Option<String>,
    started: Instant,
}

impl Evidence {
    /// Begin the record for `scenario` (its name in the run
    /// directory; distinct per scenario).
    #[must_use]
    pub fn new(scenario: &str) -> Self {
        Self {
            scenario: scenario.to_owned(),
            fault: None,
            oracle: None,
            invariants: Vec::new(),
            outcome: None,
            started: Instant::now(),
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
            "tier": "S",
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

/// Collect every scenario record under `dir` (the scenario names
/// nest — `kill-matrix/transfer/after-intent.json` — so the walk is
/// recursive over everything except the `logs` capture).
fn collect_records(dir: &Path, records: &mut Vec<Value>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if entry.file_name() != "logs" {
                collect_records(&path, records);
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

/// Render `REPORT.md` from the run directory's records (§6): the
/// §10 coverage-matrix skeleton for the stage-A rows plus the claim
/// discipline, verbatim, as the header. Idempotent — every finished
/// scenario refreshes it, so the file describes the run as far as it
/// has progressed (parallel finishes race benignly; the last write
/// wins and includes every record on disk).
pub fn render_report(dir: &Path) {
    let mut records = Vec::new();
    collect_records(dir, &mut records);
    records.sort_by(|left, right| {
        let key = |record: &Value| record["scenario"].as_str().unwrap_or_default().to_owned();
        key(left).cmp(&key(right))
    });

    let mut report = String::new();
    // `write!`/`writeln!` cannot fail on a `String` (its fmt::Write
    // impl is infallible); the discards keep that explicit.
    report.push_str("# Volvisor aggressive failure campaign — evidence report\n\n");
    // The claim discipline, verbatim (§6).
    report.push_str(
        "> Tier S proves the implemented logic's behavior under the bounded\n\
         > injected fault space (§0/§3.2); it proves nothing about real media,\n\
         > real DRBD, or a real VMM; production support is not claimed.\n\n",
    );
    let _ = writeln!(
        report,
        "- Run: `{}`\n- Commit: `{}`\n- Kernel: `{}`\n- Records: {}",
        dir.file_name().map_or_else(
            || "?".to_owned(),
            |name| name.to_string_lossy().into_owned()
        ),
        records
            .first()
            .and_then(|record| record["commit"].as_str())
            .unwrap_or("unknown"),
        records
            .first()
            .and_then(|record| record["kernel"].as_str())
            .unwrap_or("unknown"),
        records.len(),
    );
    report.push_str("\n## Coverage matrix (stage A: §9 rows 1–3)\n\n");
    report.push_str(
        "| Scenario | Fault | Outcome | Acknowledged | Verified | Corrupted | Tail | Duration |\n",
    );
    report.push_str("|---|---|---|---|---|---|---|---|\n");
    for record in &records {
        let fault = record["fault"]["at"]
            .as_str()
            .or_else(|| record["fault"]["kind"].as_str())
            .unwrap_or("—");
        let _ = writeln!(
            report,
            "| {} | {} | {} | {} | {} | {} | {} | {} ms |",
            record["scenario"].as_str().unwrap_or("?"),
            fault,
            record["outcome"].as_str().unwrap_or("?"),
            record["oracle"]["acknowledged"].as_u64().unwrap_or(0),
            record["oracle"]["verified"].as_u64().unwrap_or(0),
            record["oracle"]["corrupted"].as_u64().unwrap_or(0),
            record["oracle"]["tail"].as_u64().unwrap_or(0),
            record["duration_ms"].as_u64().unwrap_or(0),
        );
    }
    report.push_str(
        "\nStage B will extend this matrix with the remaining fault\n\
         families (§9 rows 4+): store-save kills, the generated kill\n\
         matrix, the §5 injections, the abort storm, witness/VMM kills\n\
         and partitions. Stage A's rows prove the oracle, the boundary\n\
         rules and the kill/recovery machinery on the migration\n\
         pipeline's own durable-write boundaries.\n",
    );
    let path = dir.join("REPORT.md");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("evidence dir");
    }
    std::fs::write(&path, report).expect("write report");
}
