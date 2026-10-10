//! The journal-append crash hook (P5 plan §3.1, stage A subset).
//!
//! The aggressive failure campaign must kill a daemon at the durable
//! write boundaries of its journal-before-mutate pipeline — after a
//! matching operation's intent write, or before/after its outcome
//! write — because those are the windows a real process death
//! lands in. This module is that injection point: a per-operation
//! **armed table** the ops pipeline (the crate's `ops` module)
//! consults after each
//! durable journal write of a matching operation.
//!
//! # Trust class (doc-gated, the `FakeFailKnobs` precedent)
//!
//! The hook is **test-gated and inert by default**:
//!
//! - an `AppState` built without
//!   `with_crash_hooks` carries
//!   **no** hooks at all — every production path, every config and
//!   every route consults nothing;
//! - the armed table starts empty and **no route, request body,
//!   config value or input path can set it** — only the constructing
//!   test rig, which is the sole caller of [`CrashHooks::arm`] and
//!   [`CrashHooks::set_kill_switch`];
//! - an armed entry is one-shot: the first matching durable write
//!   consumes it, so a kill fires exactly once per arming.
//!
//! # Firing semantics (§3.3's task-group kill model)
//!
//! When a consulted write matches an armed `(operation kind, crash
//! point)` pair, the hook, in order:
//!
//! 1. consumes the entry (one shot);
//! 2. fires the registered kill switch — the rig's supervisor, which
//!    aborts the daemon's task group (serve, the rig-side renewal
//!    loop, the migration retry task, the drive tasks);
//! 3. terminates the firing request mid-handler via
//!    [`std::panic::panic_any`].
//!
//! Step 3 is the in-band equivalent of a process dying mid-handler:
//! the request's connection dies without a reply (the caller observes
//! a transport failure, never a typed response), the outcome write
//! never happens for `AfterIntent`/`BeforeOutcome` points, and every
//! `Arc` the handler held unwinds with it — which is also what frees
//! the journal's `flock` for the restart. A panic is used because a
//! task cannot abort itself through any other tokio API; the payload
//! carries [`CRASH_PANIC_PREFIX`] so a test binary's panic hook can
//! tell campaign injections from real failures.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

/// The durable-write boundary one armed entry kills at (P5 plan
/// §3.1). The points are the journal-before-mutate pipeline's own
/// ordering: intent durably first, mutation, outcome durably after.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrashPoint {
    /// After the durable intent append, before the mutation runs —
    /// the crash window in which only the intent exists.
    AfterIntent,
    /// After the mutation, immediately before the durable outcome
    /// append — the act has landed, its record has not.
    BeforeOutcome,
    /// After the durable outcome append, before the reply is served —
    /// the replay-able window.
    AfterOutcome,
}

/// The kill switch the constructing rig registers: fired inside the
/// armed point, before the request dies. The switch must be safe to
/// call from within a request task — it runs from the ops pipeline
/// between journal writes, OUTSIDE every journal critical section
/// (the append has returned; no lock is held across the fire) — and
/// must not await: aborts are requests, and the supervisor owns the
/// awaiting.
pub type KillSwitch = Arc<dyn Fn() + Send + Sync>;

/// The prefix every crash-injection panic payload starts with — a
/// campaign test binary filters its panic hook on this so injected
/// kills stay silent while real panics print normally.
pub const CRASH_PANIC_PREFIX: &str = "volvisor-campaign-crash:";

/// The per-daemon armed table (P5 plan §3.1): operation kind → the
/// crash point its next matching durable write dies at. Inert until
/// the constructing rig arms an entry and registers a kill switch;
/// every method below is only reachable from the rig (the pipeline
/// calls the crate-private `CrashHooks::consult`).
///
/// One instance belongs to one daemon's
/// `AppState` and outlives its restarts (the rig
/// keeps the `Arc` in the fixture core, so a re-launched daemon
/// consults the same table), which is what makes a kill
/// deterministic across the two-daemon world: operation kinds are
/// role-specific — only the source journals `migration_transfer`,
/// only the destination journals the peer operations — so an armed
/// key targets exactly one daemon.
pub struct CrashHooks {
    /// The armed entries (one per operation kind, consumed on fire).
    armed: Mutex<BTreeMap<&'static str, CrashPoint>>,
    /// The rig's group-abort action, when registered.
    kill: Mutex<Option<KillSwitch>>,
}

impl CrashHooks {
    /// An inert table: nothing armed, no kill switch. This is the
    /// only state any non-rig constructor can observe.
    #[must_use]
    pub fn new() -> Self {
        Self {
            armed: Mutex::new(BTreeMap::new()),
            kill: Mutex::new(None),
        }
    }

    /// Arm one operation kind to die at `point` on its next matching
    /// durable write (one shot — the firing write consumes the
    /// entry). **Rig only** (the doc-gated trust class above).
    pub fn arm(&self, op_kind: &'static str, point: CrashPoint) {
        self.lock_armed().insert(op_kind, point);
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

    /// Consult the table at one durable-write boundary of
    /// `op_kind` (crate-private: the ops pipeline is the only
    /// caller). Inert unless the rig armed exactly this
    /// `(operation kind, point)` pair — a differently-pointed entry
    /// stays armed for its own point. When armed: consume the entry,
    /// fire the kill switch, then terminate the firing request
    /// mid-handler (see the module docs).
    ///
    /// The `clippy::panic` allow below is the one deliberate,
    /// documented exception in this workspace: terminating the
    /// firing request IS the injection's semantics (the in-band
    /// equivalent of a process dying mid-handler — no reply, no
    /// outcome write, every held `Arc` unwinding with the task), and
    /// no other tokio API can stop the current task from inside
    /// itself. The payload is marked with [`CRASH_PANIC_PREFIX`] so
    /// test binaries can tell injections from real failures.
    #[allow(clippy::panic)] // the deliberate crash-injection point (see above)
    pub(crate) fn consult(&self, op_kind: &'static str, point: CrashPoint) {
        let fired = {
            let mut armed = self.lock_armed();
            if armed.get(op_kind) == Some(&point) {
                armed.remove(op_kind);
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
        // The in-band process death: unwind this request task so its
        // reply never lands and its Arcs (the journal flock among
        // them) release with it.
        std::panic::panic_any(format!("{CRASH_PANIC_PREFIX}{op_kind}/{point:?}"));
    }

    /// Lock the armed table, recovering from a poison left by an
    /// earlier firing (the panic above unwinds under this lock by
    /// design — a poisoned table is a fired table, not a broken one).
    fn lock_armed(&self) -> MutexGuard<'_, BTreeMap<&'static str, CrashPoint>> {
        self.armed.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// See [`CrashHooks::lock_armed`].
    fn lock_kill(&self) -> MutexGuard<'_, Option<KillSwitch>> {
        self.kill.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Default for CrashHooks {
    fn default() -> Self {
        Self::new()
    }
}
