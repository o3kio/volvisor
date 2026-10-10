//! The grow-notification engine (P6-B, ADR-0006 first slice part 1):
//! the real `guest_notification_status` machine behind
//! [`volvisor_types::request::GrowGuestNotification`].
//!
//! # The contract it implements
//!
//! Volume API v2 §4A's partial-failure rule: the backend may grow
//! before the VMM/guest is notified; the notification is **retried,
//! never undone by shrinking the backing**; guest filesystem
//! expansion is not implied. The three response values mean:
//!
//! - `not_applicable` — the volume has no attachment: there is no
//!   frontend to notify;
//! - `notified` — the VMM accepted the resize-disk call for the
//!   grow's effective size;
//! - `retry_required` — the notification is outstanding: refused by
//!   the version gate, unaddressable (no recorded VMM disk id),
//!   refused by the VMM, or failed in transport. The reason is
//!   recorded durably. Never a silent un-notified success.
//!
//! # The invariant
//!
//! The engine maintains one invariant over every attached,
//! addressable volume: **the VMM has been told a size at least the
//! volume's current size.** Everything follows from it:
//!
//! - [`GrowNotifier::notify_grow`] runs inside the grow
//!   operation (the API layer composes it around the provider's
//!   response, inside the journal's execute closure, so the
//!   journaled outcome carries the real status and replays
//!   byte-compatibly) and re-establishes the invariant for the new
//!   size;
//! - [`GrowNotificationEngine::retry_pass`] — the bounded retry tick
//!   and the startup reconcile, one and the same pass — restores the
//!   invariant for **every** attached volume: a pending record is
//!   re-driven, a record whose target is below the volume's current
//!   size is re-driven, and a volume with no record at all is driven
//!   (a fresh attach therefore receives one idempotent no-op resize
//!   on the first pass — the price of a reconcile that cannot miss a
//!   crash between the backing grow and the intent journal, and the
//!   mechanism that heals volumes grown under the pre-P6-B
//!   placeholder, which were never notified);
//! - a pending record whose volume lost its attachment resolves
//!   `not_applicable` and is removed (a detached frontend learned
//!   the device's size when the VM next opened it; retrying against
//!   a dead socket would never converge).
//!
//! The never-shrink rule holds structurally: the pass drives the
//! volume's **current** size, which only grows (grow-only is the
//! provider's contract), and a stale record whose target is above
//! the current size (a deleted-and-recreated volume identity)
//! triggers no drive — the VMM is never told a smaller size.
//!
//! # The durable state
//!
//! One JSON file (`grow-notifications.json` under the daemon's
//! journal directory), one record per volume — the latest
//! notification state, not a history. Saves are atomic with the
//! house discipline (serialize → owner-only tmp write → fsync →
//! rename → directory fsync) and carry the P5 store-save crash seam
//! (`STORE_GROW_NOTIFICATIONS`): a rig can kill the saving task at
//! each commit boundary, and the reload sees exactly what the crash
//! point left. The intent is journaled **before** the resize-disk
//! attempt (AGENTS rule 8), so a crash between the intent and the
//! outcome leaves a pending record the pass re-drives.
//!
//! # The seams
//!
//! The engine is deliberately synchronous — the established
//! control-path pattern (providers run whole CLIs inside their async
//! trait methods; one bounded HTTP exchange is the same class). Its
//! collaborators are injected:
//!
//! - the [`VmmController`] (`ChRemoteVmm` in production, `FakeVmm`
//!   in tests) — `None` when the socket directory is unconfigured,
//!   which refuses the notification with a recorded reason;
//! - the [`VmmVersionGate`] — the startup-verified, fail-closed
//!   verdict (see `vmm_version`);
//! - the [`GrowAttachmentFacts`] closure — the provider-local truth
//!   (which volume is attached, to which VM, with which recorded VMM
//!   disk id, at which current size), never a consumer assertion;
//! - the [`Clock`] — deterministic timestamps in tests.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use volvisor_types::crash::{STORE_GROW_NOTIFICATIONS, StoreCrashHooks, StoreSavePoint};
use volvisor_types::id::VolumeId;
use volvisor_types::request::GrowGuestNotification;
use volvisor_types::{ApiError, ApiErrorCode};

use crate::vmm::VmmController;
use crate::vmm_version::VmmVersionGate;

/// The engine's clock: unix epoch seconds, injectable for
/// determinism (the migration coordinator's shape).
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// One attached volume's notification facts, as the provider reports
/// them from its own durable state — the participant-facts pattern:
/// never asserted by the consumer, never guessed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttachmentForGrow {
    /// Attached and addressable: the attachment recorded the VMM
    /// disk id, and the provider's current size is the notification
    /// target.
    Addressable {
        /// The consuming VM (the api-socket is
        /// `{api_socket_dir}/{vm_id}.sock`).
        vm_id: String,
        /// The attachment-recorded VMM device id — what the
        /// resize-disk body addresses.
        vmm_disk_id: String,
        /// The volume's current size in bytes (the notification
        /// target; grow-only, so it never decreases).
        current_size_bytes: u64,
    },
    /// Attached but unaddressable: the attachment recorded no VMM
    /// disk id. The notification is refused with a recorded reason —
    /// a frontend exists, so this is never `not_applicable`.
    Unaddressable,
}

/// The provider-local attachment enumeration: every volume with an
/// attachment, keyed by volume identity (a volume absent from the
/// map is detached). One call per notification and per reconcile
/// pass; the provider derives it under its own state lock.
pub type GrowAttachmentFacts =
    Arc<dyn Fn() -> Result<BTreeMap<VolumeId, AttachmentForGrow>, ApiError> + Send + Sync>;

/// One volume's durable notification state (the latest, not a
/// history): what the VMM is owed or was told.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrowNotificationRecord {
    /// The volume the notification belongs to.
    pub volume_id: VolumeId,
    /// The consuming VM the notification addresses (empty when the
    /// recorded refusal precedes addressing — an unaddressable
    /// attachment).
    pub vm_id: String,
    /// The VMM device id the notification addresses (empty for the
    /// same unaddressable reason).
    pub vmm_disk_id: String,
    /// The size the notification drives (bytes).
    pub target_size_bytes: u64,
    /// The status: outstanding (with the recorded reason) or told.
    pub status: GrowNotificationStatus,
    /// Last update, unix epoch seconds (the injected clock).
    pub updated_at: u64,
}

impl GrowNotificationRecord {
    /// Whether the notification is still outstanding.
    #[must_use]
    pub fn is_pending(&self) -> bool {
        matches!(self.status, GrowNotificationStatus::Pending { .. })
    }

    /// The recorded pending reason, when outstanding.
    #[must_use]
    pub fn pending_reason(&self) -> Option<String> {
        match &self.status {
            GrowNotificationStatus::Pending { reason } => Some(reason.clone()),
            GrowNotificationStatus::Notified { .. } => None,
        }
    }
}

/// The record's status half.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum GrowNotificationStatus {
    /// The notification is outstanding; the reason records why (in
    /// flight, gate refusal, unaddressable, VMM refusal, transport
    /// failure). The retry pass re-drives it.
    Pending {
        /// The recorded, human-readable reason.
        reason: String,
    },
    /// The VMM accepted the resize-disk call for
    /// [`GrowNotificationRecord::target_size_bytes`].
    Notified {
        /// When the VMM accepted (unix epoch seconds).
        at: u64,
    },
}

/// The per-volume outcome report of one retry pass (the renewal
/// report's shape: the daemon's task logs it, the engine stays
/// tracing-free).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GrowRetryReport {
    /// Volumes whose notification succeeded on this pass, with the
    /// size the VMM was told.
    pub notified: Vec<(VolumeId, u64)>,
    /// Volumes whose notification is still outstanding, with the
    /// recorded reason.
    pub retry_required: Vec<(VolumeId, String)>,
    /// Volumes whose pending record resolved `not_applicable` (the
    /// attachment is gone).
    pub not_applicable: Vec<VolumeId>,
}

/// The daemon-side grow-notification seam: what the API layer calls
/// inside the grow operation, after the provider resized the
/// backing, to drive the VMM capacity notification and report the
/// honest [`GrowGuestNotification`].
///
/// Infallible by design: the grow already succeeded; the contract's
/// partial-failure rule makes every notification-side failure a
/// recorded `retry_required`, never the grow's failure.
pub trait GrowNotifier: Send + Sync {
    /// Drive (or refuse, with the reason recorded) the capacity
    /// notification for `volume_id` at `effective_size_bytes` — the
    /// grow's effective size from the provider's own report.
    fn notify_grow(&self, volume_id: &VolumeId, effective_size_bytes: u64)
    -> GrowGuestNotification;
}

/// The grow-notification engine (the module docs). Constructed by
/// the daemon's wiring (`volvisord::runtime`) or directly by test
/// rigs; every collaborator is injected.
pub struct GrowNotificationEngine {
    /// The VMM controller seam; `None` when the socket directory is
    /// unconfigured (the notification is refused with the recorded
    /// reason, never attempted against a guess).
    vmm: Option<Arc<dyn VmmController>>,
    /// The startup-verified, fail-closed version gate.
    gate: VmmVersionGate,
    /// The provider-local attachment facts.
    facts: GrowAttachmentFacts,
    /// The durable state. The engine is the single writer; the
    /// store itself is deliberately unsynchronized.
    store: Mutex<GrowNotificationStore>,
    /// The injected clock.
    clock: Clock,
}

impl GrowNotificationEngine {
    /// Compose the engine from its seams.
    #[must_use]
    pub fn new(
        vmm: Option<Arc<dyn VmmController>>,
        gate: VmmVersionGate,
        facts: GrowAttachmentFacts,
        store: GrowNotificationStore,
        clock: Clock,
    ) -> Self {
        Self {
            vmm,
            gate,
            facts,
            store: Mutex::new(store),
            clock,
        }
    }

    /// One volume's durable notification state (introspection for
    /// wiring tests and the e2e rig).
    ///
    /// # Errors
    /// `INTERNAL` when the store lock is poisoned (a crash-injected
    /// save died inside it — the rig's shape, never production).
    pub fn record(&self, volume_id: &VolumeId) -> Result<Option<GrowNotificationRecord>, ApiError> {
        Ok(self.lock_store()?.get(volume_id))
    }

    /// Every durable notification state, ordered by volume identity
    /// (introspection).
    ///
    /// # Errors
    /// `INTERNAL` when the store lock is poisoned.
    pub fn records(&self) -> Result<Vec<GrowNotificationRecord>, ApiError> {
        Ok(self.lock_store()?.records())
    }

    /// The grow path (the module docs): derive the honest status for
    /// a just-grown volume and journal the notification state.
    ///
    /// Every internal failure — the facts cannot be read, the store
    /// cannot be saved — lands as `retry_required`: the grow already
    /// succeeded, and the contract's partial-failure rule makes the
    /// notification retryable, never the grow's failure. A
    /// facts-read failure is unrecordable (the addressing lives in
    /// the facts) but not silent: the status is `retry_required`,
    /// and the same failure fails the next retry pass wholesale,
    /// which the daemon's task logs. A store failure likewise
    /// leaves nothing recorded; the retry pass's error surfaces it
    /// every tick.
    fn notify(&self, volume_id: &VolumeId, effective_size_bytes: u64) -> GrowGuestNotification {
        let Ok(attachments) = (self.facts)() else {
            return GrowGuestNotification::RetryRequired;
        };
        match attachments.get(volume_id) {
            None => {
                // Detached: no frontend to notify. Any pending record
                // for the volume resolves the same way (the pass
                // would do it; doing it here keeps the response and
                // the durable state in step).
                self.resolve_detached(volume_id);
                GrowGuestNotification::NotApplicable
            }
            Some(AttachmentForGrow::Unaddressable) => self.record_pending(
                volume_id,
                "",
                "",
                effective_size_bytes,
                "the attachment records no vmm_disk_id; the VMM resize-disk call cannot be \
                 addressed (re-attach with vmm_disk_id to enable the notification)"
                    .to_owned(),
            ),
            Some(AttachmentForGrow::Addressable {
                vm_id, vmm_disk_id, ..
            }) => self.drive(volume_id, vm_id, vmm_disk_id, effective_size_bytes),
        }
    }

    /// Drive one notification to `target`: gate, addressability,
    /// journal intent, attempt, journal outcome — returning the
    /// honest status, every refusal or failure recorded as the
    /// pending reason (the module docs' invariant).
    fn drive(
        &self,
        volume_id: &VolumeId,
        vm_id: &str,
        vmm_disk_id: &str,
        target: u64,
    ) -> GrowGuestNotification {
        // The version gate first (a read-only consult of the cached
        // verdict): an unproven VMM version refuses the notification
        // typed; the reason is recorded, never a silent un-notified
        // success.
        if let Err(refusal) = self.gate.check() {
            return self.record_pending(volume_id, vm_id, vmm_disk_id, target, refusal.detail);
        }
        let Some(vmm) = self.vmm.as_ref() else {
            return self.record_pending(
                volume_id,
                vm_id,
                vmm_disk_id,
                target,
                "the VMM controller is not wired (vmm.api_socket_dir and vmm.ch_remote_bin \
                 must both be configured); the resize-disk call cannot be addressed"
                    .to_owned(),
            );
        };
        // Journal the intent before the attempt (AGENTS rule 8): a
        // pending record with the target is durable before the VMM
        // is touched, so a crash between here and the outcome leaves
        // a re-drivable record. A store that cannot journal the
        // intent fails closed — the attempt is not made.
        let intent = self.pending_record(
            volume_id,
            vm_id,
            vmm_disk_id,
            target,
            "the notification is in flight".to_owned(),
        );
        if self.save_record(&intent).is_err() {
            return GrowGuestNotification::RetryRequired;
        }
        match vmm.resize_disk(vm_id, vmm_disk_id, target) {
            Ok(()) => {
                let now = (self.clock)();
                let record = GrowNotificationRecord {
                    volume_id: volume_id.clone(),
                    vm_id: vm_id.to_owned(),
                    vmm_disk_id: vmm_disk_id.to_owned(),
                    target_size_bytes: target,
                    status: GrowNotificationStatus::Notified { at: now },
                    updated_at: now,
                };
                match self.save_record(&record) {
                    Ok(()) => GrowGuestNotification::Notified,
                    // The VMM was told but the outcome is unrecordable:
                    // honestly retry_required (the next pass re-drives
                    // the idempotent resize and records it).
                    Err(_) => GrowGuestNotification::RetryRequired,
                }
            }
            Err(error) => self.record_pending(volume_id, vm_id, vmm_disk_id, target, error.detail),
        }
    }

    /// Record one pending notification with `reason`, returning
    /// `retry_required` — the recorded refusal shape, whether or not
    /// the store could persist it (a failed save surfaces through
    /// the retry pass's error).
    fn record_pending(
        &self,
        volume_id: &VolumeId,
        vm_id: &str,
        vmm_disk_id: &str,
        target: u64,
        reason: String,
    ) -> GrowGuestNotification {
        let record = self.pending_record(volume_id, vm_id, vmm_disk_id, target, reason);
        let _ = self.save_record(&record);
        GrowGuestNotification::RetryRequired
    }

    /// Build (without saving) one pending record.
    fn pending_record(
        &self,
        volume_id: &VolumeId,
        vm_id: &str,
        vmm_disk_id: &str,
        target: u64,
        reason: String,
    ) -> GrowNotificationRecord {
        GrowNotificationRecord {
            volume_id: volume_id.clone(),
            vm_id: vm_id.to_owned(),
            vmm_disk_id: vmm_disk_id.to_owned(),
            target_size_bytes: target,
            status: GrowNotificationStatus::Pending { reason },
            updated_at: (self.clock)(),
        }
    }

    /// Save one record through the store (the single-writer lock).
    fn save_record(&self, record: &GrowNotificationRecord) -> Result<(), ApiError> {
        self.lock_store()?.upsert(record)
    }

    /// Resolve a detached volume's pending record (`not_applicable`,
    /// removed — the module docs).
    fn resolve_detached(&self, volume_id: &VolumeId) {
        if let Ok(mut store) = self.lock_store() {
            drop(store.remove(volume_id));
        }
    }

    /// One reconcile pass (the module docs): restore the invariant
    /// for every attached volume, resolve detached pendings. The
    /// startup reconcile and the bounded retry tick are the same
    /// pass — the migration retry task's shape (an immediate pass at
    /// startup, then one per tick).
    ///
    /// # Errors
    /// `INTERNAL` when the attachment facts cannot be read or the
    /// store fails (the whole pass fails, the renewal-report
    /// pattern); per-volume drive failures land in the report's
    /// `retry_required` with the recorded reason.
    pub fn retry_pass(&self) -> Result<GrowRetryReport, ApiError> {
        let attachments = (self.facts)()?;
        let mut report = GrowRetryReport::default();
        for (volume_id, facts) in &attachments {
            let AttachmentForGrow::Addressable {
                vm_id,
                vmm_disk_id,
                current_size_bytes,
            } = facts
            else {
                // Unaddressable: an existing pending record keeps its
                // recorded reason (reported); no new obligation is
                // invented for a volume that never grew.
                if let Some(reason) = self
                    .lock_store()?
                    .get(volume_id)
                    .and_then(|record| record.pending_reason())
                {
                    report.retry_required.push((volume_id.clone(), reason));
                }
                continue;
            };
            let needs_drive = match self.lock_store()?.get(volume_id) {
                // No record: nothing proves the VMM was told the
                // current size — drive (the module docs'
                // crash-window and pre-P6-B healing case).
                None => true,
                Some(record) => {
                    record.is_pending() || record.target_size_bytes < *current_size_bytes
                }
            };
            if !needs_drive {
                continue;
            }
            match self.drive(volume_id, vm_id, vmm_disk_id, *current_size_bytes) {
                GrowGuestNotification::Notified => {
                    report
                        .notified
                        .push((volume_id.clone(), *current_size_bytes));
                }
                GrowGuestNotification::RetryRequired => {
                    let reason = self
                        .lock_store()?
                        .get(volume_id)
                        .and_then(|record| record.pending_reason())
                        .unwrap_or_else(|| "the notification is outstanding".to_owned());
                    report.retry_required.push((volume_id.clone(), reason));
                }
                GrowGuestNotification::NotApplicable => {
                    // drive() never returns this (the caller derived
                    // an addressable attachment).
                    report.not_applicable.push(volume_id.clone());
                }
            }
        }
        // Detached resolution: pending records whose volume has no
        // attachment anymore.
        let detached: Vec<VolumeId> = self
            .lock_store()?
            .records()
            .into_iter()
            .filter(GrowNotificationRecord::is_pending)
            .map(|record| record.volume_id)
            .filter(|volume_id| !attachments.contains_key(volume_id))
            .collect();
        for volume_id in detached {
            self.lock_store()?.remove(&volume_id)?;
            report.not_applicable.push(volume_id);
        }
        Ok(report)
    }

    /// Lock the store, mapping poisoning to the typed error (a
    /// crash-injected save died inside it — the rig's shape; the
    /// engine is single-writer otherwise).
    fn lock_store(&self) -> Result<std::sync::MutexGuard<'_, GrowNotificationStore>, ApiError> {
        self.store.lock().map_err(|_| {
            ApiError::new(
                ApiErrorCode::Internal,
                "the grow-notification store lock is poisoned (a crash-injected save died \
                 inside it)"
                    .to_owned(),
            )
        })
    }
}

impl GrowNotifier for GrowNotificationEngine {
    fn notify_grow(
        &self,
        volume_id: &VolumeId,
        effective_size_bytes: u64,
    ) -> GrowGuestNotification {
        self.notify(volume_id, effective_size_bytes)
    }
}

/// The durable notification state: one JSON file, one record per
/// volume, atomically saved (the module docs). Deliberately dumb —
/// it persists exactly the state it is handed; transitions and
/// timestamps are the engine's. Not internally synchronized: the
/// engine is the single writer (behind its mutex).
#[derive(Debug)]
pub struct GrowNotificationStore {
    /// The state file.
    path: PathBuf,
    /// The loaded index, keyed by volume identity.
    records: BTreeMap<VolumeId, GrowNotificationRecord>,
    /// The store-save crash seam (P5 plan §3.1): inert by default,
    /// armed only by the constructing test rig through
    /// [`Self::store_crash_hooks`].
    crash: Arc<StoreCrashHooks>,
}

impl GrowNotificationStore {
    /// Open the store at `path`. A missing file loads as an empty
    /// store (first start); a leftover `.tmp` sibling is the
    /// discarded half of an interrupted save and is ignored.
    ///
    /// # Errors
    /// `INTERNAL` when the file cannot be read or fails to parse —
    /// notification state is a partial-failure obligation, so a
    /// corrupt file is a typed startup error, never silently
    /// dropped.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, ApiError> {
        let path = path.into();
        let internal = |detail: String| ApiError::new(ApiErrorCode::Internal, detail);
        let records = match fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).map_err(|e| {
                internal(format!(
                    "failed to parse the grow-notification state {}: {e}",
                    path.display()
                ))
            })?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => {
                return Err(internal(format!(
                    "failed to read the grow-notification state {}: {e}",
                    path.display()
                )));
            }
        };
        Ok(Self {
            path,
            records,
            crash: Arc::new(StoreCrashHooks::new()),
        })
    }

    /// Look up one volume's record.
    #[must_use]
    pub fn get(&self, volume_id: &VolumeId) -> Option<GrowNotificationRecord> {
        self.records.get(volume_id).cloned()
    }

    /// All records, ordered by volume identity.
    #[must_use]
    pub fn records(&self) -> Vec<GrowNotificationRecord> {
        self.records.values().cloned().collect()
    }

    /// Insert or replace one volume's record, persisting the whole
    /// state atomically (save first, apply after — a failed save
    /// leaves the in-memory state untouched).
    ///
    /// # Errors
    /// `INTERNAL` when the atomic save (serialize, owner-only tmp
    /// write, fsync, rename, directory fsync) fails; the previous
    /// file remains intact.
    pub fn upsert(&mut self, record: &GrowNotificationRecord) -> Result<(), ApiError> {
        let mut next = self.records.clone();
        next.insert(record.volume_id.clone(), record.clone());
        save_state_atomic(&self.path, &next, &self.crash)?;
        self.records = next;
        Ok(())
    }

    /// Remove one volume's record (persisting the removal). Removing
    /// an absent record is a no-op.
    ///
    /// # Errors
    /// `INTERNAL` when the atomic save fails.
    pub fn remove(
        &mut self,
        volume_id: &VolumeId,
    ) -> Result<Option<GrowNotificationRecord>, ApiError> {
        let Some(removed) = self.records.get(volume_id).cloned() else {
            return Ok(None);
        };
        let mut next = self.records.clone();
        next.remove(volume_id);
        save_state_atomic(&self.path, &next, &self.crash)?;
        self.records = next;
        Ok(Some(removed))
    }

    /// The store-save crash seam (P5 plan §3.1): the armed table a
    /// test rig aims and the kill switch fires into. Inert unless a
    /// rig arms it; no route or input reaches it.
    #[must_use]
    pub fn store_crash_hooks(&self) -> &Arc<StoreCrashHooks> {
        &self.crash
    }
}

/// Persist the state map atomically (serialize → owner-only tmp
/// write → fsync → rename → directory fsync), consulting the crash
/// seam at each commit boundary (the `MigrationStore` discipline).
fn save_state_atomic(
    path: &Path,
    records: &BTreeMap<VolumeId, GrowNotificationRecord>,
    crash: &StoreCrashHooks,
) -> Result<(), ApiError> {
    let internal = |detail: String| ApiError::new(ApiErrorCode::Internal, detail);
    let tmp_path = sibling_tmp_path(path);
    let tmp_display = tmp_path.display();
    let path_display = path.display();
    let data = serde_json::to_vec_pretty(records).map_err(|e| {
        internal(format!(
            "failed to serialize the grow-notification state: {e}"
        ))
    })?;
    let mut file = create_owner_only(&tmp_path)
        .map_err(|e| internal(format!("failed to create {tmp_display}: {e}")))?;
    let write = file.write_all(&data);
    if let Err(e) = write {
        drop(fs::remove_file(&tmp_path));
        return Err(internal(format!("failed to write {tmp_display}: {e}")));
    }
    // The store-save crash points (P5 plan §3.1): after the tmp
    // content write, after its fsync, after the rename. Inert unless
    // the rig armed this store's seam.
    crash.consult(STORE_GROW_NOTIFICATIONS, StoreSavePoint::AfterTmpWrite);
    if let Err(e) = file.sync_all() {
        drop(fs::remove_file(&tmp_path));
        return Err(internal(format!("failed to fsync {tmp_display}: {e}")));
    }
    drop(file);
    crash.consult(
        STORE_GROW_NOTIFICATIONS,
        StoreSavePoint::AfterFsyncBeforeRename,
    );
    if let Err(e) = fs::rename(&tmp_path, path) {
        drop(fs::remove_file(&tmp_path));
        return Err(internal(format!(
            "failed to rename {tmp_display} to {path_display}: {e}"
        )));
    }
    crash.consult(STORE_GROW_NOTIFICATIONS, StoreSavePoint::AfterRename);
    // fsync the directory so the rename itself is durable.
    let dir = fs::File::open(
        path.parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new(".")),
    );
    let result = match dir {
        Ok(dir) => dir
            .sync_all()
            .map_err(|e| internal(format!("failed to fsync the parent of {path_display}: {e}"))),
        Err(e) => Err(internal(format!(
            "failed to open the parent of {path_display}: {e}"
        ))),
    };
    if result.is_err() {
        drop(fs::remove_file(&tmp_path));
    }
    result
}

/// The `<path>.tmp` sibling used for atomic saves.
fn sibling_tmp_path(path: &Path) -> PathBuf {
    let mut os_name = path.as_os_str().to_owned();
    os_name.push(".tmp");
    PathBuf::from(os_name)
}

/// Create (or truncate) `path` for writing with owner-only
/// permissions (`0600` on unix; the platform default elsewhere).
fn create_owner_only(path: &Path) -> std::io::Result<fs::File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
    }
    #[cfg(not(unix))]
    {
        fs::File::create(path)
    }
}
