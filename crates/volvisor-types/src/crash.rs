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
//!   [`StoreCrashHooks::arm`], [`StoreCrashHooks::set_kill_switch`],
//!   [`StoreCrashHooks::arm_save_failure`] and
//!   [`StoreCrashHooks::arm_save_failure_until_cleared`] (the latter two
//!   record a private `SaveFailureArm`) — can change that (the stores
//!   expose their instance through read-only accessors so a rig can
//!   register its kill switch or failure arm, the same discipline as the
//!   journal-append hook in `volvisor-api`). These entry points are
//!   **rig-only and unreachable from configuration or HTTP**; the
//!   persistent variant is `pub` because the rigs live in other crates,
//!   not because it is part of the production surface;
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
/// See [`STORE_MIGRATION_RECORDS`]: the one grow-notification
/// state file a daemon's engine owns (P6-B).
pub const STORE_GROW_NOTIFICATIONS: &str = "grow_notifications";
/// See [`STORE_MIGRATION_RECORDS`]: the LVM provider's durable
/// state file — the move records' commit boundaries (P6-C: the
/// online same-VG move's PREPARING/COPYING/COMPLETE/IN_DOUBT
/// record saves are armed through this seam).
pub const STORE_LVM_STATE: &str = "lvm_state";
/// See [`STORE_MIGRATION_RECORDS`]: the device-ownership record
/// file of the lending surface (P9-A: the lend/reclaim
/// pending-intent journal and the ownership-record effect saves
/// are armed through this seam — the crash window between a
/// journaled intent and its effect is what quarantines the
/// device).
pub const STORE_DEVICE_OWNERSHIP: &str = "device_ownership";
/// See [`STORE_MIGRATION_RECORDS`]: the destination's
/// peer-preparation store — one JSON file per migration (the
/// prepare install, the per-participant device-path recordings,
/// and the GrantSet re-issue attempt counter's bump are armed
/// through this seam; the horizon remedy's durable boundaries).
pub const STORE_PEER_PREPARATION: &str = "peer_preparation";

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
    /// After the rename, before the parent-directory fsync, for a
    /// **non-kill** injected failure at the directory-fsync boundary
    /// (see [`StoreCrashHooks::arm_save_failure`]). The rename has
    /// already landed, so a reload sees the **new** state, but the
    /// save site returns an error rather than panicking, exercising
    /// the caller's error path.
    AfterRenameBeforeDirFsync,
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
    /// The rig-only injected **error** arms, keyed by store id: the
    /// target point, how many matching consults still pass before the
    /// failure fires, and whether the arm is one-shot or repeats until
    /// cleared (see [`StoreCrashHooks::arm_save_failure`] and
    /// [`StoreCrashHooks::arm_save_failure_until_cleared`]).
    failures: Mutex<BTreeMap<&'static str, SaveFailureArm>>,
}

/// One injected save-failure arm: the target boundary, the number of
/// matching consults that still pass, and whether the arm is consumed
/// when it fires.
#[derive(Clone, Copy, Debug)]
struct SaveFailureArm {
    point: StoreSavePoint,
    skip: usize,
    persistent: bool,
}

impl std::fmt::Debug for StoreCrashHooks {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The kill switch is opaque plumbing; everything observable
        // is the armed tables' shape.
        formatter
            .debug_struct("StoreCrashHooks")
            .field("armed", &*self.lock_armed())
            .field("witness_arm", &*self.lock_witness_arm())
            .field("failures", &*self.lock_failures())
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
            failures: Mutex::new(BTreeMap::new()),
        }
    }

    /// Arm `store`'s next save to die at `point` (one shot — the
    /// firing save consumes the entry). **Rig only** (the doc-gated
    /// trust class in the module docs).
    pub fn arm(&self, store: &'static str, point: StoreSavePoint) {
        self.lock_armed().insert(store, point);
    }

    /// Drop every armed entry — kill arms and injected-failure arms —
    /// without firing (scenario teardown). **Rig only.**
    pub fn clear(&self) {
        self.lock_armed().clear();
        self.lock_failures().clear();
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

    /// Arm `store`'s save to return an injected **error** at `point`
    /// after passing `skip` matching consults. **Rig only**, inert by
    /// default (the same trust class as [`StoreCrashHooks::arm`]).
    ///
    /// This is deliberately separate from the kill arm table: it never
    /// fires the kill switch or panics, it makes the save site return
    /// an error, so callers exercise their error paths while the
    /// durable artifacts stay exactly as the boundary left them.
    ///
    /// `skip == 0` fails at the next matching consult; `skip == n`
    /// passes `n` matching consults first. One shot — the firing
    /// consult consumes the entry. A consult at a different point
    /// neither consumes nor decrements the arm.
    ///
    /// Only the LVM provider state save consults this table today
    /// (including the `AfterRenameBeforeDirFsync` point). Arming a
    /// point for a store whose save does not consult it is inert, so a
    /// rig must not treat a non-firing arm as evidence.
    pub fn arm_save_failure(&self, store: &'static str, point: StoreSavePoint, skip: usize) {
        self.lock_failures().insert(
            store,
            SaveFailureArm {
                point,
                skip,
                persistent: false,
            },
        );
    }

    /// Arm `store`'s save to return an injected **error** at `point`
    /// after passing `skip` matching consults, and to keep failing at
    /// that point until [`StoreCrashHooks::clear`] is called. **Rig
    /// only**, inert by default (the same trust class as
    /// [`StoreCrashHooks::arm`]).
    ///
    /// This exists for scenarios that need *every* save in a window to
    /// fail (for example a final save and its compensating clear save),
    /// not just the next one. The durable artifacts stay exactly as the
    /// boundary leaves them, exactly like
    /// [`StoreCrashHooks::arm_save_failure`].
    pub fn arm_save_failure_until_cleared(
        &self,
        store: &'static str,
        point: StoreSavePoint,
        skip: usize,
    ) {
        self.lock_failures().insert(
            store,
            SaveFailureArm {
                point,
                skip,
                persistent: true,
            },
        );
    }

    /// Consult the injected-failure table at one save boundary of
    /// `store`. Returns `true` exactly when an armed `(store, point)`
    /// failure fires; a one-shot entry is consumed, a persistent entry
    /// stays armed.
    ///
    /// A consult at a different point (or of an unarmed store) returns
    /// `false` and leaves the arm untouched. Inert unless the
    /// constructing rig called [`StoreCrashHooks::arm_save_failure`] or
    /// [`StoreCrashHooks::arm_save_failure_until_cleared`] (the
    /// module's doc-gated trust class).
    pub fn take_save_failure(&self, store: &'static str, point: StoreSavePoint) -> bool {
        let mut failures = self.lock_failures();
        let Some(arm) = failures.get_mut(store) else {
            return false;
        };
        if arm.point != point {
            return false;
        }
        if arm.skip > 0 {
            arm.skip -= 1;
            return false;
        }
        if arm.persistent {
            return true;
        }
        failures.remove(store);
        true
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

    /// The injected-failure table's lock (same poison recovery).
    fn lock_failures(&self) -> MutexGuard<'_, BTreeMap<&'static str, SaveFailureArm>> {
        self.failures.lock().unwrap_or_else(PoisonError::into_inner)
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
    fn failure_table_is_inert_by_default() {
        let hooks = StoreCrashHooks::new();
        assert!(
            !hooks.take_save_failure(STORE_LVM_STATE, StoreSavePoint::AfterRenameBeforeDirFsync)
        );
        assert!(
            format!("{hooks:?}").contains("failures"),
            "the debug shape includes the failure table"
        );
    }

    #[test]
    fn save_failure_skip_passes_that_many_matching_consults() {
        let hooks = StoreCrashHooks::new();
        hooks.arm_save_failure(
            STORE_LVM_STATE,
            StoreSavePoint::AfterRenameBeforeDirFsync,
            2,
        );
        assert!(
            !hooks.take_save_failure(STORE_LVM_STATE, StoreSavePoint::AfterRenameBeforeDirFsync)
        );
        assert!(
            !hooks.take_save_failure(STORE_LVM_STATE, StoreSavePoint::AfterRenameBeforeDirFsync)
        );
        assert!(
            hooks.take_save_failure(STORE_LVM_STATE, StoreSavePoint::AfterRenameBeforeDirFsync)
        );
    }

    #[test]
    fn save_failure_is_one_shot() {
        let hooks = StoreCrashHooks::new();
        hooks.arm_save_failure(
            STORE_LVM_STATE,
            StoreSavePoint::AfterRenameBeforeDirFsync,
            0,
        );
        assert!(
            hooks.take_save_failure(STORE_LVM_STATE, StoreSavePoint::AfterRenameBeforeDirFsync)
        );
        assert!(
            !hooks.take_save_failure(STORE_LVM_STATE, StoreSavePoint::AfterRenameBeforeDirFsync),
            "the firing consult consumes the entry"
        );
    }

    #[test]
    fn persistent_save_failure_keeps_firing_until_cleared() {
        let hooks = StoreCrashHooks::new();
        hooks.arm_save_failure_until_cleared(STORE_LVM_STATE, StoreSavePoint::AfterTmpWrite, 1);
        // The skipped consult passes.
        assert!(!hooks.take_save_failure(STORE_LVM_STATE, StoreSavePoint::AfterTmpWrite));
        // Every following matching consult fires, and the arm survives.
        for _ in 0..3 {
            assert!(hooks.take_save_failure(STORE_LVM_STATE, StoreSavePoint::AfterTmpWrite));
        }
        // A mis-pointed consult neither fires nor consumes.
        assert!(!hooks.take_save_failure(STORE_LVM_STATE, StoreSavePoint::AfterRename));
        hooks.clear();
        assert!(!hooks.take_save_failure(STORE_LVM_STATE, StoreSavePoint::AfterTmpWrite));
    }

    #[test]
    fn save_failure_at_a_different_point_neither_fires_nor_decrements() {
        let hooks = StoreCrashHooks::new();
        hooks.arm_save_failure(
            STORE_LVM_STATE,
            StoreSavePoint::AfterRenameBeforeDirFsync,
            1,
        );
        // Any number of mis-pointed consults must not touch the arm.
        for _ in 0..5 {
            assert!(!hooks.take_save_failure(STORE_LVM_STATE, StoreSavePoint::AfterRename));
        }
        // The first matching consult is still the skipped one.
        assert!(
            !hooks.take_save_failure(STORE_LVM_STATE, StoreSavePoint::AfterRenameBeforeDirFsync)
        );
        assert!(
            hooks.take_save_failure(STORE_LVM_STATE, StoreSavePoint::AfterRenameBeforeDirFsync)
        );
    }

    #[test]
    fn clear_drops_kill_and_failure_arms() {
        let hooks = StoreCrashHooks::new();
        hooks.arm(STORE_DRBD_STATE, StoreSavePoint::AfterRename);
        hooks.arm_save_failure(
            STORE_LVM_STATE,
            StoreSavePoint::AfterRenameBeforeDirFsync,
            0,
        );
        let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Arc::clone(&fired);
        hooks.set_kill_switch(Arc::new(move || {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        }));
        hooks.clear();
        assert!(
            !hooks.take_save_failure(STORE_LVM_STATE, StoreSavePoint::AfterRenameBeforeDirFsync)
        );
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            hooks.consult(STORE_DRBD_STATE, StoreSavePoint::AfterRename);
        }))
        .expect("clear must drop the kill arm too");
        assert!(!fired.load(std::sync::atomic::Ordering::SeqCst));
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
