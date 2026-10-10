//! # volvisor-campaign — the aggressive failure campaign harness
//!
//! The P5 plan of record's independent harness
//! (`docs/plans/2026-10-10-p5-aggressive-failure-campaign.md` §4):
//! the two-daemon rig, the write-trace oracle (§2.2), the
//! evidence emitter (§6) and — in `tests/` — the stage-A scenario
//! rows (§9 rows 1–3). This crate is **test-support, never
//! production**: no other crate depends on it, and it must never
//! become one (AGENTS rule 12's evidence discipline starts here).
//!
//! ## The claim discipline (§0/§6, verbatim)
//!
//! Tier S proves the implemented logic's behavior under the bounded
//! injected fault space (§0/§3.2); it proves nothing about real
//! media, real DRBD, or a real VMM; production support is not
//! claimed.
//!
//! In-process fake worlds survive daemon kills *by construction* —
//! media-level durability is Tier R and is stated as such in every
//! evidence record this crate emits.
//!
//! ## The independence disciplines (§4)
//!
//! The rig composes both daemons from the exported constructors
//! exactly as `migration_e2e.rs` does (`wire_migration`,
//! `AppState::new`, `router`, `Journal::open`, the witness state,
//! the testkit fixtures, the shared frozen clock) — the composition
//! boundary is stated honestly in the plan: independence is **not**
//! claimed from construction. It comes from the disciplines the
//! modules below hold:
//!
//! - **HTTP-only driving**: every act goes through the public routes
//!   (admin consumer routes, peer routes with the peer token,
//!   witness routes with host credentials). The campaign never
//!   calls coordinator methods, driver methods or `resolve`
//!   directly; recovery runs through the background retry task and
//!   is polled via the public observation route with bounded waits.
//! - **Bytes as ground truth**: post-state assertions read the
//!   device maps (§2.1) and the observation routes an operator has
//!   — never coordinator internals.
//! - **One request at a time**: scenarios drive one request at a
//!   time, so the only in-flight request at kill time is the firing
//!   one (§3.3).
//!
//! Injection reaches inside (the crash hooks, the world fail knobs,
//! `write_raw`, the clock); assertion and driving do not. The split
//! is the harness's contract, tested by construction: this crate
//! imports no `volvisord` internals beyond the exported
//! constructors and the migration handle's public surface.
//!
//! ## Conventions
//!
//! This is a test-support crate (the `volvisor-drbd-testkit`
//! precedent): `expect`/`unwrap` are allowed crate-wide, every
//! public item carries its plan citation, and all waits are
//! bounded (real timeouts, small steps — no unbounded polls, no
//! sleeps as synchronization).

// Test-support crate (the `volvisor-drbd-testkit` precedent):
// invariant assertions may expect/unwrap, and every helper that
// stops a scenario on a violated invariant documents that in its
// `# Panics` section (the lint is silenced crate-wide because the
// crate exists only inside test binaries — its panics ARE the
// assertions). The `clippy::panic` allow covers the handful of
// helpers that abort with formatted context (a scenario whose rig
// broke is a failed test, never a production failure path).
#![allow(clippy::expect_used, clippy::unwrap_used)]
#![allow(clippy::missing_panics_doc, clippy::must_use_candidate)]
#![allow(clippy::panic)] // scenario-abort assertions (see above)

pub mod evidence;
pub mod oracle;
pub mod rig;

pub use evidence::{Evidence, LogSources, render_report, run_dir};
pub use oracle::{AckedWrite, Verdict, WriterHandle, verify_against};
pub use rig::{Daemon, Reply, Rig, campaign_rig};
