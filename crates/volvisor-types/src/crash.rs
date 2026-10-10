//! The store-save crash hook (P5 plan §3.1, stage B): kills inside
//! the durable **store saves** — the "meta commit" windows the
//! nearline contract's SIGKILL-during-WAL/meta-commit class means.
//!
//! Two save shapes exist in the workspace, and this seam covers
//! both:
//!
//! - **Atomic file saves** (`tmp` write → fsync → rename → directory
//!   fsync): the migration record store (`volvisor-handoff`) and the
//!   DRBD provider state (`volvisor-drbd`). A crash between the tmp
//!   write and the rename leaves the **old** durable state in place
//!   plus a leftover `.tmp`; a crash after the rename leaves the
//!   **new** state. The recovery under test must handle exactly
//!   that distinction.
//! - **The witness journal's save** (`volvisor-witness`): the
//!   append-only intent/outcome pair of one journaled mutation. A
//!   crash after the intent append (before the apply and outcome)
//!   leaves a mutation whose replay re-derives everything; a crash
//!   after the in-memory apply (before the durable outcome) leaves
//!   the W3b in-flight window the witness's startup roll-forward
//!   completes.
//!
//! # Trust class (doc-gated, the `volvisor-api` `crash` precedent)
//!
//! The seam is **test-gated and inert by default**:
//!
//! - every store constructs its own hook instance **empty**, and no
//!   route, request body, config value or input path can reach it;
//! - only the constructing test rig — the sole caller of
//!   [`StoreCrashHooks::arm`] and [`StoreCrashHooks::set_kill_switch`]
//!   — can change that (the stores expose their instance through
//!   read-only accessors so a rig can register its kill switch, the
//!   same discipline as the journal-append hook in
//!   `volvisor-api`);
//! - [`StoreCrashHooks::consult`] is `pub` only because the save
//!   sites live in other crates; without an armed entry matching
//!   the exact `(store, point)` pair it does nothing.
//!
//! # The kill model at a save site
//!
//! Unlike the journal-append hook (which fires between journal
//! writes, outside every critical section), a save-site consult
//! fires **inside** the store's lock — the provider's state lock or
//! the coordinator's store mutex — by design: a real process death
//! lands wherever the code is, lock or not. The panic poisons that
//! lock; the killed daemon is dead as a group (the rig's task-group
//! model, §3.3), and the rig's **restart constructs the whole
//! daemon fresh from the durable artifacts** — which is the honest
//! restart shape a real process follows, and the reason the rig
//! re-loads the provider instead of reusing the poisoned instance.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;

/// The prefix every crash-injection panic payload starts with. The
/// canonical constant lives here so the file-store seam, the witness
/// seam and the journal-append seam (`volvisor-api`, which re-exports
/// this) mark their payloads identically — a campaign test binary
/// filters its panic hook on this so injected kills stay silent
/// while real panics print normally.
pub const CRASH_PANIC_PREFIX: &str = "volvisor-campaign-crash:";

/// The kill switch the constructing rig registers: fired inside the
/// armed point, before the saving task dies. Must be safe to call
/// from within the saving task — it runs **inside** the store's lock
/// (see the module docs) — and must not await: aborts are requests,
/// and the supervisor owns the awaiting.
pub type KillSwitch = Arc<dyn Fn() + Send + Sync>;

/// The store-identity half of an armed key. One instance of
/// [`StoreCrashHooks`] can serve several stores of one daemon (or
/// witness); the store id targets the arm at exactly one save site.
///
/// The witness variant is keyed additionally by mutation kind at
/// [`StoreCrashHooks::arm_witness`] — the witness journals renewals
/// on a timer, so a bare "next commit" arm would be
/// non-deterministic; arming `(mutation kind, point)` targets the
/// scenario's own mutation.
pub const STORE_MIGRATION_RECORDS: &str = "migration_records";
/// See [`STORE_MIGRATION_RECORDS`].
pub const STORE_DRBD_STATE: &str = "drbd_state";
/// See [`STORE_MIGRATION_RECORDS`].
pub const STORE_WITNESS_COMMIT: &str = "witness_commit";

/// One deterministic kill point inside a durable save (P5 plan
/// §3.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreSavePoint {
    /// After the `tmp` file's content write, before its fsync — the
    /// old durable state remains, a torn `.tmp` is residue.
    AfterTmpWrite,
    /// After the `tmp` file's fsync, before the rename — the tmp is
    /// durable but unreferenced; the old durable state remains the
    /// state.
    AfterFsyncBeforeRename,
    /// After the rename, before the directory fsync — the **new**
    /// state is what a reload sees (on the filesystems the campaign
    /// runs on, a completed rename is the commit).
    AfterRename,
    /// Witness variant: after the mutation's durable intent append,
    /// before the in-memory apply — the replay re-derives the whole
    /// mutation from the journal.
    WitnessAfterIntentAppend,
    /// Witness variant: after the in-memory apply, before the
    /// durable outcome append — the W3b in-flight window the
    /// witness's startup roll-forward completes on restart.
    WitnessAfterApplyBeforeOutcome,
}

/// The armed table for the store saves of one daemon or witness
/// (P5 plan §3.1): `(store id, save point)` pairs, one shot each —
/// the firing save consumes its entry. Inert until the constructing
/// rig arms an entry and registers a kill switch.
///
/// The armed key must resolve to exactly one save site at runtime:
/// store ids are per-component (`STORE_DRBD_STATE` names the one
/// state file a daemon's provider owns), so a two-daemon world arms
/// each daemon's own instance.
pub struct StoreCrashHooks {
    /// The armed entries, keyed by store id (one per store, consumed
    /// on fire).
    armed: Mutex<BTreeMap<&'static str, StoreSavePoint>>,
    /// The mutation kind a `STORE_WITNESS_COMMIT` arm targets (set
    /// by [`StoreCrashHooks::arm_witness`]).
    witness_arm: Mutex<&'static str>,
    /// The rig's group-abort action, when registered.
    kill: Mutex<Option<KillSwitch>>,
}

impl std::fmt::Debug for StoreCrashHooks {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The kill switch is opaque plumbing; everything observable
        // is the armed table's shape.
        formatter
            .debug_struct("StoreCrashHooks")
            .field("armed", &*self.lock_armed())
            .field("witness_arm", &*self.lock_witness_arm())
            .finish_non_exhaustive()
    }
}

impl StoreCrashHooks {
    /// An inert table: nothing armed, no kill switch. This is the
    /// only state any non-rig constructor can observe.
    #[must_use]
    pub fn new() -> Self {
        Self {
            armed: Mutex::new(BTreeMap::new()),
            witness_arm: Mutex::new(""),
            kill: Mutex::new(None),
        }
    }

    /// Arm `store`'s next save to die at `point` (one shot — the
    /// firing save consumes the entry). **Rig only** (the doc-gated
    /// trust class in the module docs).
    pub fn arm(&self, store: &'static str, point: StoreSavePoint) {
        self.lock_armed().insert(store, point);
    }

    /// Drop every armed entry without firing (scenario teardown).
    /// **Rig only.**
    pub fn clear(&self) {
        self.lock_armed().clear();
    }

    /// Register the kill switch fired inside an armed point (the
    /// supervisor's group abort). **Rig only.**
    pub fn set_kill_switch(&self, kill: KillSwitch) {
        *self.lock_kill() = Some(kill);
    }

    /// Consult the table at one save boundary of `store`. Inert
    /// unless the rig armed exactly this `(store, point)` pair — a
    /// differently-pointed entry stays armed for its own point.
    /// When armed: consume the entry, fire the kill switch, then
    /// terminate the saving task mid-save (see the module docs for
    /// why that panic is the injection's semantics and runs inside
    /// the store's lock by design).
    #[allow(clippy::panic)] // the deliberate crash-injection point (module docs)
    pub fn consult(&self, store: &'static str, point: StoreSavePoint) {
        let fired = {
            let mut armed = self.lock_armed();
            if armed.get(store) == Some(&point) {
                armed.remove(store);
                true
            } else {
                false
            }
        };
        if !fired {
            return;
        }
        if let Some(kill) = self.lock_kill().as_ref() {
            kill();
        }
        // The in-band process death: unwind the saving task so the
        // half-finished save never completes, its locks unwind with
        // it, and the durable artifacts stay exactly as the crash
        // point left them.
        std::panic::panic_any(format!("{CRASH_PANIC_PREFIX}{store}/{point:?}"));
    }
}

impl StoreCrashHooks {
    /// The witness-commit consultation: the same armed-table
    /// semantics as [`StoreCrashHooks::consult`], with the mutation
    /// kind as a guard — only a commit of `mutation_kind` can fire a
    /// `STORE_WITNESS_COMMIT` arm, because the witness journals
    /// timer-driven renewals too and a bare next-commit arm would
    /// not be deterministic.
    ///
    /// The `clippy::panic` allow is the one deliberate, documented
    /// exception (see [`StoreCrashHooks::consult`]): terminating the
    /// firing task IS the injection's semantics.
    #[allow(clippy::panic)] // the deliberate crash-injection point (see consult)
    pub fn consult_witness(&self, mutation_kind: &str, point: StoreSavePoint) {
        let fired = {
            let mut armed = self.lock_armed();
            if armed.get(STORE_WITNESS_COMMIT) == Some(&point)
                && *self.lock_witness_arm() == mutation_kind
            {
                armed.remove(STORE_WITNESS_COMMIT);
                true
            } else {
                false
            }
        };
        if !fired {
            return;
        }
        if let Some(kill) = self.lock_kill().as_ref() {
            kill();
        }
        std::panic::panic_any(format!(
            "{CRASH_PANIC_PREFIX}witness_commit:{mutation_kind}/{point:?}"
        ));
    }

    /// Arm the witness-commit seam for the next commit of
    /// `mutation_kind` (one shot). **Rig only.**
    pub fn arm_witness(&self, mutation_kind: &'static str, point: StoreSavePoint) {
        *self.lock_witness_arm() = mutation_kind;
        self.arm(STORE_WITNESS_COMMIT, point);
    }
}

/// Lock helpers, recovering from a poison an earlier firing left
/// behind (the consult panics under these locks by design — a
/// poisoned table is a fired table, not a broken one).
impl StoreCrashHooks {
    /// The armed table's lock.
    fn lock_armed(&self) -> MutexGuard<'_, BTreeMap<&'static str, StoreSavePoint>> {
        self.armed.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The witness arm's target-kind lock (same poison recovery).
    fn lock_witness_arm(&self) -> MutexGuard<'_, &'static str> {
        self.witness_arm
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// The kill switch slot's lock (same poison recovery).
    fn lock_kill(&self) -> MutexGuard<'_, Option<KillSwitch>> {
        self.kill.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Default for StoreCrashHooks {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inert_table_never_fires() {
        let hooks = StoreCrashHooks::new();
        // No arm, no switch: every consult is a no-op (this test
        // completing is the assertion — an inert table must be
        // unable to kill anything).
        hooks.consult(STORE_DRBD_STATE, StoreSavePoint::AfterRename);
        hooks.consult_witness("record_barrier", StoreSavePoint::WitnessAfterIntentAppend);
    }

    #[test]
    fn differently_pointed_entry_stays_armed() {
        let hooks = StoreCrashHooks::new();
        hooks.arm(STORE_DRBD_STATE, StoreSavePoint::AfterRename);
        // A consult at another point of the same store must not
        // consume the arm: the entry stays for its own point. (The
        // armed entry is observable only through the firing side
        // effect, so this test registers a kill switch that must
        // stay unfired.)
        let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Arc::clone(&fired);
        hooks.set_kill_switch(Arc::new(move || {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        }));
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            hooks.consult(STORE_DRBD_STATE, StoreSavePoint::AfterTmpWrite);
        }))
        .expect("the mis-pointed consult must not fire");
        assert!(!fired.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn fired_consult_switches_kills_and_marks_the_payload() {
        let hooks = StoreCrashHooks::new();
        hooks.arm(
            STORE_MIGRATION_RECORDS,
            StoreSavePoint::AfterFsyncBeforeRename,
        );
        let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Arc::clone(&fired);
        hooks.set_kill_switch(Arc::new(move || {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        }));
        let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            hooks.consult(
                STORE_MIGRATION_RECORDS,
                StoreSavePoint::AfterFsyncBeforeRename,
            );
        }))
        .expect_err("the armed consult must fire");
        let payload = payload
            .downcast_ref::<String>()
            .expect("the payload is the marked string");
        assert!(
            payload.starts_with(CRASH_PANIC_PREFIX),
            "the payload must carry the filter prefix: {payload}"
        );
        assert!(fired.load(std::sync::atomic::Ordering::SeqCst));
        // One shot: the same consult again must be inert.
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            hooks.consult(
                STORE_MIGRATION_RECORDS,
                StoreSavePoint::AfterFsyncBeforeRename,
            );
        }))
        .expect("the fired entry must be consumed");
    }

    #[test]
    fn witness_arm_guards_on_the_mutation_kind() {
        let hooks = StoreCrashHooks::new();
        hooks.arm_witness("record_barrier", StoreSavePoint::WitnessAfterIntentAppend);
        // A renewal's commit (the timer-driven kind) must not
        // consume a barrier-targeted arm.
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            hooks.consult_witness("renew", StoreSavePoint::WitnessAfterIntentAppend);
        }))
        .expect("the mis-kinded witness consult must not fire");
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            hooks.consult_witness("record_barrier", StoreSavePoint::WitnessAfterIntentAppend);
        }))
        .expect_err("the kinded witness consult must fire");
    }
}
