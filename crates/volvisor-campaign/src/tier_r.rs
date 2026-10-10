//! The Tier R scaffolding (P5 plan §0/§6, §9's Tier R paragraph,
//! stage D): the real-host scenario families — env-gated, never
//! silently absent.
//!
//! §0's split is the module's contract: Tier R (the same families
//! against real DRBD 9 and a real Cloud Hypervisor, plus the
//! real-host-only fault classes) is *scaffolded* by this phase but
//! cannot run in this repository's environment. Every scenario
//! ships as an env-gated test that emits an explicit `skipped`
//! evidence record with its reason — the coverage matrix
//! distinguishes "not run, no hardware" from "not implemented".
//! Setting `VOLVISOR_CAMPAIGN_TIER=R` is a CLAIM the environment
//! must back: the scenarios then run the same fail-closed toolchain
//! detection the production config path uses, and a claimed tier
//! without the real toolchain FAILS LOUDLY (never a silent pass).
//! With the toolchain present the scaffold still cannot drive the
//! real fault — the body documents exactly what it WOULD run (§0's
//! scenario portability) and the scenario fails loudly with the
//! not-implemented record, because passing would fabricate a
//! real-host capability (§0: do not fabricate).
//!
//! Production support is claimed NOWHERE (§0); the Tier R records
//! are the open hardware gate any future claim must close.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::evidence::Evidence;

/// The env gate (§6): `VOLVISOR_CAMPAIGN_TIER=R` claims real-host
/// execution. Unset or any other value is the default skipped
/// gate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gate {
    /// The default: no tier is claimed; scenarios emit their skip
    /// records.
    Skipped,
    /// `VOLVISOR_CAMPAIGN_TIER=R`: real-host execution is claimed —
    /// the environment must back it.
    Claimed,
}

/// Read the env gate.
#[must_use]
pub fn env_gate() -> Gate {
    if std::env::var("VOLVISOR_CAMPAIGN_TIER").as_deref() == Ok("R") {
        Gate::Claimed
    } else {
        Gate::Skipped
    }
}

/// One Tier R scenario's scaffold: what it covers, its skip
/// reason, and the body it would run on a real host (§0's scenario
/// portability — the portability demand is the documented body,
/// never a fabricated run).
pub struct Scenario {
    /// The scenario's short name (the record is `tier-r/<name>`).
    pub name: &'static str,
    /// §9's family label for the row.
    pub family: &'static str,
    /// The nearline §10 / SPEC-0002 class the scenario covers.
    pub class: &'static str,
    /// The canonical skip reason (§6's record shape), when the only
    /// gate is hardware.
    pub skip_reason: &'static str,
    /// What the scenario WOULD run on a real host (documented where
    /// it cannot run — the scaffold's portability contract).
    pub body: &'static str,
}

/// The Tier R scenario table (§9's Tier R paragraph, enumerated):
/// the same families against real DRBD/CH, plus the rows with no
/// Tier S equivalent. [`crate::summary`] realizes this table in the
/// coverage matrix — every entry shows its recorded gate, never a
/// hole.
pub const SCENARIOS: &[Scenario] = &[
    Scenario {
        name: "same-families-on-real-hosts",
        family: "the §9 Tier S families on real hosts",
        class: "the full §9 matrix against real DRBD 9 and a real Cloud Hypervisor",
        skip_reason: "no real DRBD/CH hardware in this environment",
        body: "re-drive the §9 rows 1-14 against a real two-host DRBD 9 pair with a real \
               Cloud Hypervisor VM: the same HTTP-only rig, the same write-trace oracle \
               (a real guest writer over the real device), the same evidence records — \
               component versions recorded from the real toolchain (drbdadm --version, \
               the CH build), faults delivered as real kills (kill -9 of the real \
               daemons), real witness stop/restart and a real replication partition",
    },
    Scenario {
        name: "power-cut",
        family: "host power cut",
        class: "host power cut",
        skip_reason: "no real DRBD/CH hardware in this environment",
        body: "cut power to the source host (a managed PDU, or a VM-level poweroff that \
               skips disk flush) mid-write and mid-cut; on recovery, boot and prove the \
               acknowledged prefix from the witness barrier backward — the durable \
               boundary only real media can test (Tier S worlds survive their daemons' \
               kills by construction)",
    },
    Scenario {
        name: "ssd-loss",
        family: "separate SSD loss",
        class: "separate SSD loss",
        skip_reason: "no real DRBD/CH hardware in this environment",
        body: "remove or fail one host's SSD (detach the real backing device) with the \
               replica live; prove the survivor holds the acknowledged prefix and the \
               failed side never silently resumes — the storage-backplane loss class",
    },
    Scenario {
        name: "media-flush-fua",
        family: "media-level flush/FUA",
        class: "media-level flush/FUA crash consistency (rule 11)",
        skip_reason: "no real DRBD/CH hardware in this environment",
        body: "a real guest issuing flush/FUA writes across the cut window; cut power at \
               the flush boundary and prove the barrier's durable claim byte-exactly on \
               real media — Tier S covers the flush PROTOCOL (the barrier and suspension \
               proofs, B1/B2), the media level is this gate",
    },
    Scenario {
        name: "saturation-multitb",
        family: "saturation and multi-TB scale",
        class: "saturated disk/network, multi-TB seed, growing dirty rate",
        skip_reason: "no real DRBD/CH hardware in this environment",
        body: "a multi-TB seeded replica behind a saturated link with a growing \
               foreground dirty rate; prove the convergence gate never lies (no \
               caught-up claim over the lag) and the abort-before-deadline policy holds \
               under real timing — Tier S models the logic (row 12), not the scale or \
               the wall clock",
    },
    Scenario {
        name: "mirror-leg-loss",
        family: "local mirror leg loss",
        class: "local mirror leg loss / repair / rebuild",
        skip_reason: "no local mirror implementation exists to fault (plan §1) — real \
                      hardware alone does not open this gate; the mirror model and its \
                      Tier R gate are a recorded follow-up requiring a mirror \
                      implementation first",
        body: "fail one leg of a real local mirror under foreground writes; prove \
               repair/rebuild preserves the acknowledged prefix and the steady-state \
               write ordering contract (nearline §4) — nothing to drive until a mirror \
               implementation exists",
    },
    Scenario {
        name: "durable-dirty-bitmap-meta",
        family: "the durable dirty-bitmap/meta boundary",
        class: "SIGKILL during durable dirty bitmap / peer-stream meta",
        skip_reason: "no real DRBD bitmap persistence in this environment (the \
                      prototype persists no bitmap — plan §1 records the durable \
                      boundary as Tier R; Tier S covers kills during an in-flight \
                      resync, row 12)",
        body: "kill the real replication stack inside its durable bitmap/meta writes \
               (peer stream reconnect, bitmap churn); on restart prove resync resumes \
               from the durable bitmap and no acknowledged write is re-applied as new \
               or lost",
    },
    Scenario {
        name: "kill-timing-races",
        family: "real kill-timing races",
        class: "concurrent requests at kill time, scheduling interleavings",
        skip_reason: "no real DRBD/CH hardware in this environment",
        body: "kill the daemons at real scheduling interleavings — concurrent in-flight \
               requests at kill time (the Tier S model drives one request at a time, \
               §3.3), CPU contention, timer skew — and prove the invariant set holds or \
               fails typed; the harness's deterministic kill points are Tier S's \
               bounded fault space, the real races are this gate",
    },
];

/// The real-host environment the gate demands (the detection's
/// honest result — the versions are recorded, never guessed).
pub struct RealHost {
    /// `drbdadm --version`'s first line.
    pub drbd_version: String,
    /// The detected VMM-side binary.
    pub ch_remote: String,
}

/// The fail-closed real-host detection: the SAME toolchain checks
/// the production config path runs at startup
/// (`DrbdProvider::verify_startup`'s order — the toolchain first,
/// then the kernel module) plus the real VMM side. Returns the
/// first failed check as its `Err` (the detection is itself
/// fail-closed: an unknown environment is not a real host).
pub fn detect_real_host() -> Result<RealHost, String> {
    let output = Command::new("drbdadm")
        .arg("--version")
        .output()
        .map_err(|error| {
            format!("drbdadm is not executable ({error}): is drbd-utils installed?")
        })?;
    if !output.status.success() {
        return Err(format!(
            "drbdadm --version failed ({}): is drbd-utils installed?",
            output.status
        ));
    }
    let drbd_version = String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .unwrap_or("unknown")
        .trim()
        .to_owned();
    std::fs::read_to_string("/proc/drbd").map_err(|error| {
        format!("cannot read /proc/drbd ({error}): is the DRBD kernel module loaded?")
    })?;
    // The VMM side: the binary's presence (the `ch-remote` adapter
    // drives it; a spawn failure is an absent toolchain, any
    // answer is a present one — the version pin is Tier R's own
    // first act once the gate opens).
    Command::new("ch-remote")
        .arg("--version")
        .output()
        .map_err(|error| {
            format!("ch-remote is not executable ({error}): is cloud-hypervisor installed?")
        })?;
    Ok(RealHost {
        drbd_version,
        ch_remote: "ch-remote".to_owned(),
    })
}

/// Drive one Tier R scenario's scaffold (§0/§6). The default gate
/// emits the explicit `skipped` record (the matrix shows the gate,
/// never a hole) and returns its path. A claimed gate fails loudly
/// (see [`drive_with`]); this function reads the env gate and the
/// live detection, so a test binary calling it in the default
/// environment passes WITH its skip record, and setting
/// `VOLVISOR_CAMPAIGN_TIER=R` turns every caller red until the
/// environment — and the scaffold — can back the claim.
///
/// # Panics
///
/// When the tier is claimed and either the environment lacks the
/// real toolchain or the real-host drive is not implemented (see
/// [`drive_with`] — both panics are the loud gate, never a silent
/// pass).
pub fn drive(scenario: &Scenario) -> PathBuf {
    let out_dir = crate::evidence::run_dir();
    let path = drive_with(scenario, env_gate(), detect_real_host(), &out_dir);
    // The live path refreshes the run's `REPORT.md` like any
    // finish (§6) — the staging path does not (a staging directory
    // is not a run directory). In the claimed arms this line is
    // unreachable: the loud refusal above IS the gate.
    crate::evidence::render_report(&out_dir);
    path
}

/// The scaffold's drive over an explicit gate, detection result and
/// output directory (the testable core of [`drive`]):
///
/// - `Skipped` → the skip record (§6's canonical shape, plus the
///   documented body — the record is the gate statement).
/// - `Claimed` with a failed detection → panic: the environment
///   did not back the claim.
/// - `Claimed` with the toolchain present → the `blocked` record
///   (the scaffold detected the real host but the drive is the
///   recorded follow-up), then panic: passing would fabricate a
///   real-host capability (§0).
///
/// `out_dir` is the write target: [`drive`] passes the LIVE run
/// directory (§6's shippable tree); the unit tests pass a STAGING
/// directory — a constructed-host record must never reach the live
/// tree (round-1 review, MAJOR-1; the guard lives at the write
/// choke point, the private `Evidence::write_tier_r_record` —
/// round-2 R2-MINOR-1 — so it holds for ANY seam that targets the
/// live run directory, including this function called with
/// `&run_dir()`; the staging seam [`Evidence::finish_tier_r_at`]
/// skips the report refresh).
///
/// # Panics
///
/// In both `Claimed` arms, with the exact reason — the loud gate.
pub fn drive_with(
    scenario: &Scenario,
    gate: Gate,
    host: Result<RealHost, String>,
    out_dir: &Path,
) -> PathBuf {
    match gate {
        Gate::Skipped => {
            let evidence = Evidence::new_tier_r(&format!("tier-r/{}", scenario.name));
            evidence.finish_tier_r_at(out_dir, "skipped", scenario.skip_reason, scenario.body)
        }
        Gate::Claimed => match host {
            Err(detail) => panic!(
                "VOLVISOR_CAMPAIGN_TIER=R is set but this environment does not back the \
                 claim (scenario tier-r/{}): {detail}",
                scenario.name
            ),
            Ok(host) => {
                // The blocked record lands first — the artifact of
                // the attempt — then the loud refusal (the record
                // alone would read as a pass in the matrix).
                let evidence = Evidence::new_tier_r(&format!("tier-r/{}", scenario.name));
                evidence.finish_tier_r_at(
                    out_dir,
                    "blocked",
                    &format!(
                        "real-host drive not implemented: the scaffold detected the \
                         real toolchain (drbdadm {}, {}) but the scenario body is the \
                         recorded follow-up (plan §0: scaffolded, cannot run)",
                        host.drbd_version, host.ch_remote
                    ),
                    scenario.body,
                );
                panic!(
                    "VOLVISOR_CAMPAIGN_TIER=R is set and the real toolchain answered \
                     (drbdadm {}, {}), but the tier-r/{} scenario body is not implemented \
                     (plan §0: the scaffold records what it would run — {}) — refusing to \
                     pass silently: fabricating a real-host capability is the one thing \
                     §0 forbids",
                    host.drbd_version, host.ch_remote, scenario.name, scenario.body
                );
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A staging directory for the scaffold's unit-test records —
    /// NEVER the live run directory (round-1 review, MAJOR-1): the
    /// claimed-gate arms construct a host, and a constructed-host
    /// record in the evidence tree would be a fabricated
    /// environmental claim no gate catches.
    fn staging_dir() -> PathBuf {
        volvisor_drbd_testkit::leak_tempdir()
    }

    /// A claimed gate without hardware fails loudly with the
    /// detection's own detail (never a silent pass).
    #[test]
    #[should_panic(expected = "does not back the claim")]
    fn a_claimed_gate_without_hardware_fails_loudly() {
        let scenario = &SCENARIOS[0];
        drive_with(
            scenario,
            Gate::Claimed,
            Err("drbdadm is not executable".to_owned()),
            &staging_dir(),
        );
    }

    /// A claimed gate WITH hardware still refuses to pass: the
    /// real-host drive is not implemented, and the scaffold says so
    /// (fabricating a capability is the one thing §0 forbids). The
    /// constructed-host `blocked` record persists to a STAGING
    /// directory — the real refusal path (record shape + panic) is
    /// exercised without ever writing a synthetic-host claim into
    /// the live evidence tree (round-1 review, MAJOR-1).
    #[test]
    #[should_panic(expected = "refusing to pass silently")]
    fn a_claimed_gate_with_hardware_refuses_to_fabricate() {
        let scenario = &SCENARIOS[0];
        let staging = staging_dir();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drive_with(
                scenario,
                Gate::Claimed,
                Ok(RealHost {
                    drbd_version: "DRBDADM_BUILTIN".to_owned(),
                    ch_remote: "ch-remote".to_owned(),
                }),
                &staging,
            );
        }));
        // The refusal panicked (the loud gate) AND the blocked
        // record landed in the staging directory with the §6 shape
        // — the record exists, carries the constructed toolchain,
        // and never touched run_dir().
        assert!(result.is_err(), "the claimed-with-hardware arm panics");
        let path = staging.join(format!("tier-r/{}.json", scenario.name));
        let body = std::fs::read_to_string(&path).expect("the blocked record landed in staging");
        let record: serde_json::Value =
            serde_json::from_str(&body).expect("the blocked record is JSON");
        assert_eq!(record["outcome"], "blocked");
        assert_eq!(record["tier"], "R");
        assert!(
            record["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("DRBDADM_BUILTIN")),
            "the staged record carries the constructed toolchain"
        );
        // Resume the refusal so #[should_panic] sees it.
        std::panic::resume_unwind(result.expect_err("the panic payload"));
    }

    /// The default gate emits the §6 skip-record shape: tier R,
    /// outcome skipped, the canonical reason and the documented
    /// body — persisted to a staging directory like every unit
    /// test of the scaffold.
    #[test]
    fn the_default_gate_emits_the_skip_record() {
        let scenario = &SCENARIOS[1];
        let staging = staging_dir();
        let path = drive_with(
            scenario,
            Gate::Skipped,
            Err("unused in this arm".to_owned()),
            &staging,
        );
        let body = std::fs::read_to_string(&path).expect("the skip record landed");
        let record: serde_json::Value =
            serde_json::from_str(&body).expect("the skip record is JSON");
        assert_eq!(record["tier"], "R");
        assert_eq!(record["outcome"], "skipped");
        assert_eq!(record["reason"], scenario.skip_reason);
        assert_eq!(record["would_run"], scenario.body);
    }

    /// The live seam's defense-in-depth guard (round-1 review,
    /// MAJOR-1, hoisted to the write choke point in round 2): a
    /// Tier R record carrying a synthetic-host marker must NEVER be
    /// writable into the live run directory — the guard fires
    /// before any write, whichever seam the caller took.
    #[test]
    #[should_panic(expected = "must never be written into the live evidence tree")]
    fn the_live_seam_refuses_synthetic_host_records() {
        Evidence::new_tier_r("tier-r/guard-probe").finish_tier_r(
            "blocked",
            "the scaffold detected the real toolchain (drbdadm DRBDADM_BUILTIN, ch-remote)",
            "the documented body",
        );
    }

    /// The staging seam accepts the same record the live seam
    /// refuses — the unit tests' constructed-host records have
    /// somewhere to land without touching the evidence tree.
    #[test]
    fn the_staging_seam_accepts_what_the_live_seam_refuses() {
        let staging = staging_dir();
        let path = Evidence::new_tier_r("tier-r/staging-probe").finish_tier_r_at(
            &staging,
            "blocked",
            "the scaffold detected the real toolchain (drbdadm DRBDADM_BUILTIN, ch-remote)",
            "the documented body",
        );
        assert!(path.exists(), "the staged record landed");
    }

    /// The OUTCOME VOCABULARY (the comprehensive review's U2): a
    /// Tier R record IS a gate statement — skipped or blocked,
    /// never a pass. A "pass" outcome is refused at the write for
    /// BOTH seams (it would render as a matrix pass in the summary;
    /// the gate layer rejects it again on read — defense in depth).
    #[test]
    #[should_panic(expected = "a gate statement (skipped/blocked)")]
    fn a_tier_r_record_cannot_claim_a_pass() {
        let staging = staging_dir();
        Evidence::new_tier_r("tier-r/pass-probe").finish_tier_r_at(
            &staging,
            "pass",
            "a fabricated claim",
            "the documented body",
        );
    }

    /// The CHOKE-POINT guard (round-2 R2-MINOR-1): the natural
    /// fabrication regression — `drive_with` aimed at the LIVE run
    /// directory with a constructed host, bypassing `finish_tier_r`
    /// entirely — must be refused before any byte is written. The
    /// guard's placement is the fix: on the finisher it was
    /// dead-code discipline; on the write path it holds for every
    /// seam.
    #[test]
    #[should_panic(expected = "must never be written into the live evidence tree")]
    fn the_choke_point_refuses_a_constructed_host_through_drive_with() {
        drive_with(
            &SCENARIOS[0],
            Gate::Claimed,
            Ok(RealHost {
                drbd_version: "DRBDADM_BUILTIN".to_owned(),
                ch_remote: "ch-remote".to_owned(),
            }),
            &crate::evidence::run_dir(),
        );
    }
}
