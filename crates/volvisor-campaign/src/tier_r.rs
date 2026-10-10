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

use std::path::PathBuf;
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
    drive_with(scenario, env_gate(), detect_real_host())
}

/// The scaffold's drive over an explicit gate and detection result
/// (the testable core of [`drive`]):
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
/// # Panics
///
/// In both `Claimed` arms, with the exact reason — the loud gate.
pub fn drive_with(scenario: &Scenario, gate: Gate, host: Result<RealHost, String>) -> PathBuf {
    match gate {
        Gate::Skipped => {
            let evidence = Evidence::new_tier_r(&format!("tier-r/{}", scenario.name));
            evidence.finish_tier_r("skipped", scenario.skip_reason, scenario.body)
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
                evidence.finish_tier_r(
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
        );
    }

    /// A claimed gate WITH hardware still refuses to pass: the
    /// real-host drive is not implemented, and the scaffold says so
    /// (fabricating a capability is the one thing §0 forbids).
    #[test]
    #[should_panic(expected = "refusing to pass silently")]
    fn a_claimed_gate_with_hardware_refuses_to_fabricate() {
        let scenario = &SCENARIOS[0];
        drive_with(
            scenario,
            Gate::Claimed,
            Ok(RealHost {
                drbd_version: "DRBDADM_BUILTIN".to_owned(),
                ch_remote: "ch-remote".to_owned(),
            }),
        );
    }

    /// The default gate emits the §6 skip-record shape: tier R,
    /// outcome skipped, the canonical reason and the documented
    /// body. Uses a DIFFERENT scenario than the claimed-gate tests:
    /// the records land in the live run directory under the
    /// scenario's name, and parallel unit tests writing the same
    /// file would race (the blocked-record test below writes
    /// `same-families-on-real-hosts`; this one takes `power-cut`).
    #[test]
    fn the_default_gate_emits_the_skip_record() {
        let scenario = &SCENARIOS[1];
        let path = drive_with(
            scenario,
            Gate::Skipped,
            Err("unused in this arm".to_owned()),
        );
        let body = std::fs::read_to_string(&path).expect("the skip record landed");
        let record: serde_json::Value =
            serde_json::from_str(&body).expect("the skip record is JSON");
        assert_eq!(record["tier"], "R");
        assert_eq!(record["outcome"], "skipped");
        assert_eq!(record["reason"], scenario.skip_reason);
        assert_eq!(record["would_run"], scenario.body);
    }
}
