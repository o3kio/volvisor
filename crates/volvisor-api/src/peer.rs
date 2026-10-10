//! # The internal peer routes (destination host; P4b plan §6, stage B2)
//!
//! The destination daemon's half of a coordinated migration: the six
//! `/v2/internal/peer/*` routes the source daemon's handoff driver
//! calls over the daemon-to-daemon credential. Every mutation routes
//! through the same journal pipeline as every other privileged act
//! (`crate::ops::execute_resolvable`), with **derived** operation ids
//! (`mig-api-{tag}-{16hex}` over the migration id) so a post-crash
//! re-drive replays the recorded outcome instead of re-executing.
//!
//! ## The in-flight resolution rule (peer-route journaling)
//!
//! An intent-without-outcome retry of a peer mutation is resolved by
//! **inspection, never a blind re-execution**: each route's `inspect`
//! closure proves — from the witness and the provider state — whether
//! the act already landed. `Ok(Some(...))` journals and serves the
//! proven result; `Ok(None)` proves the act did not land (or only its
//! idempotent prefix did) and the mutation re-executes, with the act's
//! own first step re-verifying its preconditions; an `Err` surfaces
//! typed and nothing is guessed.
//!
//! ## The failure re-issue rule (the `grant_set` wedge fix)
//!
//! A recorded **failure** outcome of a peer mutation is re-issued, not
//! re-served (`crate::ops::FailureReplay::Reissue`): the failure is a
//! fact about a past attempt, and the world may have converged past it
//! — the recorded wedge (P6-A part 3) was exactly that shape, a
//! witness kill inside the grant commit whose witness-side replay
//! landed the grant while B's journaled failure replayed forever. The
//! re-issue runs the *same* inspection as the in-flight rule: proven
//! landed → the proven outcome supersedes the recorded failure and is
//! served; proven not landed → the act re-executes under its
//! idempotency discipline (below); the inspection's own error surfaces
//! typed — the stale failure is never re-served as a terminal answer.
//! The rule is safe for exactly these four acts and no others: their
//! operation ids are migration-derived (deterministic per migration —
//! no consumer recourse exists after the cut, which is why a permanent
//! park was a defect), their landed-ness is totally inspectable, their
//! re-execution is idempotent at every layer, and their refusals are
//! world-derived and reproduce identically on re-execution (an
//! operator-judgment refusal never rides this path — the strict routes
//! keep verbatim failure replay). A genuinely unresolvable failure
//! (a stable typed refusal, e.g. a promote the resource cannot
//! serve) still parks: the re-issue re-executes and the refusal
//! reproduces — a bounded, honest spin.
//!
//! Each act is built to make that resolution honest:
//!
//! - **prepare** is content-idempotent through the durable
//!   [`TargetPreparationStore`](crate::peer::TargetPreparationStore)
//!   (identical content re-serves, different content is a typed
//!   conflict);
//! - **grant** inspects the witness for a live lease held by **this
//!   host** on every participant and the preparation record for the
//!   promoted device paths; a partially landed grant (witness batch
//!   done, promote not) re-executes safely — the witness `GrantSet`
//!   runs under a deterministic operation id (the journal replays the
//!   recorded outcome) and `promote_target` is idempotent per
//!   migration;
//! - **restore-vm** is `vmm.state`-gated (`Running` proves the act,
//!   `Paused` proves a no-resume restore);
//! - **discard** is proven by the preparation record's absence.
//!
//! ## The snapshot directory boundary
//!
//! The cutover's v1 snapshot transport is operator-provided shared
//! storage (plan §1 out-of-scope record): the destination proves it
//! can use the configured `snapshot_dir` root **at `PREPARED`**, not
//! at config time — the `prepare` route probes
//! `{snapshot_root}/{vm_id}` by writing, reading back and removing a
//! probe file, refusing the whole preparation typed when the shared
//! path is unusable. `vm_id` is joined into the path only after a
//! path-segment guard (it is consumer-supplied free text on the
//! source side, never a validated identity).
//!
//! ## The fifth route
//!
//! `POST /v2/internal/peer/discard` is an additive stage-B2 deviation
//! from the plan §6 route list (which names prepare/grant/restore-vm/
//! health): `discard_target` needs a destination-side act — the
//! pre-cut abort tail that drops the target preparation — and the
//! source driver must reach it over the same authenticated surface.
//! It is idempotent (`Ok` when the preparation is already absent).
//!
//! ## The sixth route
//!
//! `POST /v2/internal/peer/verify-lineage` is the barrier-time
//! lineage re-verification (P6-A F1, defense in depth): an
//! **observation**, never journaled — the source re-reads its live
//! lineage set and this route re-runs exactly the replica-level gate
//! prepare ran, so a wrong-lineage injection landing on the target
//! after the prepare is refused at the barrier, before the cut
//! crosses foreign data (see the `verify_lineage` handler below).

// axum handlers consume their extractors by value; clippy's
// pass-by-value heuristics do not apply to the handler boundary.
#![allow(clippy::needless_pass_by_value)]

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;

use axum::extract::State;
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::Response;
use serde::{Deserialize, Serialize};
use volvisor_handoff::{BatchStep, Participant, batch_operation_id};
use volvisor_provider::handoff::HandoffSurface;
use volvisor_provider::provider::VolumeProvider;
use volvisor_provider::vmm::{DiskMapping, VmState, VmmController};
use volvisor_types::id::{HostId, MigrationId, VolumeId};
use volvisor_types::request::{AccessModeRequest, AttachVolumeRequest};
use volvisor_types::{
    ApiError, ApiErrorCode, AttachmentId, AuthorityView, Frontend, LeaseState, OperationId,
};
use volvisor_witness::WITNESS_PROTOCOL_VERSION;
use volvisor_witness::blocking::BlockingWitnessConnection;
use volvisor_witness::proto::{BatchGrantVolume, GrantSetRequest};

use crate::error::{ApiErrorReply, error_response, json_response, to_json_value};
use crate::extract::{ValidJson, bearer_token, fixed_time_eq};
use crate::ops;
use crate::state::SharedState;

/// The contract api-version every internally-derived attach request
/// carries (the volume contract's fixed envelope).
const API_VERSION: &str = "volvisor.volume.v2";

/// The typed `NOT_FOUND` refusal for a peer act whose migration has no
/// target preparation on this host.
fn preparation_absent(migration_id: &MigrationId) -> ApiError {
    ApiError::not_found(format!(
        "no target preparation for migration {migration_id} on this host"
    ))
}

/// The typed refusal when this daemon does not serve the peer surface.
fn peer_surface_unavailable() -> ApiError {
    ApiError::new(
        ApiErrorCode::NotFound,
        "the peer surface is not enabled on this deployment \
         (this daemon is not migration-enabled as a destination)",
    )
}

/// Whether `vm_id` is safe to join into a filesystem path: non-empty,
/// no path separators, and neither `.` nor `..` (a consumer-supplied
/// free-text identity on the source side — the join happens only
/// behind this guard).
fn safe_path_segment(vm_id: &str) -> bool {
    !vm_id.is_empty()
        && vm_id != "."
        && vm_id != ".."
        && !vm_id.contains('/')
        && !vm_id.contains('\0')
}

/// Probe one directory for read-write usability: create it if missing,
/// write a probe file, read it back, remove it. Any failure is `false`
/// — the caller refuses typed, never guesses.
fn probe_dir(dir: &Path) -> bool {
    let probe = dir.join(".volvisor-peer-probe");
    let ok = (|| {
        fs::create_dir_all(dir).ok()?;
        fs::write(&probe, b"probe").ok()?;
        let read_back = fs::read(&probe).ok()?;
        if read_back != b"probe" {
            return None;
        }
        fs::remove_file(&probe).ok()?;
        Some(())
    })();
    // Never leave the probe file behind on a failed read-back path.
    if ok.is_none() {
        drop(fs::remove_file(&probe));
    }
    ok.is_some()
}

/// Run one synchronous operation on the blocking thread pool (the
/// witness boundary and the VMM controller are synchronous seams;
/// calling them inline would pin an async worker for their full
/// bounded wait).
async fn run_blocking<T, F>(operation: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, ApiError> + Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|join_error| {
            ApiError::new(
                ApiErrorCode::Internal,
                format!("blocking peer operation failed to run: {join_error}"),
            )
        })?
}

/// Map a witness refusal onto the API error taxonomy.
fn witness_error(error: volvisor_witness::proto::WitnessError) -> ApiError {
    error.to_api_error()
}

// ---------------------------------------------------------------------------
// PeerRouteContext
// ---------------------------------------------------------------------------

/// Everything the peer routes need on the destination host: the
/// witness connection (this host's W8 credential), the VMM controller
/// (the destination VMM's proxy, D6), the provider's handoff surface
/// (`promote_target`), the volume provider (the `prepare` route's
/// existence/generation verification), this host's identity, the
/// durable target-preparation store and the configured snapshot root.
///
/// Built by the daemon when `[migration]` is enabled; its absence in
/// [`crate::AppState`] serves the typed 404 on every peer route.
pub struct PeerRouteContext {
    /// This host's witness connection (host credential, W8-bound).
    witness: Arc<dyn BlockingWitnessConnection>,
    /// The destination VMM controller (`ch-remote` adapter or a fake).
    vmm: Arc<dyn VmmController>,
    /// The provider's coordinated-handoff surface (`promote_target`).
    handoff: Arc<dyn HandoffSurface>,
    /// The volume provider of this daemon (prepare verification).
    provider: Arc<dyn VolumeProvider>,
    /// This host's identity (the grant holder, the attach host).
    host_id: HostId,
    /// The durable target-preparation store.
    preparations: TargetPreparationStore,
    /// The configured snapshot root (shared with the peer, plan §6).
    snapshot_root: PathBuf,
}

impl PeerRouteContext {
    /// Build the destination-side context.
    #[must_use]
    pub fn new(
        witness: Arc<dyn BlockingWitnessConnection>,
        vmm: Arc<dyn VmmController>,
        handoff: Arc<dyn HandoffSurface>,
        provider: Arc<dyn VolumeProvider>,
        host_id: HostId,
        preparations: TargetPreparationStore,
        snapshot_root: impl Into<PathBuf>,
    ) -> Self {
        Self {
            witness,
            vmm,
            handoff,
            provider,
            host_id,
            preparations,
            snapshot_root: snapshot_root.into(),
        }
    }

    /// This host's identity (diagnostics; the daemon owns the real use).
    #[must_use]
    pub fn host_id(&self) -> &HostId {
        &self.host_id
    }

    /// The configured snapshot root (diagnostics).
    #[must_use]
    pub fn snapshot_root(&self) -> &Path {
        &self.snapshot_root
    }
}

// ---------------------------------------------------------------------------
// RequirePeer
// ---------------------------------------------------------------------------

/// Extractor enforcing the daemon-to-daemon credential on the internal
/// peer routes (plan §6: distinct from both the admin token and the
/// witness credentials — a peer call is never a consumer call).
///
/// **Fail closed**, both directions:
///
/// - a missing or mismatching token is rejected with `401` — a peer
///   surface without its own credential must not be callable;
/// - a daemon without a configured peer context (not
///   migration-enabled as a destination) serves the typed `404`
///   instead — the route exists uniformly, the deployment opted out.
pub(crate) struct RequirePeer;

impl axum::extract::FromRequestParts<SharedState> for RequirePeer {
    type Rejection = Response;

    // The trait mandates an async signature; the check itself is
    // synchronous (header comparison and state presence only).
    #[allow(clippy::unused_async_trait_impl)]
    async fn from_request_parts(
        parts: &mut Parts,
        state: &SharedState,
    ) -> Result<Self, Self::Rejection> {
        if state.peer_ctx.is_none() {
            return Err(error_response(&peer_surface_unavailable()));
        }
        let Some(expected) = state.peer_token.as_deref() else {
            return Err(unauthorized_response(
                "peer_api_token is not configured; the internal peer routes are \
                 disabled (fail closed)",
            ));
        };
        let presented = bearer_token(&parts.headers);
        if presented.is_some_and(|token| fixed_time_eq(token, expected)) {
            Ok(Self)
        } else {
            Err(unauthorized_response(
                "the internal peer routes require a valid peer bearer token",
            ))
        }
    }
}

/// The `401 UNAUTHORIZED` reply (transport-level code, contract error
/// shape — the same spelling as the admin extractor's).
fn unauthorized_response(message: &str) -> Response {
    let body = serde_json::json!({
        "code": "UNAUTHORIZED",
        "message": message,
    });
    json_response(StatusCode::UNAUTHORIZED, &body)
}

// ---------------------------------------------------------------------------
// The durable target-preparation store
// ---------------------------------------------------------------------------

/// One prepared participant of a target preparation: the verified
/// volume, the generation it was verified against, and — after the
/// grant act promoted it — the device path the restore's disk mapping
/// needs (recorded durably so an in-flight grant is resolvable by
/// inspection).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedParticipant {
    /// The participating volume.
    pub volume_id: VolumeId,
    /// The generation the volume was verified against at `prepare`.
    pub expected_generation: u64,
    /// The promoted replica's device path, recorded by the grant act;
    /// `None` until the promotion landed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_path: Option<String>,
}

/// The destination-side preparation record of one migration (plan §2:
/// "target replica verified, resource present, Secondary, connected,
/// no fence marker; snapshot dir readability verified"), persisted as
/// one JSON file per migration under the store discipline shared with
/// the migration records: serialize → tmp write (`0600`) →
/// fsync → rename → directory fsync. A crash can never leave a torn
/// record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetPreparation {
    /// The migration identity (store key).
    pub migration_id: MigrationId,
    /// The migrated VM (the restore's target).
    pub vm_id: String,
    /// The source host (diagnostics; the coordinator's peer).
    pub source_host: HostId,
    /// The destination host (always this host on whose store it lives).
    pub target_host: HostId,
    /// The verified participants, in the migration's preparation order
    /// (the order is part of the deterministic batch operation-id
    /// derivation).
    pub participants: Vec<PreparedParticipant>,
    /// Unix epoch seconds at preparation.
    pub created_at: u64,
}

impl TargetPreparation {
    /// Content equality for the `prepare` route's idempotency: the
    /// identity fields and the ordered `(volume, generation)`
    /// participant set. The per-participant `device_path` is
    /// deliberately excluded — it is the grant act's output, not the
    /// preparation's content, so a re-prepare after a grant still
    /// re-serves the record instead of conflicting with it.
    #[must_use]
    pub fn content_eq(&self, other: &Self) -> bool {
        self.migration_id == other.migration_id
            && self.vm_id == other.vm_id
            && self.source_host == other.source_host
            && self.target_host == other.target_host
            && self
                .participants
                .iter()
                .map(|p| (&p.volume_id, p.expected_generation))
                .collect::<Vec<_>>()
                == other
                    .participants
                    .iter()
                    .map(|p| (&p.volume_id, p.expected_generation))
                    .collect::<Vec<_>>()
    }
}

/// The durable store of [`TargetPreparation`] records: one JSON file
/// per migration under a dedicated directory, atomically saved (the
/// `MigrationStore` discipline). A cheaply cloneable handle to one
/// shared store (the mutex is internal); the peer routes share one
/// context across handler tasks, and the daemon keeps a clone for the
/// retry path.
///
/// Corrupt or unparseable record files are a **typed startup error**,
/// never silently dropped: a preparation is authority-relevant state
/// (the grant act trusts its participant set).
#[derive(Clone)]
pub struct TargetPreparationStore {
    inner: std::sync::Arc<Mutex<StoreInner>>,
}

struct StoreInner {
    dir: PathBuf,
    records: BTreeMap<MigrationId, TargetPreparation>,
}

impl TargetPreparationStore {
    /// Open the store at `dir`, loading every record file into the
    /// index. The directory is created if missing (first start).
    ///
    /// # Errors
    /// `INTERNAL` when the directory cannot be created or read, or a
    /// record file fails to parse, carries an unknown field, or
    /// disagrees with its file name.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self, ApiError> {
        let dir = dir.into();
        let internal = |detail: String| ApiError::new(ApiErrorCode::Internal, detail);
        fs::create_dir_all(&dir).map_err(|e| {
            internal(format!(
                "failed to create peer-preparation store {}: {e}",
                dir.display()
            ))
        })?;
        let mut records = BTreeMap::new();
        let entries = fs::read_dir(&dir).map_err(|e| {
            internal(format!(
                "failed to read peer-preparation store {}: {e}",
                dir.display()
            ))
        })?;
        for entry in entries {
            let entry = entry.map_err(|e| {
                internal(format!(
                    "failed to read peer-preparation store entry in {}: {e}",
                    dir.display()
                ))
            })?;
            let path = entry.path();
            // Only finished records are loaded; a leftover `.tmp` is
            // the discarded half of an interrupted atomic save.
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            let path_display = path.display();
            let text = fs::read_to_string(&path).map_err(|e| {
                internal(format!(
                    "failed to read peer preparation {path_display}: {e}"
                ))
            })?;
            let record: TargetPreparation = serde_json::from_str(&text).map_err(|e| {
                internal(format!(
                    "failed to parse peer preparation {path_display}: {e}"
                ))
            })?;
            let file_id = MigrationId::new(
                path.file_stem()
                    .and_then(|stem| stem.to_str())
                    .unwrap_or_default(),
            )
            .map_err(|e| {
                internal(format!(
                    "peer preparation file {path_display} has an invalid identity: {e}"
                ))
            })?;
            if record.migration_id != file_id {
                return Err(internal(format!(
                    "peer preparation file {path_display} holds a record for {} \
                     (name/record mismatch)",
                    record.migration_id
                )));
            }
            records.insert(file_id, record);
        }
        Ok(Self {
            inner: std::sync::Arc::new(Mutex::new(StoreInner { dir, records })),
        })
    }

    /// An in-memory empty store (tests and preflight checks).
    #[must_use]
    pub fn new_in_memory() -> Self {
        Self {
            inner: std::sync::Arc::new(Mutex::new(StoreInner {
                dir: PathBuf::new(),
                records: BTreeMap::new(),
            })),
        }
    }

    /// Look up one preparation.
    ///
    /// # Errors
    /// `INTERNAL` only when the internal lock is poisoned.
    pub fn load(&self, migration_id: &MigrationId) -> Result<Option<TargetPreparation>, ApiError> {
        let inner = self.lock()?;
        Ok(inner.records.get(migration_id).cloned())
    }

    /// Install one preparation idempotently: absent → persisted;
    /// identical content → re-served; different content for the same
    /// migration → the typed conflict (a `migration_id` is never
    /// silently re-targeted).
    ///
    /// # Errors
    /// `IDEMPOTENCY_CONFLICT` on differing content; `INTERNAL` on the
    /// atomic-save failure (the previous record file remains intact).
    pub fn install(&self, record: &TargetPreparation) -> Result<(), ApiError> {
        let mut inner = self.lock()?;
        if let Some(existing) = inner.records.get(&record.migration_id) {
            if existing.content_eq(record) {
                return Ok(());
            }
            return Err(ApiError::idempotency_conflict(format!(
                "target preparation of {}",
                record.migration_id
            )));
        }
        let path = record_path(&inner.dir, &record.migration_id);
        save_atomic(
            &path,
            &serde_json::to_vec_pretty(record).map_err(|e| {
                ApiError::new(
                    ApiErrorCode::Internal,
                    format!("failed to serialize peer preparation: {e}"),
                )
            })?,
        )?;
        inner
            .records
            .insert(record.migration_id.clone(), record.clone());
        Ok(())
    }

    /// Record one participant's promoted device path durably (the
    /// grant act's per-participant tail; the in-flight resolution and
    /// the restore's disk-mapping verification both read it).
    ///
    /// # Errors
    /// `NOT_FOUND` when the preparation is absent; `INTERNAL` on the
    /// atomic-save failure.
    pub fn record_device_path(
        &self,
        migration_id: &MigrationId,
        volume_id: &VolumeId,
        device_path: &str,
    ) -> Result<(), ApiError> {
        let mut inner = self.lock()?;
        let Some(record) = inner.records.get_mut(migration_id) else {
            return Err(preparation_absent(migration_id));
        };
        let Some(participant) = record
            .participants
            .iter_mut()
            .find(|p| p.volume_id == *volume_id)
        else {
            return Err(ApiError::not_found(format!(
                "volume {volume_id} is not a participant of migration {migration_id}"
            )));
        };
        // First recording wins (the promotion of one participant under
        // one migration is deterministic; a differing path is a bug we
        // surface, never paper over).
        if let Some(existing) = &participant.device_path {
            if existing != device_path {
                return Err(ApiError::new(
                    ApiErrorCode::Internal,
                    format!(
                        "participant {volume_id} of {migration_id} already records device \
                         path {existing}; refusing to overwrite it with {device_path}"
                    ),
                ));
            }
            return Ok(());
        }
        participant.device_path = Some(device_path.to_owned());
        let updated = record.clone();
        let path = record_path(&inner.dir, migration_id);
        let bytes = serde_json::to_vec_pretty(&updated).map_err(|e| {
            ApiError::new(
                ApiErrorCode::Internal,
                format!("failed to serialize peer preparation: {e}"),
            )
        })?;
        save_atomic(&path, &bytes)?;
        Ok(())
    }

    /// Remove one preparation and its file; removing an absent record
    /// is a no-op returning `false`.
    ///
    /// # Errors
    /// `INTERNAL` when the file cannot be removed (other than it
    /// already being absent).
    pub fn remove(&self, migration_id: &MigrationId) -> Result<bool, ApiError> {
        let mut inner = self.lock()?;
        let path = record_path(&inner.dir, migration_id);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(ApiError::new(
                    ApiErrorCode::Internal,
                    format!("failed to remove peer preparation {}: {e}", path.display()),
                ));
            }
        }
        Ok(inner.records.remove(migration_id).is_some())
    }

    fn lock(&self) -> Result<MutexGuard<'_, StoreInner>, ApiError> {
        self.inner.lock().map_err(|_| {
            ApiError::new(
                ApiErrorCode::Internal,
                "peer-preparation store lock poisoned by a previous failure",
            )
        })
    }
}

/// The `{dir}/{migration_id}.json` record path.
fn record_path(dir: &Path, migration_id: &MigrationId) -> PathBuf {
    dir.join(format!("{migration_id}.json"))
}

/// Persist `bytes` at `path` atomically: write `<path>.tmp`
/// (owner-only `0600` on unix) → fsync → rename over `path` → fsync
/// the parent directory (the `MigrationStore` discipline).
fn save_atomic(path: &Path, bytes: &[u8]) -> Result<(), ApiError> {
    let internal = |detail: String| ApiError::new(ApiErrorCode::Internal, detail);
    let mut os_name = path.as_os_str().to_owned();
    os_name.push(".tmp");
    let tmp_path = PathBuf::from(os_name);
    let tmp_display = tmp_path.display();
    let path_display = path.display();
    let result = (|| -> Result<(), ApiError> {
        let mut file = create_owner_only(&tmp_path)
            .map_err(|e| internal(format!("failed to create {tmp_display}: {e}")))?;
        file.write_all(bytes)
            .map_err(|e| internal(format!("failed to write {tmp_display}: {e}")))?;
        file.sync_all()
            .map_err(|e| internal(format!("failed to fsync {tmp_display}: {e}")))?;
        drop(file);
        fs::rename(&tmp_path, path).map_err(|e| {
            internal(format!(
                "failed to rename {tmp_display} to {path_display}: {e}"
            ))
        })?;
        let dir = fs::File::open(
            path.parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new(".")),
        )
        .map_err(|e| internal(format!("failed to open parent of {path_display}: {e}")))?;
        dir.sync_all()
            .map_err(|e| internal(format!("failed to fsync parent of {path_display}: {e}")))?;
        Ok(())
    })();
    if result.is_err() {
        // Best-effort cleanup: never leave a stale .tmp behind.
        drop(fs::remove_file(&tmp_path));
    }
    result
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

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

/// `POST /v2/internal/peer/prepare` request: the source daemon's
/// notification of one migration's participant set, verified against
/// this host's provider before the preparation is persisted.
///
/// The plan §6 route body names `{migration_id, volume_ids[],
/// expected_generations[]}`; `vm_id` and `source_host` are additive
/// stage-B2 fields (the destination must know which VM the restore
/// targets and the preparation record is the durable place to carry
/// it — there is no second source call to ask).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerPrepareRequest {
    /// The migration identity.
    pub migration_id: MigrationId,
    /// The migrated VM (the restore's target).
    pub vm_id: String,
    /// The source host.
    pub source_host: HostId,
    /// The participating volumes (non-empty, unique, ordered).
    pub volume_ids: Vec<VolumeId>,
    /// The expected generation per volume, positionally aligned.
    pub expected_generations: Vec<u64>,
    /// The expected data-generation lineage per volume (the source's
    /// live `show-gi` set, positionally aligned; P5 plan §5.2 — the
    /// destination's replica-level gate compares it against the
    /// target's live lineage, refusing foreign data before the cut).
    #[serde(default)]
    pub expected_lineages: Vec<Vec<String>>,
}

impl PeerPrepareRequest {
    /// Validate the shape before anything is journaled.
    ///
    /// # Errors
    /// `INVALID_REQUEST` for every shape violation (typed, before the
    /// journal sees the payload).
    pub fn validate(&self) -> Result<(), ApiError> {
        if !safe_path_segment(&self.vm_id) {
            return Err(ApiError::invalid_request(
                "vm_id must be a non-empty path segment (no '/', not '.' or '..')",
            ));
        }
        if self.volume_ids.is_empty() {
            return Err(ApiError::invalid_request(
                "a migration needs at least one participating volume",
            ));
        }
        if self.volume_ids.len() != self.expected_generations.len() {
            return Err(ApiError::invalid_request(format!(
                "expected_generations has {} entries for {} volumes \
                 (positionally aligned lists)",
                self.expected_generations.len(),
                self.volume_ids.len()
            )));
        }
        if self.volume_ids.len() != self.expected_lineages.len() {
            return Err(ApiError::invalid_request(format!(
                "expected_lineages has {} entries for {} volumes \
                 (positionally aligned lists)",
                self.expected_lineages.len(),
                self.volume_ids.len()
            )));
        }
        for (volume_id, lineage) in self.volume_ids.iter().zip(&self.expected_lineages) {
            if lineage.is_empty() {
                return Err(ApiError::invalid_request(format!(
                    "participant {volume_id} carries an empty expected lineage (the source \
                     must attest its live data-generation set)"
                )));
            }
        }
        let mut seen = std::collections::BTreeSet::new();
        for volume_id in &self.volume_ids {
            if !seen.insert(volume_id.clone()) {
                return Err(ApiError::invalid_request(format!(
                    "duplicate participant volume {volume_id}"
                )));
            }
        }
        Ok(())
    }
}

/// `POST /v2/internal/peer/prepare` response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerPrepareResponse {
    /// The migration identity.
    pub migration_id: MigrationId,
    /// The migrated VM.
    pub vm_id: String,
    /// The verified participants, in preparation order.
    pub participants: Vec<PeerPreparedVolume>,
}

/// One verified participant of a prepare response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerPreparedVolume {
    /// The verified volume.
    pub volume_id: VolumeId,
    /// The generation the volume was verified against.
    pub expected_generation: u64,
}

/// `POST /v2/internal/peer/verify-lineage` request: the barrier-time
/// lineage re-verification (P6-A F1, defense in depth). The same
/// lineage input prepare carries — the source's live
/// data-generation set per participant, positionally aligned — read
/// fresh by the source at call time, never cached in a record. The
/// route re-runs exactly the replica-level gate prepare ran
/// ([`HandoffSurface::verify_target_replica`]) and mutates nothing.
///
/// No `deny_unknown_fields` (the additive discipline this PR's new
/// types follow): an unknown field from a newer peer is skipped, not
/// a decode failure.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerVerifyLineageRequest {
    /// The migration identity.
    pub migration_id: MigrationId,
    /// The participating volumes (non-empty, unique, ordered).
    pub volume_ids: Vec<VolumeId>,
    /// The source's live expected lineage per volume, positionally
    /// aligned with `volume_ids`.
    pub expected_lineages: Vec<Vec<String>>,
}

impl PeerVerifyLineageRequest {
    /// Validate the request shape (aligned, non-empty, unique).
    ///
    /// # Errors
    /// `INVALID_REQUEST` for an empty participant set, misaligned
    /// lists, an empty lineage or a duplicate participant.
    pub fn validate(&self) -> Result<(), ApiError> {
        if self.volume_ids.is_empty() {
            return Err(ApiError::invalid_request(
                "a lineage verification needs at least one participating volume",
            ));
        }
        if self.volume_ids.len() != self.expected_lineages.len() {
            return Err(ApiError::invalid_request(format!(
                "expected_lineages has {} entries for {} volumes \
                 (positionally aligned lists)",
                self.expected_lineages.len(),
                self.volume_ids.len()
            )));
        }
        for (volume_id, lineage) in self.volume_ids.iter().zip(&self.expected_lineages) {
            if lineage.is_empty() {
                return Err(ApiError::invalid_request(format!(
                    "participant {volume_id} carries an empty expected lineage (the source \
                     must attest its live data-generation set)"
                )));
            }
        }
        let mut seen = std::collections::BTreeSet::new();
        for volume_id in &self.volume_ids {
            if !seen.insert(volume_id.clone()) {
                return Err(ApiError::invalid_request(format!(
                    "duplicate participant volume {volume_id}"
                )));
            }
        }
        Ok(())
    }
}

/// `POST /v2/internal/peer/verify-lineage` response: the verified
/// participant set, echoed in preparation order so the source driver
/// can assert the destination re-verified exactly the volumes the
/// record carries (same order, never a subset).
///
/// No `deny_unknown_fields` (the additive discipline this PR's new
/// types follow).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerVerifyLineageResponse {
    /// The migration identity.
    pub migration_id: MigrationId,
    /// The re-verified volumes, in preparation order.
    pub volume_ids: Vec<VolumeId>,
}

/// `POST /v2/internal/peer/grant` request: the migration identity
/// alone — the participant set, the witness operation id and the
/// attach identities are all derived from the durable preparation, so
/// a peer cannot assert a set this host did not verify.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerGrantRequest {
    /// The migration identity.
    pub migration_id: MigrationId,
}

/// `POST /v2/internal/peer/grant` response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerGrantResponse {
    /// The migration identity.
    pub migration_id: MigrationId,
    /// One outcome per participant, in preparation order.
    pub grants: Vec<PeerGrantOutcome>,
}

/// One participant's grant outcome: the witness-minted authority facts
/// and the promoted replica's device path (the source driver's
/// disk-mapping input for the restore).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerGrantOutcome {
    /// The granted volume.
    pub volume_id: VolumeId,
    /// The promoted replica's device path.
    pub device_path: String,
    /// The granted writer epoch.
    pub epoch: u64,
    /// The granted lease identity.
    pub lease_id: u64,
    /// The lease TTL as a duration from this response (W5).
    pub lease_ttl_secs: u64,
}

/// `POST /v2/internal/peer/restore-vm` request.
///
/// The plan §6 route body is `{migration_id, snapshot_dir, disks[]}`;
/// `resume` is an additive stage-B2 field (the source's forward path
/// restores **and** resumes in one act, while a bare restore leaves
/// the destination VM paused for an operator-triggered resume).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerRestoreVmRequest {
    /// The migration identity.
    pub migration_id: MigrationId,
    /// The migration's snapshot directory (absolute, on the shared
    /// filesystem).
    pub snapshot_dir: String,
    /// The disk mappings: the snapshot config's declared source paths
    /// → this host's promoted device paths.
    pub disks: Vec<DiskMapping>,
    /// Restore **and** resume (the forward path); `false` leaves the
    /// restored VM paused.
    #[serde(default)]
    pub resume: bool,
}

impl PeerRestoreVmRequest {
    /// Validate the shape before anything is journaled.
    ///
    /// # Errors
    /// `INVALID_REQUEST` for an empty disk set or a non-absolute
    /// snapshot directory.
    pub fn validate(&self) -> Result<(), ApiError> {
        if self.disks.is_empty() {
            return Err(ApiError::invalid_request(
                "a restore needs at least one disk mapping",
            ));
        }
        if !self.snapshot_dir.starts_with('/') {
            return Err(ApiError::invalid_request(
                "snapshot_dir must be an absolute path on the shared filesystem",
            ));
        }
        Ok(())
    }
}

/// `POST /v2/internal/peer/restore-vm` response: the observed VM state
/// after the act (honest by construction — a `vm.info` observation,
/// never an inference from a command's exit status).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerRestoreVmResponse {
    /// The migration identity.
    pub migration_id: MigrationId,
    /// The observed state of the destination VM after the act.
    pub vm_state: VmState,
}

/// `POST /v2/internal/peer/discard` request (the additive fifth route;
/// see the module docs).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerDiscardRequest {
    /// The migration identity.
    pub migration_id: MigrationId,
}

/// `POST /v2/internal/peer/discard` response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerDiscardResponse {
    /// The migration identity.
    pub migration_id: MigrationId,
    /// Whether this call removed a preparation (`false` when it was
    /// already absent — the idempotent re-drive answer).
    pub discarded: bool,
}

/// `GET /v2/internal/peer/health` response: the honest snapshot-dir
/// answer, probed at call time (never a config-time claim).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerHealthResponse {
    /// This host's identity.
    pub host_id: HostId,
    /// The configured snapshot root.
    pub snapshot_dir: String,
    /// Whether the root is usable right now (write+read-back+remove
    /// probe).
    pub snapshot_dir_readable: bool,
}

// ---------------------------------------------------------------------------
// Derived identities
// ---------------------------------------------------------------------------

/// The deterministic identity string shared by the promote attach's
/// `operation_id` and `attachment_id`: `mig-attach-{16hex}` over a
/// domain-separated SHA-256 of the migration id and the volume id.
///
/// One derivation for both namespaces (they never meet: the attach
/// request's operation id lives in the provider's journal, the
/// attachment id in the volume's attachment set) — and deterministic
/// across re-drives so `promote_target`'s idempotent tail recognizes
/// its own prior attachment.
fn attach_identity(migration_id: &MigrationId, volume_id: &VolumeId) -> String {
    use sha2::Digest;
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut hasher = sha2::Sha256::new();
    hasher.update(b"volvisor.api.peer.attach.v1:");
    hasher.update(migration_id.as_str().as_bytes());
    hasher.update(b":");
    hasher.update(volume_id.as_str().as_bytes());
    let digest = hasher.finalize();
    let mut out = String::from("mig-attach-");
    for byte in digest.iter().take(8) {
        out.push(HEX[usize::from(byte >> 4)] as char);
        out.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    out
}

/// The handoff-crate participants for the deterministic `GrantSet`
/// witness operation id. The id folds only the **ordered volume ids**
/// (`volvisor_handoff::batch_operation_id`'s hash input); the
/// provider-local `resource`/`minor` facts are deliberately not
/// re-derived on the destination — the id must match what the ordered
/// participant set derives, and the preparation record already fixed
/// that order at `prepare` time.
fn id_only_participants(preparation: &TargetPreparation) -> Vec<Participant> {
    preparation
        .participants
        .iter()
        .map(|p| Participant {
            volume_id: p.volume_id.clone(),
            expected_generation: p.expected_generation,
            resource: String::new(),
            minor: 0,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// verify-lineage
// ---------------------------------------------------------------------------

/// `POST /v2/internal/peer/verify-lineage` — the barrier-time lineage
/// re-verification (P6-A F1, defense in depth): re-run exactly the
/// replica-level gate prepare ran ([`HandoffSurface::
/// verify_target_replica`]) over the source's freshly-read live
/// lineage set, so a wrong-lineage injection that landed on the
/// target after the prepare is refused at the barrier — before the
/// cut crosses foreign data.
///
/// Read-only with respect to authority and journal state (an
/// observation, like `health`): never journaled, no preparation
/// record, no witness mutation, no idempotency machinery — the gate
/// is deterministic over observed replica state and repeats verbatim
/// on every call. The cost is the I/O class prepare already paid (the
/// destination's own replica-status and lineage reads); the source's
/// re-read of its live lineage is the source driver's side of the
/// same contract.
pub(crate) async fn verify_lineage(
    State(state): State<SharedState>,
    _peer: RequirePeer,
    ValidJson(req): ValidJson<PeerVerifyLineageRequest>,
) -> Result<Response, ApiErrorReply> {
    req.validate()?;
    let ctx = peer_context(&state)?;
    tracing::info!(
        migration_id = %req.migration_id,
        volumes = req.volume_ids.len(),
        "accepting peer lineage verification"
    );
    for (volume_id, lineage) in req.volume_ids.iter().zip(&req.expected_lineages) {
        // The same gate prepare runs, per participant, first-failure
        // refuses: resource present, Secondary, connected, the
        // definition naming this host, no tracked residues — and the
        // live data-generation set equal to the source-supplied
        // expected set (FOREIGN_DEVICE_STATE on mismatch).
        ctx.handoff
            .verify_target_replica(volume_id, lineage)
            .await?;
    }
    let body = to_json_value(&PeerVerifyLineageResponse {
        migration_id: req.migration_id,
        volume_ids: req.volume_ids,
    })?;
    Ok(json_response(StatusCode::OK, &body))
}

// ---------------------------------------------------------------------------
// Shared act helpers
// ---------------------------------------------------------------------------

/// This host's witness view of one volume (blocking seam, off the
/// async worker).
async fn witness_view(
    witness: &Arc<dyn BlockingWitnessConnection>,
    volume_id: VolumeId,
) -> Result<AuthorityView, ApiError> {
    let witness = Arc::clone(witness);
    run_blocking(move || witness.inspect(&volume_id).map_err(witness_error)).await
}

/// The observed state of one VM (blocking seam, off the async worker).
async fn vmm_state(vmm: &Arc<dyn VmmController>, vm_id: &str) -> Result<VmState, ApiError> {
    let vmm = Arc::clone(vmm);
    let vm_id = vm_id.to_owned();
    run_blocking(move || vmm.state(&vm_id)).await
}

// ---------------------------------------------------------------------------
// prepare
// ---------------------------------------------------------------------------

/// `POST /v2/internal/peer/prepare` — verify the participant set
/// against this host's provider (existence, generation), probe the
/// migration's snapshot directory on the shared filesystem, and
/// persist the preparation. Journaled; in-flight retries resolve by
/// the preparation store (content-idempotent).
pub(crate) async fn prepare(
    State(state): State<SharedState>,
    _peer: RequirePeer,
    ValidJson(req): ValidJson<PeerPrepareRequest>,
) -> Result<Response, ApiErrorReply> {
    req.validate()?;
    let ctx = peer_context(&state)?;
    let operation_id = ops::mobility_operation_id(&req.migration_id, "peer-prepare")?;
    let body = to_json_value(&req)?;
    let hash = ops::mobility_request_hash("peer-prepare", &body);
    tracing::info!(
        kind = ops::OP_PEER_PREPARE,
        operation_id = %operation_id,
        migration_id = %req.migration_id,
        vm_id = %req.vm_id,
        "accepting peer prepare"
    );
    let inspect_ctx = Arc::clone(&ctx);
    let inspect_req = req.clone();
    let run_ctx = Arc::clone(&ctx);
    ops::execute_resolvable(
        &state,
        ops::OP_PEER_PREPARE,
        operation_id,
        hash,
        body,
        None,
        ops::FailureReplay::Reissue,
        move || {
            let ctx = Arc::clone(&inspect_ctx);
            let req = inspect_req.clone();
            async move { prepare_inspect(ctx, req) }
        },
        move || {
            let ctx = Arc::clone(&run_ctx);
            async move { prepare_act(ctx, req).await }
        },
    )
    .await
    .map_err(ApiErrorReply::from)
}

/// The shared peer context, guaranteed present by [`RequirePeer`]
/// (the extractor ran first; the `?` is for the type checker).
fn peer_context(state: &SharedState) -> Result<Arc<PeerRouteContext>, ApiError> {
    state.peer_ctx.clone().ok_or_else(peer_surface_unavailable)
}

/// The prepare act's inspection: an identical preparation proves the
/// act landed (its result is re-served); a differing preparation for
/// the same migration is the typed conflict; absence proves nothing
/// landed and the act re-executes. (Synchronous: the preparation
/// store is the only input.)
fn prepare_inspect(
    ctx: Arc<PeerRouteContext>,
    req: PeerPrepareRequest,
) -> Result<Option<PeerPrepareResponse>, ApiError> {
    match ctx.preparations.load(&req.migration_id)? {
        Some(existing) => {
            let candidate = preparation_from_request(&ctx, &req, 0);
            if existing.content_eq(&candidate) {
                Ok(Some(prepare_response(&existing)))
            } else {
                Err(ApiError::idempotency_conflict(format!(
                    "target preparation of {}",
                    req.migration_id
                )))
            }
        }
        None => Ok(None),
    }
}

/// Build the preparation record a request maps to (the timestamp is
/// the coordinator's; content equality ignores it).
fn preparation_from_request(
    ctx: &PeerRouteContext,
    req: &PeerPrepareRequest,
    created_at: u64,
) -> TargetPreparation {
    TargetPreparation {
        migration_id: req.migration_id.clone(),
        vm_id: req.vm_id.clone(),
        source_host: req.source_host.clone(),
        target_host: ctx.host_id.clone(),
        participants: req
            .volume_ids
            .iter()
            .zip(&req.expected_generations)
            .map(|(volume_id, expected_generation)| PreparedParticipant {
                volume_id: volume_id.clone(),
                expected_generation: *expected_generation,
                device_path: None,
            })
            .collect(),
        created_at,
    }
}

/// The prepare response a preparation record re-serves.
fn prepare_response(record: &TargetPreparation) -> PeerPrepareResponse {
    PeerPrepareResponse {
        migration_id: record.migration_id.clone(),
        vm_id: record.vm_id.clone(),
        participants: record
            .participants
            .iter()
            .map(|p| PeerPreparedVolume {
                volume_id: p.volume_id.clone(),
                expected_generation: p.expected_generation,
            })
            .collect(),
    }
}

/// The prepare act: verify every participant against this host's
/// provider (typed refusals for a stale generation on a volume this
/// host tracks, and — for the untracked P3 peer side, operator-
/// provisioned until the promote adopts it — the handoff surface's
/// replica-level gate), probe the migration's snapshot directory,
/// persist the preparation.
async fn prepare_act(
    ctx: Arc<PeerRouteContext>,
    req: PeerPrepareRequest,
) -> Result<PeerPrepareResponse, ApiError> {
    let participants: Vec<_> = req
        .volume_ids
        .iter()
        .zip(&req.expected_generations)
        .zip(&req.expected_lineages)
        .map(|((volume_id, expected), lineage)| (volume_id, *expected, lineage))
        .collect();
    for (volume_id, expected, lineage) in participants {
        match ctx.provider.inspect_volume(volume_id).await {
            Ok(inspected) => {
                if inspected.generation != expected {
                    return Err(ApiError::stale_generation(expected, inspected.generation));
                }
            }
            // The P3 peer side is operator-provisioned and untracked
            // in this host's provider state (its volume records begin
            // at the promote): `NOT_FOUND` here is not a refusal —
            // the replica-level gate below owns the verification.
            Err(error) if error.code == ApiErrorCode::NotFound => {}
            Err(error) => return Err(error),
        }
        // The replica-level gate for EVERY participant (plan §6:
        // "target replica verified — resource present, Secondary,
        // connected, no fence marker"): refusing an unready
        // destination here, before the source's cut, is this route's
        // whole purpose (row 12 — one unprepared participant refuses
        // the whole migration). The expected data-generation lineage
        // is the SOURCE's live set (P5 plan §5.2 — wrong-lineage data
        // at the target is refused typed here), carried in the
        // request so this act never needs the witness: the
        // crash-window shapes park a record with the witness down,
        // and preparation is not an authority act.
        ctx.handoff
            .verify_target_replica(volume_id, lineage)
            .await?;
    }
    // The snapshot-dir boundary (plan §1/§6): prove the shared path is
    // usable by this host NOW, not at config time. The per-migration
    // directory is `{snapshot_root}/{vm_id}` — the VM's whole writable
    // set shares one snapshot (rule 6), and the join is guarded.
    let snapshot_dir = ctx.snapshot_root.join(&req.vm_id);
    if !probe_dir(&snapshot_dir) {
        return Err(ApiError::new(
            ApiErrorCode::InvalidState,
            format!(
                "snapshot directory {} is not usable from this host \
                 (the shared filesystem must be readable and writable here)",
                snapshot_dir.display()
            ),
        ));
    }
    // The destination VMM must be empty for this VM. The restore act's
    // destroy-first re-drive (plan §3, row 18) treats any VM present
    // at restore time as this migration's own half-restore — which is
    // only sound if the socket was verified empty when the preparation
    // was recorded. A squatted VM id is refused here, before the
    // source's cut; a foreign VM is never destroyed.
    let observed = vmm_state(&ctx.vmm, &req.vm_id).await?;
    if observed != VmState::Absent {
        return Err(ApiError::new(
            ApiErrorCode::InvalidState,
            format!(
                "destination VMM of VM {} is not empty (state {observed:?}); the coordinated \
                 restore requires an empty destination",
                req.vm_id
            ),
        ));
    }
    let record = preparation_from_request(&ctx, &req, unix_now());
    ctx.preparations.install(&record)?;
    Ok(prepare_response(&record))
}

/// Local unix time in seconds (the house `now_unix` discipline).
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

// ---------------------------------------------------------------------------
// grant
// ---------------------------------------------------------------------------

/// `POST /v2/internal/peer/grant` — the destination's
/// `DESTINATION_AUTHORIZED` act: the witness `GrantSet` (fresh epochs
/// for this host, set-wide, under the deterministic batch operation
/// id) followed by `promote_target` per participant under the granted
/// lease. Journaled; in-flight retries resolve by the witness lease
/// state plus the durably recorded device paths.
pub(crate) async fn grant(
    State(state): State<SharedState>,
    _peer: RequirePeer,
    ValidJson(req): ValidJson<PeerGrantRequest>,
) -> Result<Response, ApiErrorReply> {
    let ctx = peer_context(&state)?;
    let operation_id = ops::mobility_operation_id(&req.migration_id, "peer-grant")?;
    let body = to_json_value(&req)?;
    let hash = ops::mobility_request_hash("peer-grant", &body);
    tracing::info!(
        kind = ops::OP_PEER_GRANT,
        operation_id = %operation_id,
        migration_id = %req.migration_id,
        "accepting peer grant"
    );
    let inspect_ctx = Arc::clone(&ctx);
    let inspect_req = req.clone();
    let run_ctx = Arc::clone(&ctx);
    ops::execute_resolvable(
        &state,
        ops::OP_PEER_GRANT,
        operation_id,
        hash,
        body,
        None,
        ops::FailureReplay::Reissue,
        move || {
            let ctx = Arc::clone(&inspect_ctx);
            let req = inspect_req.clone();
            async move { grant_inspect(ctx, req).await }
        },
        move || {
            let ctx = Arc::clone(&run_ctx);
            async move { grant_act(ctx, req).await }
        },
    )
    .await
    .map_err(ApiErrorReply::from)
}

/// The grant act's inspection: the act is proven landed only when
/// **every** participant's witness lease is live and held by this host
/// **and** the preparation durably records every promoted device path
/// (the response needs them). Anything less reports "not landed" and
/// the act re-executes — safely: the witness batch runs under its
/// deterministic operation id (the witness journal replays the
/// recorded outcome) and `promote_target` is idempotent per migration.
async fn grant_inspect(
    ctx: Arc<PeerRouteContext>,
    req: PeerGrantRequest,
) -> Result<Option<PeerGrantResponse>, ApiError> {
    let Some(preparation) = ctx.preparations.load(&req.migration_id)? else {
        return Err(preparation_absent(&req.migration_id));
    };
    let mut outcomes = Vec::with_capacity(preparation.participants.len());
    for participant in &preparation.participants {
        let view = witness_view(&ctx.witness, participant.volume_id.clone()).await?;
        if view.lease_state != LeaseState::Live || view.holder.as_ref() != Some(&ctx.host_id) {
            return Ok(None);
        }
        let (Some(device_path), Some(lease_id), Some(remaining)) = (
            participant.device_path.clone(),
            view.lease_id,
            view.lease_remaining_secs,
        ) else {
            // A live lease without its identity, or a promotion whose
            // device path was not durably recorded: not provable, so
            // not proven — re-execute.
            return Ok(None);
        };
        outcomes.push(PeerGrantOutcome {
            volume_id: participant.volume_id.clone(),
            device_path,
            epoch: view.current_epoch.0,
            lease_id: lease_id.0,
            lease_ttl_secs: remaining,
        });
    }
    Ok(Some(PeerGrantResponse {
        migration_id: req.migration_id,
        grants: outcomes,
    }))
}

/// The grant act.
async fn grant_act(
    ctx: Arc<PeerRouteContext>,
    req: PeerGrantRequest,
) -> Result<PeerGrantResponse, ApiError> {
    let Some(preparation) = ctx.preparations.load(&req.migration_id)? else {
        return Err(preparation_absent(&req.migration_id));
    };
    // The witness batch operation id: derived over the ordered
    // participant volume set, exactly like the source's own batch ids,
    // so a re-drive replays the recorded outcome instead of minting a
    // redundant epoch.
    let witness_op = batch_operation_id(
        &req.migration_id,
        BatchStep::GrantSet,
        &id_only_participants(&preparation),
    )?;
    let grant_request = GrantSetRequest {
        protocol_version: WITNESS_PROTOCOL_VERSION,
        operation_id: witness_op,
        host_id: ctx.host_id.clone(),
        migration_id: Some(req.migration_id.clone()),
        requests: preparation
            .participants
            .iter()
            .map(|p| BatchGrantVolume {
                volume_id: p.volume_id.clone(),
            })
            .collect(),
    };
    let witness = Arc::clone(&ctx.witness);
    let grant_set =
        run_blocking(move || witness.grant_set(grant_request).map_err(witness_error)).await?;
    if grant_set.grants.len() != preparation.participants.len() {
        return Err(ApiError::new(
            ApiErrorCode::Internal,
            format!(
                "witness grant-set answered {} grants for {} participants \
                 (migration {})",
                grant_set.grants.len(),
                preparation.participants.len(),
                req.migration_id
            ),
        ));
    }
    let mut outcomes = Vec::with_capacity(preparation.participants.len());
    for (participant, granted) in preparation.participants.iter().zip(&grant_set.grants) {
        // The promote attach identity: deterministic per
        // (migration, volume), shared by the operation id and the
        // attachment id (see `attach_identity`).
        let identity = attach_identity(&req.migration_id, &participant.volume_id);
        let attach = AttachVolumeRequest {
            api_version: API_VERSION.to_owned(),
            operation_id: OperationId::new(identity.clone())?,
            vm_id: preparation.vm_id.clone(),
            host_id: ctx.host_id.clone(),
            attachment_id: AttachmentId::new(identity)?,
            // The expected volume generation is passed through from
            // the preparation: the promote path deliberately does not
            // enforce it (the volume's generation moved when the
            // source detached; the grant's authority is the witness
            // lease, not the attachment generation).
            expected_volume_generation: participant.expected_generation,
            access_mode: AccessModeRequest::SingleWriter,
            requested_frontend: None,
            // No VMM disk id crosses the promote seam: the
            // grow-notification mapping is the consumer's own attach
            // request on the destination host, not the migration's
            // business (P6-B).
            vmm_disk_id: None,
        };
        let promoted = ctx
            .handoff
            .promote_target(&participant.volume_id, &req.migration_id, &attach)
            .await?;
        let device_path = match promoted.frontend {
            Frontend::VirtioBlk { host_device_path } => host_device_path,
            Frontend::PciPassthrough { .. } => {
                return Err(ApiError::new(
                    ApiErrorCode::InvalidState,
                    format!(
                        "promoted participant {} carries a PCI-passthrough frontend; \
                         the coordinated handoff requires a virtio-blk device path",
                        participant.volume_id
                    ),
                ));
            }
        };
        // Durable before the outcome: an in-flight grant is resolvable
        // by inspection only when the device paths are on record.
        ctx.preparations.record_device_path(
            &req.migration_id,
            &participant.volume_id,
            &device_path,
        )?;
        outcomes.push(PeerGrantOutcome {
            volume_id: participant.volume_id.clone(),
            device_path,
            epoch: granted.epoch.0,
            lease_id: granted.lease_id.0,
            lease_ttl_secs: granted.lease_ttl_secs,
        });
    }
    Ok(PeerGrantResponse {
        migration_id: req.migration_id,
        grants: outcomes,
    })
}

// ---------------------------------------------------------------------------
// restore-vm
// ---------------------------------------------------------------------------

/// `POST /v2/internal/peer/restore-vm` — the destination's `VM_RESUMED`
/// act: restore the VM from the migration's snapshot into the
/// pre-started empty destination VMM (a half-restored VM is destroyed
/// first — the re-drive is idempotent), optionally resuming it.
/// Journaled; in-flight retries resolve by the observed VM state.
pub(crate) async fn restore_vm(
    State(state): State<SharedState>,
    _peer: RequirePeer,
    ValidJson(req): ValidJson<PeerRestoreVmRequest>,
) -> Result<Response, ApiErrorReply> {
    req.validate()?;
    let ctx = peer_context(&state)?;
    let operation_id = ops::mobility_operation_id(&req.migration_id, "peer-restore-vm")?;
    let body = to_json_value(&req)?;
    let hash = ops::mobility_request_hash("peer-restore-vm", &body);
    tracing::info!(
        kind = ops::OP_PEER_RESTORE_VM,
        operation_id = %operation_id,
        migration_id = %req.migration_id,
        resume = req.resume,
        "accepting peer restore-vm"
    );
    let inspect_ctx = Arc::clone(&ctx);
    let inspect_req = req.clone();
    let run_ctx = Arc::clone(&ctx);
    ops::execute_resolvable(
        &state,
        ops::OP_PEER_RESTORE_VM,
        operation_id,
        hash,
        body,
        None,
        ops::FailureReplay::Reissue,
        move || {
            let ctx = Arc::clone(&inspect_ctx);
            let req = inspect_req.clone();
            async move { restore_inspect(ctx, req).await }
        },
        move || {
            let ctx = Arc::clone(&run_ctx);
            async move { restore_act(ctx, req).await }
        },
    )
    .await
    .map_err(ApiErrorReply::from)
}

/// Verify the request's disk mappings against the preparation: every
/// participant's promoted device path must appear exactly once, and no
/// foreign path may — the restore opens what the migration promoted,
/// never a path a caller smuggled in.
fn verify_disk_mappings(
    preparation: &TargetPreparation,
    req: &PeerRestoreVmRequest,
) -> Result<(), ApiError> {
    let mut expected: Vec<&str> = Vec::with_capacity(preparation.participants.len());
    for participant in &preparation.participants {
        let Some(device_path) = participant.device_path.as_deref() else {
            return Err(ApiError::new(
                ApiErrorCode::InvalidState,
                format!(
                    "participant {} of migration {} has no promoted device path on \
                     record; the grant act must land before the restore",
                    participant.volume_id, req.migration_id
                ),
            ));
        };
        expected.push(device_path);
    }
    let mut presented: Vec<&str> = req.disks.iter().map(|d| d.device_path.as_str()).collect();
    presented.sort_unstable();
    expected.sort_unstable();
    if presented != expected {
        return Err(ApiError::new(
            ApiErrorCode::InvalidState,
            format!(
                "the restore's disk mappings do not match migration {}'s promoted \
                 participant set (device paths differ)",
                req.migration_id
            ),
        ));
    }
    Ok(())
}

/// The restore act's inspection: a `Running` VM proves the act for
/// both resume modes' forward goal; a `Paused` VM proves a completed
/// no-resume restore. Anything else reports "not landed" and the act
/// re-executes (idempotently: restore into an absent VMM, destroy a
/// half-restored one first).
async fn restore_inspect(
    ctx: Arc<PeerRouteContext>,
    req: PeerRestoreVmRequest,
) -> Result<Option<PeerRestoreVmResponse>, ApiError> {
    let Some(preparation) = ctx.preparations.load(&req.migration_id)? else {
        return Err(preparation_absent(&req.migration_id));
    };
    verify_disk_mappings(&preparation, &req)?;
    let observed = vmm_state(&ctx.vmm, &preparation.vm_id).await?;
    match observed {
        VmState::Running => Ok(Some(PeerRestoreVmResponse {
            migration_id: req.migration_id,
            vm_state: VmState::Running,
        })),
        VmState::Paused if !req.resume => Ok(Some(PeerRestoreVmResponse {
            migration_id: req.migration_id,
            vm_state: VmState::Paused,
        })),
        _ => Ok(None),
    }
}

/// The restore act.
async fn restore_act(
    ctx: Arc<PeerRouteContext>,
    req: PeerRestoreVmRequest,
) -> Result<PeerRestoreVmResponse, ApiError> {
    let Some(preparation) = ctx.preparations.load(&req.migration_id)? else {
        return Err(preparation_absent(&req.migration_id));
    };
    verify_disk_mappings(&preparation, &req)?;
    let vm_id = preparation.vm_id.clone();
    let snapshot_dir = PathBuf::from(&req.snapshot_dir);
    let disks = req.disks.clone();
    let mut observed = vmm_state(&ctx.vmm, &vm_id).await?;
    match (req.resume, observed) {
        // The resume path's already-complete shapes: a paused VM only
        // needs the resume below, a running one is the act's goal; an
        // empty VMM is the clean restore path.
        (_, VmState::Absent) | (true, VmState::Paused | VmState::Running) => {}
        // Everything else present on the socket is this migration's
        // own half-restore — a crash between define and boot leaves
        // exactly the `Created` shape, a crashed restore a `Paused`
        // one — and is destroyed first (plan §3's re-drive rule, row
        // 18), on both resume flavors. The foreign-VM guard is the
        // prepare act's emptiness verification: the socket was proven
        // empty before the source's cut, so whatever appeared since
        // is this migration's own doing. One call converges: after
        // the destroy the act continues into the restore below.
        _ => {
            let vmm = Arc::clone(&ctx.vmm);
            let vm = vm_id.clone();
            run_blocking(move || vmm.destroy(&vm)).await?;
            observed = vmm_state(&ctx.vmm, &vm_id).await?;
            if observed != VmState::Absent {
                return Err(ApiError::new(
                    ApiErrorCode::InvalidState,
                    format!(
                        "destination VMM of VM {vm_id} still reports {observed:?} after the \
                         half-restore was destroyed; refusing to guess",
                    ),
                ));
            }
        }
    }
    if observed == VmState::Absent {
        let vmm = Arc::clone(&ctx.vmm);
        let vm = vm_id.clone();
        let dir = snapshot_dir.clone();
        let restore_disks = disks.clone();
        run_blocking(move || vmm.restore(&vm, &dir, &restore_disks)).await?;
    }
    if req.resume && observed != VmState::Running {
        let vmm = Arc::clone(&ctx.vmm);
        let vm = vm_id.clone();
        run_blocking(move || vmm.resume(&vm)).await?;
    }
    let final_state = vmm_state(&ctx.vmm, &vm_id).await?;
    Ok(PeerRestoreVmResponse {
        migration_id: req.migration_id,
        vm_state: final_state,
    })
}

// ---------------------------------------------------------------------------
// discard
// ---------------------------------------------------------------------------

/// `POST /v2/internal/peer/discard` — the pre-cut abort tail's
/// destination half: drop the target preparation (the additive fifth
/// route; see the module docs). Journaled; idempotent — an absent
/// preparation is the proven "already discarded" answer.
pub(crate) async fn discard(
    State(state): State<SharedState>,
    _peer: RequirePeer,
    ValidJson(req): ValidJson<PeerDiscardRequest>,
) -> Result<Response, ApiErrorReply> {
    let ctx = peer_context(&state)?;
    let operation_id = ops::mobility_operation_id(&req.migration_id, "peer-discard")?;
    let body = to_json_value(&req)?;
    let hash = ops::mobility_request_hash("peer-discard", &body);
    tracing::info!(
        kind = ops::OP_PEER_DISCARD,
        operation_id = %operation_id,
        migration_id = %req.migration_id,
        "accepting peer discard"
    );
    let inspect_ctx = Arc::clone(&ctx);
    let inspect_req = req.clone();
    let run_ctx = Arc::clone(&ctx);
    ops::execute_resolvable(
        &state,
        ops::OP_PEER_DISCARD,
        operation_id,
        hash,
        body,
        None,
        ops::FailureReplay::Reissue,
        move || {
            let ctx = Arc::clone(&inspect_ctx);
            let req = inspect_req.clone();
            async move { discard_inspect(ctx, req) }
        },
        move || {
            let ctx = Arc::clone(&run_ctx);
            async move { discard_act(ctx, req) }
        },
    )
    .await
    .map_err(ApiErrorReply::from)
}

/// The discard act's inspection: an absent preparation proves the act
/// landed (nothing to discard); a present one reports "not landed".
/// (Synchronous: the preparation store is the only input.)
fn discard_inspect(
    ctx: Arc<PeerRouteContext>,
    req: PeerDiscardRequest,
) -> Result<Option<PeerDiscardResponse>, ApiError> {
    match ctx.preparations.load(&req.migration_id)? {
        None => Ok(Some(PeerDiscardResponse {
            migration_id: req.migration_id,
            discarded: false,
        })),
        Some(_) => Ok(None),
    }
}

/// The discard act. (Synchronous: the store removal is the whole
/// act.)
fn discard_act(
    ctx: Arc<PeerRouteContext>,
    req: PeerDiscardRequest,
) -> Result<PeerDiscardResponse, ApiError> {
    let discarded = ctx.preparations.remove(&req.migration_id)?;
    Ok(PeerDiscardResponse {
        migration_id: req.migration_id,
        discarded,
    })
}

// ---------------------------------------------------------------------------
// health
// ---------------------------------------------------------------------------

/// `GET /v2/internal/peer/health` — the honest destination probe: this
/// host's identity and whether the snapshot root is usable right now
/// (write+read-back+remove). Read-only with respect to authority
/// state; never journaled (no mutation).
pub(crate) async fn health(
    State(state): State<SharedState>,
    _peer: RequirePeer,
) -> Result<Response, ApiErrorReply> {
    let ctx = peer_context(&state)?;
    let body = to_json_value(&PeerHealthResponse {
        host_id: ctx.host_id.clone(),
        snapshot_dir: ctx.snapshot_root.display().to_string(),
        snapshot_dir_readable: probe_dir(&ctx.snapshot_root),
    })?;
    Ok(json_response(StatusCode::OK, &body))
}
