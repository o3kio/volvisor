//! VMM coordination for the coordinated VMM/storage handoff (P4b
//! plan §5): the engine-neutral [`VmmController`] trait the migration
//! coordinator drives, the Cloud Hypervisor `ch-remote` adapter
//! ([`ChRemoteVmm`]) and the TEST-ONLY [`FakeVmm`] harness that makes
//! the device-open discipline provable in tests (AGENTS rule 17).
//!
//! # The verified `ch-remote` command surface
//!
//! Every command [`ChRemoteVmm`] issues exists in the upstream
//! `ch-remote` surface, verified against the cloud-hypervisor docs
//! 2026-10-09 (P4b plan §5). The adapter pins exactly this list and
//! issues nothing else:
//!
//! | Step | argv tail (after `--api-socket S`) | Verified fact |
//! |---|---|---|
//! | pause | `pause` | `/vm.pause` requires the VM booted; the command returns after the pause completes |
//! | pause proof | `info` | `/vm.info` reports the VM state; the adapter requires `Paused` |
//! | snapshot | `snapshot file://DIR` | `/vm.snapshot` requires the VM paused; writes `config.json`, `memory-ranges`, `state.json` |
//! | destroy | `delete` | `/vm.delete` has no prerequisites — legal on a paused VM; releases the VM's devices |
//! | restore | `restore source_url=file://DIR` | `/vm.restore` runs on a pre-started empty VMM; the restored VM lands paused; `config.json` may be adjusted between snapshot and restore |
//! | resume | `resume` | `/vm.resume` requires the VM paused |
//!
//! Socket paths follow the configured convention
//! ([`ChRemoteConfig::api_socket_dir`]): one API socket per VM,
//! `{api_socket_dir}/{vm_id}.sock`. Every command runs through an
//! injected [`CommandRunner`] (`RealRunner` in production,
//! `FakeRunner`/custom runners in tests) — the established
//! argv-verified, timeout-guarded, shell-free execution path.
//!
//! # The resize-disk exception (P6-B)
//!
//! One operation is **not** a `ch-remote` command:
//! [`VmmController::resize_disk`]. Verified against
//! cloud-hypervisor v37.0's published command list, `ch-remote` has
//! no `resize-disk` subcommand — disk resize is REST-API-only
//! (`PUT /api/v1/vm.resize-disk` over the same `--api-socket`, JSON
//! body per the `VmResizeDisk` schema). The adapter issues that call
//! through [`crate::vmm_http`] — a hand-rolled HTTP/1.1 `PUT` over
//! the unix domain socket, byte-pinned the same way the argv table
//! above pins the commands (ADR-0006 first slice part 1; the
//! argv-exact discipline translates to byte-exact HTTP assertions
//! for this call).
//!
//! Not used, with reasons (recorded so a future change must confront
//! them, not rediscover them):
//!
//! - `send-migration` / `receive-migration`: the receiving VMM opens
//!   every destination disk when the transfer starts, which requires
//!   a writable destination device before the authority transfer —
//!   exactly the temporary dual-primary shape the v2 contract
//!   forbids by default (plan §1 out of scope, AGENTS rule 17);
//! - `shutdown` / `power-button`: both need a running, cooperating
//!   guest (ACPI); `delete` is the only deterministic device release
//!   (plan §2 D1).
//!
//! # `vm.info` state mapping
//!
//! The real `vm.info` states map onto [`VmState`] as follows:
//!
//! - `Created` → [`VmState::Created`];
//! - `Running` → [`VmState::Running`];
//! - `Paused` → [`VmState::Paused`];
//! - `Shutoff` → [`VmState::Created`] — a defined, not-running VM.
//!   The choice is deliberate and conservative: `Shutoff` and
//!   `Created` are both "the VM exists but is not running", and for
//!   every decision the cutover makes of [`VmState`] that shape is
//!   equivalent (a pause requires a booted VM; the crash-reconcile's
//!   destroy re-drive handles *any* present state the same way). The
//!   adapter never asserts anything stronger from a `Shutoff`
//!   observation — in particular nothing about device open/close,
//!   which the storage side observes for itself.
//! - `Absent` is not a `vm.info` state: it is the API-not-found error
//!   on the socket (see the matching rule below).
//!
//! # The `Absent` matching rule
//!
//! `ch-remote info` exits nonzero with a characteristic message when
//! the VMM behind the socket holds no VM. The adapter's rule is
//! explicit and conservative — only an enumerated not-found shape
//! reads as [`VmState::Absent`]: the command's flattened stderr
//! excerpt, lowercased, must contain one of the tokens pinned in
//! `NOT_FOUND_TOKENS` ("not found", "no vm", "not initialized",
//! "vm not created"). Every other failure — including a socket
//! that cannot be connected (a dead VMM process, which must stall the
//! migration `IN_DOUBT` with a typed detail, plan §3, never read as
//! "no VM") — surfaces as a typed error. The token list is modeled on
//! the verified surface and is pinned against real `ch-remote`
//! output by the env-gated `VOLVISOR_TEST_CH` campaign (plan §9, a
//! later slice).
//!
//! # Re-drive safety
//!
//! Two trait operations are crash-re-drive-safe by contract (the
//! coordinator's forward-only cut reconcile depends on both, plan
//! §3):
//!
//! - [`VmmController::destroy`] of an already-`Absent` VM is a no-op
//!   that succeeds **without issuing `delete`** — the absence is
//!   observed through `vm.info`, never assumed;
//! - [`VmmController::restore`] refuses a non-empty VMM typed
//!   (`INVALID_STATE`): the coordinator destroys the half-restored VM
//!   first (plan §3); the adapter never destroys on the caller's
//!   behalf inside `restore`.
//!
//! # Honesty boundary
//!
//! This is a verified command surface, not exercised against a real
//! cloud-hypervisor in CI: no production-support claim is made here
//! (AGENTS rule 12). Nothing in this module asserts storage safety
//! from VMM state alone (AGENTS rule 6): [`PauseProof`] is a VMM-side
//! observation, the storage boundary is the provider's own suspension
//! (plan §2 D2), and the source demote re-observes the device closed
//! from the storage side (`drbdsetup status`) — never from `delete`'s
//! exit status.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use volvisor_types::{ApiError, ApiErrorCode};

use crate::runner::{CommandOutput, CommandRunner, STDERR_EXCERPT_MAX_CHARS};

/// The characteristic stderr tokens (lowercased substring match on
/// the flattened excerpt) of a `ch-remote info` failure that means
/// "the VMM behind the socket holds no VM" — the only failures the
/// adapter maps to [`VmState::Absent`].
///
/// The list is deliberately short and explicit: a failure that does
/// not match one of these tokens surfaces typed (a dead VMM socket
/// must never read as "no VM" — plan §3). Modeled on the verified
/// surface; pinned against real `ch-remote` output by the
/// `VOLVISOR_TEST_CH` campaign (a later slice).
const NOT_FOUND_TOKENS: [&str; 4] = ["not found", "no vm", "not initialized", "vm not created"];

/// The per-call budget of the resize-disk HTTP exchange (the module
/// docs' resize-disk exception): the same order as the control-path
/// HTTP bounds (the peer and witness request timeouts) — generous
/// against a healthy VMM, bounded against a wedged one.
const RESIZE_DISK_TIMEOUT: Duration = Duration::from_secs(5);

/// The VMM-side state of one VM, as the migration coordinator needs
/// it (plan §5): the engine-neutral vocabulary behind
/// [`VmmController::state`].
///
/// `Absent` is not a `vm.info` state — it is the API-not-found error
/// on the socket (see the module docs' matching rule). The adapter
/// maps the real `vm.info` states `Created|Running|Paused|Shutoff`
/// onto the three present variants, with `Shutoff` → `Created` (a
/// defined, not-running VM — the mapping and its reason are recorded
/// in the module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VmState {
    /// The VMM behind the socket holds no VM (not-found).
    Absent,
    /// The VM is defined but not running (`Created`, `Shutoff`).
    Created,
    /// The VM is booted and running.
    Running,
    /// The VM is booted and paused (the pause is a first-class fact
    /// the cutover verifies before every barrier act).
    Paused,
}

impl fmt::Display for VmState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Absent => "Absent",
            Self::Created => "Created",
            Self::Running => "Running",
            Self::Paused => "Paused",
        })
    }
}

/// Proof that one VM is paused: the **observed** `Paused` state from
/// `vm.info` (plan §5).
///
/// This is volvisor's own verification — the adapter runs `pause`,
/// then `info`, and only a `Paused` observation produces a proof —
/// never the consumer's attestation and never the pause command's
/// exit status alone. `state` is therefore always [`VmState::Paused`]
/// on a successful return; the field exists so the proof is
/// self-describing about what was observed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PauseProof {
    /// The VM the proof was observed for.
    pub vm_id: String,
    /// The observed state (always [`VmState::Paused`] in a proof —
    /// anything else is a typed refusal, never a proof).
    pub state: VmState,
    /// Local unix time of the `vm.info` observation.
    pub observed_at: u64,
}

/// One disk of a snapshot restore: the snapshot config's declared
/// path on the source host → the target host's promoted device path.
///
/// The adapter rewrites the copied `config.json` when they differ
/// (DRBD minors are symmetric across the replication pair in the
/// common case, so the rewrite is usually a no-op — but it is
/// verified, never assumed). A mapping whose declared path matches no
/// disk in the snapshot config is a typed refusal: the migration
/// believes the VM has this disk and the snapshot disagrees — never a
/// guess.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiskMapping {
    /// The disk path declared in the snapshot's `config.json` (what
    /// the source VM had open).
    pub declared_path: String,
    /// The device path the target host's promoted replica presents
    /// (what the restored VM must open).
    pub device_path: String,
}

/// The VMM coordination seam of the coordinated handoff (P4b plan
/// §5): what the migration coordinator drives on both hosts.
///
/// Engine-neutral by design (ADR-0007's boundary): the Cloud
/// Hypervisor adapter ([`ChRemoteVmm`]) and the TEST-ONLY
/// [`FakeVmm`] both implement it. Synchronous like
/// [`CommandRunner`] — the DRBD provider's handoff surface runs its
/// commands synchronously through the runner, and the coordinator
/// wraps this seam in its own retry/reconcile tasks.
///
/// Implementations must honor the module's re-drive-safety rules
/// (`destroy` of an absent VM is a verified no-op; `restore` refuses
/// a non-empty VMM typed) and fail closed on every unprovable step.
pub trait VmmController: Send + Sync {
    /// Pause one VM and **prove** it: run the pause, then verify
    /// `Paused` from `vm.info` (the proof is volvisor's own
    /// observation, never the command's exit status).
    ///
    /// # Errors
    /// Returns [`ApiError`] typed: `NOT_FOUND` when the VMM holds no
    /// VM, `INVALID_STATE` when the VM is not booted (`/vm.pause`
    /// requires a booted VM) or when the pause could not be verified
    /// by observation, `INTERNAL` for execution failures.
    fn pause(&self, vm_id: &str) -> Result<PauseProof, ApiError>;

    /// Snapshot one paused VM into `dir`
    /// (`ch-remote snapshot file://DIR`, which writes `config.json`,
    /// `memory-ranges`, `state.json`). `dir` is the migration's
    /// snapshot directory as the coordinator configures it
    /// (absolute — the module docs' URL note).
    ///
    /// The adapter relies on the coordinator's ordering (a verified
    /// pause proof precedes this call) but does not re-check it:
    /// cloud-hypervisor itself refuses an unpaused snapshot, so an
    /// ordering violation surfaces as a typed command refusal here —
    /// recorded, not papered over.
    ///
    /// # Errors
    /// Returns [`ApiError`] typed: `NOT_FOUND` when the VMM holds no
    /// VM, `INVALID_STATE`/`INTERNAL` when the command is refused or
    /// fails (the excerpt is carried).
    fn snapshot(&self, vm_id: &str, dir: &Path) -> Result<(), ApiError>;

    /// Destroy one VM (`delete`) — the deterministic device release
    /// (plan §2 D1). Re-drive-safe: an already-`Absent` VM succeeds
    /// as a **no-op** without issuing `delete` (the crash-reconcile
    /// dependency; the absence is observed, never assumed).
    ///
    /// # Errors
    /// Returns [`ApiError`] typed: `NOT_FOUND` when the VMM socket
    /// answers not-found for a destroy that was attempted, or
    /// `INTERNAL` when a `delete` of a present VM fails.
    fn destroy(&self, vm_id: &str) -> Result<(), ApiError>;

    /// Restore one VM from `dir` into a **pre-started empty VMM**
    /// (`restore source_url=file://DIR`); the restored VM lands
    /// paused. The copied `config.json`'s declared disk paths are
    /// rewritten per `disks` **before** the restore (verified by
    /// re-reading, never assumed).
    ///
    /// Refuses a non-empty VMM typed: the coordinator destroys the
    /// half-restored VM first (plan §3) — this method never destroys
    /// on the caller's behalf.
    ///
    /// # Errors
    /// Returns [`ApiError`] typed: `INVALID_STATE` for a non-empty
    /// VMM or an unexpected snapshot-config structure (including a
    /// mapping that matches no config disk — never a guess),
    /// `NOT_FOUND` for an unreadable snapshot dir/config, `INTERNAL`
    /// for execution failures.
    fn restore(&self, vm_id: &str, dir: &Path, disks: &[DiskMapping]) -> Result<(), ApiError>;

    /// Resume one paused VM (`resume`; `/vm.resume` requires paused —
    /// cloud-hypervisor enforces it, and the refusal surfaces typed
    /// here).
    ///
    /// # Errors
    /// Returns [`ApiError`] typed: `NOT_FOUND` when the VMM holds no
    /// VM, `INVALID_STATE`/`INTERNAL` when the command is refused or
    /// fails (the excerpt is carried).
    fn resume(&self, vm_id: &str) -> Result<(), ApiError>;

    /// The observed state of one VM
    /// (`Absent | Created | Running | Paused`).
    ///
    /// # Errors
    /// Returns [`ApiError`] typed only for real observation failures
    /// (an unreachable socket, unparseable output): `Absent` is a
    /// **result** (the not-found shape), never an error, and never a
    /// fallback for an unrecognized failure.
    fn state(&self, vm_id: &str) -> Result<VmState, ApiError>;

    /// Resize one disk of one VM through the VMM's resize-disk API
    /// (P6-B, ADR-0006 first slice part 1): `PUT
    /// /api/v1/vm.resize-disk` over the VM's api-socket with the
    /// `VmResizeDisk` body — `{"id": <disk_id>, "new_size":
    /// <new_size_bytes>}` (`slot` is optional in the schema and
    /// omitted). The capacity-notification step of an online grow on
    /// an attached volume.
    ///
    /// Re-drive-safe for the notification engine's retry: re-issuing
    /// the same (disk, size) is a no-op on the VMM side. The
    /// never-shrink rule (contract §4A: a failed notification is
    /// retried, the backing is never shrunk to undo) is the
    /// **caller's** contract — this seam executes the resize it is
    /// handed and cannot know the disk's current size.
    ///
    /// # Errors
    /// Returns [`ApiError`] typed: `NOT_FOUND` when the VMM holds no
    /// VM, `INVALID_STATE` when the VMM answers a non-2xx status
    /// (the status is carried), `INTERNAL` for transport failures
    /// (an unreachable or wedged socket, an unparseable response).
    fn resize_disk(&self, vm_id: &str, disk_id: &str, new_size_bytes: u64) -> Result<(), ApiError>;
}

/// Configuration of the Cloud Hypervisor `ch-remote` adapter.
///
/// Mirrors the daemon's `[vmm]` table (plan §6): the binary path and
/// the directory holding one API socket per VM
/// (`{api_socket_dir}/{vm_id}.sock`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChRemoteConfig {
    /// Path of the `ch-remote` binary (e.g. `/usr/bin/ch-remote`).
    pub ch_remote_bin: PathBuf,
    /// Directory holding the per-VM API sockets; the socket for
    /// `vm_id` is `{api_socket_dir}/{vm_id}.sock`.
    pub api_socket_dir: PathBuf,
}

/// The Cloud Hypervisor adapter over `ch-remote` (P4b plan §5).
///
/// Every command runs through the injected [`CommandRunner`] with the
/// argv the verified table pins (module docs); the adapter adds
/// nothing to the surface — no `send-migration`, no `shutdown`, no
/// capability probing. The one exception is
/// [`VmmController::resize_disk`]: the REST-only resize-disk call
/// (module docs' resize-disk exception), issued as HTTP/1.1 over the
/// same api-socket. Verified command surface, not exercised
/// against a real cloud-hypervisor in CI (module docs' honesty
/// boundary).
pub struct ChRemoteVmm {
    config: ChRemoteConfig,
    runner: Arc<dyn CommandRunner>,
}

impl ChRemoteVmm {
    /// Build the adapter over `runner`.
    #[must_use]
    pub fn new(config: ChRemoteConfig, runner: Arc<dyn CommandRunner>) -> Self {
        Self { config, runner }
    }

    /// The API socket path for `vm_id`:
    /// `{api_socket_dir}/{vm_id}.sock`.
    #[must_use]
    pub fn socket_path(&self, vm_id: &str) -> PathBuf {
        self.config.api_socket_dir.join(format!("{vm_id}.sock"))
    }

    /// Run one `ch-remote` command against `vm_id`'s socket.
    ///
    /// The argv is exactly `ch-remote --api-socket S <args...>` — the
    /// runner is the established shell-free execution path, so a
    /// socket path or URL derived from identifiers can never be
    /// re-parsed as shell syntax.
    fn run_ch(&self, vm_id: &str, args: &[&str]) -> Result<CommandOutput, ApiError> {
        let socket = self.socket_path(vm_id).to_string_lossy().into_owned();
        let program = self.config.ch_remote_bin.to_string_lossy().into_owned();
        let mut invocation: Vec<&str> = vec!["--api-socket", socket.as_str()];
        invocation.extend_from_slice(args);
        self.runner.run(&program, &invocation)
    }

    /// Run `vm.info` and classify the observation.
    ///
    /// `Ok(None)` is the not-found shape (the socket answers but
    /// holds no VM — the only input mapped to [`VmState::Absent`]);
    /// `Ok(Some(state))` is the parsed state; `Err` is a failure that
    /// must not be guessed from (an unreachable socket, unparseable
    /// output).
    fn query_state(&self, vm_id: &str) -> Result<Option<VmState>, ApiError> {
        let output = self.run_ch(vm_id, &["info"])?;
        if !output.success {
            if is_not_found_failure(&output) {
                return Ok(None);
            }
            return Err(ApiError::new(
                ApiErrorCode::Internal,
                format!(
                    "ch-remote info for VM {vm_id} failed: {}",
                    output.stderr_excerpt()
                ),
            ));
        }
        match parse_info_state(&output.stdout) {
            Ok(state) => Ok(Some(state)),
            Err(detail) => Err(ApiError::new(
                ApiErrorCode::Internal,
                format!("ch-remote info for VM {vm_id}: {detail}"),
            )),
        }
    }

    /// Map one failed `ch-remote` command to a typed refusal.
    ///
    /// A not-found-shaped failure is `NOT_FOUND`; a failure of a
    /// command with a known verified precondition is `INVALID_STATE`
    /// (the requirement is named, the excerpt carried); anything else
    /// is `INTERNAL` — no state claims, never a silent success.
    fn command_refusal(
        &self,
        command: &str,
        vm_id: &str,
        requirement: Option<&str>,
        output: &CommandOutput,
    ) -> ApiError {
        if is_not_found_failure(output) {
            return ApiError::not_found(format!(
                "no VM {vm_id} on socket {} (ch-remote {command}: {})",
                self.socket_path(vm_id).display(),
                output.stderr_excerpt()
            ));
        }
        match requirement {
            Some(requirement) => ApiError::new(
                ApiErrorCode::InvalidState,
                format!(
                    "ch-remote {command} for VM {vm_id} refused ({requirement}): {}",
                    output.stderr_excerpt()
                ),
            ),
            None => ApiError::new(
                ApiErrorCode::Internal,
                format!(
                    "ch-remote {command} for VM {vm_id} failed: {}",
                    output.stderr_excerpt()
                ),
            ),
        }
    }
}

impl VmmController for ChRemoteVmm {
    fn pause(&self, vm_id: &str) -> Result<PauseProof, ApiError> {
        let output = self.run_ch(vm_id, &["pause"])?;
        if !output.success {
            return Err(self.command_refusal(
                "pause",
                vm_id,
                Some("/vm.pause requires the VM booted"),
                &output,
            ));
        }
        // The proof: volvisor's own vm.info observation, never the
        // pause command's exit status.
        let output = self.run_ch(vm_id, &["info"])?;
        if !output.success {
            return Err(ApiError::new(
                ApiErrorCode::Internal,
                format!(
                    "pause of VM {vm_id} could not be verified: ch-remote info failed: {}",
                    output.stderr_excerpt()
                ),
            ));
        }
        let observed = parse_info_state(&output.stdout).map_err(|detail| {
            ApiError::new(
                ApiErrorCode::Internal,
                format!("pause of VM {vm_id} could not be verified: {detail}"),
            )
        })?;
        if observed != VmState::Paused {
            return Err(ApiError::new(
                ApiErrorCode::InvalidState,
                format!("pause of VM {vm_id} not verified: vm.info reports {observed}"),
            ));
        }
        Ok(PauseProof {
            vm_id: vm_id.to_owned(),
            state: VmState::Paused,
            observed_at: now_unix(),
        })
    }

    fn snapshot(&self, vm_id: &str, dir: &Path) -> Result<(), ApiError> {
        let url = file_url(dir);
        let output = self.run_ch(vm_id, &["snapshot", &url])?;
        if !output.success {
            return Err(self.command_refusal(
                "snapshot",
                vm_id,
                Some("/vm.snapshot requires the VM paused"),
                &output,
            ));
        }
        Ok(())
    }

    fn destroy(&self, vm_id: &str) -> Result<(), ApiError> {
        // Re-drive safety (plan §5): an already-absent VM is a no-op
        // and no `delete` is issued. The absence is *observed* through
        // vm.info — a dead socket (an unresolvable stall) surfaces
        // typed here, never as a silent no-op.
        if self.query_state(vm_id)?.is_none() {
            return Ok(());
        }
        let output = self.run_ch(vm_id, &["delete"])?;
        if !output.success {
            return Err(self.command_refusal("delete", vm_id, None, &output));
        }
        Ok(())
    }

    fn restore(&self, vm_id: &str, dir: &Path, disks: &[DiskMapping]) -> Result<(), ApiError> {
        // A non-empty VMM is a typed refusal: the coordinator destroys
        // the half-restored VM first (plan §3). The adapter never
        // destroys on the caller's behalf inside restore.
        if let Some(state) = self.query_state(vm_id)? {
            return Err(ApiError::new(
                ApiErrorCode::InvalidState,
                format!(
                    "restore of VM {vm_id} refuses a non-empty VMM \
                     (vm.info reports {state}); the coordinator destroys the \
                     half-restored VM first (plan §3)"
                ),
            ));
        }
        rewrite_disk_paths(dir, disks)?;
        let source_url = format!("source_url={}", file_url(dir));
        let output = self.run_ch(vm_id, &["restore", &source_url])?;
        if !output.success {
            return Err(self.command_refusal("restore", vm_id, None, &output));
        }
        Ok(())
    }

    fn resume(&self, vm_id: &str) -> Result<(), ApiError> {
        let output = self.run_ch(vm_id, &["resume"])?;
        if !output.success {
            return Err(self.command_refusal(
                "resume",
                vm_id,
                Some("/vm.resume requires the VM paused"),
                &output,
            ));
        }
        Ok(())
    }

    fn state(&self, vm_id: &str) -> Result<VmState, ApiError> {
        match self.query_state(vm_id)? {
            Some(state) => Ok(state),
            // The not-found shape is a result, never an error and
            // never a fallback for an unrecognized failure (the
            // module docs' matching rule).
            None => Ok(VmState::Absent),
        }
    }

    fn resize_disk(&self, vm_id: &str, disk_id: &str, new_size_bytes: u64) -> Result<(), ApiError> {
        // The module docs' resize-disk exception: REST-only, over the
        // same api-socket, with the VmResizeDisk body byte-pinned
        // (the argv-exact discipline translated to HTTP bytes). The
        // disk id is the attachment-recorded VMM device id (P6-B) —
        // the body addresses the VMM's own device identity, never a
        // host path.
        let body = json!({ "id": disk_id, "new_size": new_size_bytes }).to_string();
        let response = crate::vmm_http::put_json(
            &self.socket_path(vm_id),
            "/api/v1/vm.resize-disk",
            &body,
            RESIZE_DISK_TIMEOUT,
        )?;
        if (200..300).contains(&response.status) {
            // 204 No Content is the verified success shape; every 2xx
            // is a success (the transport module is deliberately not
            // privy to which).
            return Ok(());
        }
        Err(ApiError::new(
            ApiErrorCode::InvalidState,
            format!(
                "vm.resize-disk for VM {vm_id} (disk {disk_id}, {new_size_bytes} bytes) refused: \
                 HTTP {} {}: {}",
                response.status,
                response.reason,
                response_excerpt(&response.rest)
            ),
        ))
    }
}

/// A short, flattened excerpt of a resize-disk response's remainder
/// (headers and any body) for error details — the
/// [`CommandOutput::stderr_excerpt`] discipline applied to HTTP.
fn response_excerpt(rest: &str) -> String {
    rest.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(STDERR_EXCERPT_MAX_CHARS)
        .collect()
}

/// Whether a failed command output carries the not-found shape (the
/// module docs' matching rule: one of [`NOT_FOUND_TOKENS`],
/// case-insensitive, against the flattened stderr excerpt).
fn is_not_found_failure(output: &CommandOutput) -> bool {
    let flattened = output.stderr_excerpt().to_lowercase();
    NOT_FOUND_TOKENS
        .iter()
        .any(|token| flattened.contains(token))
}

/// Parse the `vm.info` output into [`VmState`] (structure-checked,
/// fail-closed: an unrecognized state is an error, never a guess).
fn parse_info_state(stdout: &str) -> Result<VmState, String> {
    let value: Value = serde_json::from_str(stdout)
        .map_err(|error| format!("vm.info output is not valid JSON: {error}"))?;
    let state = value
        .get("state")
        .and_then(Value::as_str)
        .ok_or_else(|| "vm.info output carries no string \"state\" field".to_owned())?;
    match state {
        "Created" | "Shutoff" => Ok(VmState::Created),
        "Running" => Ok(VmState::Running),
        "Paused" => Ok(VmState::Paused),
        other => Err(format!(
            "vm.info reports an unrecognized state {other:?} \
             (verified surface: Created | Running | Paused | Shutoff)"
        )),
    }
}

/// The `file://DIR` URL spelling of the verified argv (snapshot and
/// restore).
///
/// `DIR` is the migration's snapshot directory as the coordinator
/// configures it (absolute — the plan §6 `snapshot_dir` root joined
/// with the migration id); a relative path would not form a URL
/// `ch-remote` can resolve, and the resulting refusal surfaces typed
/// through the command's own failure.
fn file_url(dir: &Path) -> String {
    format!("file://{}", dir.display())
}

/// Rewrite the snapshot config's declared disk paths per `disks`
/// (plan §5: usually a no-op because DRBD minors are symmetric — but
/// verified, never assumed).
///
/// Reads `{dir}/config.json` as a lossless [`Value`] (CH's full
/// config schema is deliberately **not** modeled — only the `disks`
/// array's `path` fields are edited), writes it back only when
/// something changed, then re-reads and confirms every mapping's
/// device path is what the config carries. An unexpected structure is
/// a typed refusal, never a guess.
fn rewrite_disk_paths(dir: &Path, disks: &[DiskMapping]) -> Result<(), ApiError> {
    let config_path = dir.join("config.json");
    let raw = fs::read_to_string(&config_path).map_err(|error| {
        ApiError::not_found(format!(
            "snapshot config {} unreadable: {error}",
            config_path.display()
        ))
    })?;
    let mut value: Value = serde_json::from_str(&raw).map_err(|error| {
        ApiError::new(
            ApiErrorCode::InvalidState,
            format!(
                "snapshot config {} is not valid JSON: {error}",
                config_path.display()
            ),
        )
    })?;
    let changed = apply_disk_mappings(&mut value, disks)?;
    if changed {
        let rewritten = serde_json::to_string_pretty(&value).map_err(|error| {
            ApiError::new(
                ApiErrorCode::Internal,
                format!(
                    "snapshot config {} could not be serialized: {error}",
                    config_path.display()
                ),
            )
        })?;
        fs::write(&config_path, rewritten).map_err(|error| {
            ApiError::new(
                ApiErrorCode::Internal,
                format!(
                    "snapshot config {} could not be written back: {error}",
                    config_path.display()
                ),
            )
        })?;
    }
    // Verified, never assumed: re-read the file and confirm every
    // mapping's device path is what the config now carries.
    let raw = fs::read_to_string(&config_path).map_err(|error| {
        ApiError::new(
            ApiErrorCode::Internal,
            format!(
                "post-rewrite verification could not re-read {}: {error}",
                config_path.display()
            ),
        )
    })?;
    let value: Value = serde_json::from_str(&raw).map_err(|error| {
        ApiError::new(
            ApiErrorCode::Internal,
            format!(
                "post-rewrite verification could not parse {}: {error}",
                config_path.display()
            ),
        )
    })?;
    verify_disk_mappings(&value, disks).map_err(|detail| {
        ApiError::new(
            ApiErrorCode::Internal,
            format!(
                "post-rewrite verification of {} failed: {detail}",
                config_path.display()
            ),
        )
    })
}

/// Apply the disk mappings to a parsed snapshot config, returning
/// whether anything changed.
///
/// Shared by the adapter (which persists the rewrite) and
/// [`FakeVmm`]'s restore (which opens the mapped devices) so both
/// spell one semantics: every `disks` entry with a `path` equal to a
/// mapping's `declared_path` is rewritten to that mapping's
/// `device_path`; a config disk no mapping declares is passed through
/// untouched (VM-wide eligibility is the coordinator's rule-6
/// decision, not the adapter's); a mapping that matches no config
/// disk — by declared path *or* device path (an already-rewritten
/// re-drive is idempotent) — is a typed refusal.
fn apply_disk_mappings(value: &mut Value, disks: &[DiskMapping]) -> Result<bool, ApiError> {
    let structure = |detail: String| {
        ApiError::new(
            ApiErrorCode::InvalidState,
            format!("unexpected snapshot config structure: {detail}"),
        )
    };
    let Some(object) = value.as_object_mut() else {
        return Err(structure("the top level is not a JSON object".to_owned()));
    };
    let Some(disks_value) = object.get_mut("disks") else {
        return Err(structure("no \"disks\" array".to_owned()));
    };
    let Some(entries) = disks_value.as_array_mut() else {
        return Err(structure("\"disks\" is not an array".to_owned()));
    };
    let mut changed = false;
    for entry in entries.iter_mut() {
        let Some(disk) = entry.as_object_mut() else {
            return Err(structure("a \"disks\" entry is not an object".to_owned()));
        };
        let current = match disk.get("path") {
            Some(Value::String(path)) => path.clone(),
            _ => {
                return Err(structure(
                    "a \"disks\" entry carries no string \"path\"".to_owned(),
                ));
            }
        };
        for mapping in disks {
            if current == mapping.declared_path && current != mapping.device_path {
                disk.insert(
                    "path".to_owned(),
                    Value::String(mapping.device_path.clone()),
                );
                changed = true;
            }
        }
    }
    for mapping in disks {
        let matched = entries.iter().any(|entry| {
            entry
                .get("path")
                .and_then(Value::as_str)
                .is_some_and(|path| path == mapping.declared_path || path == mapping.device_path)
        });
        if !matched {
            return Err(ApiError::new(
                ApiErrorCode::InvalidState,
                format!(
                    "disk mapping for declared path {} matches no disk in the snapshot config",
                    mapping.declared_path
                ),
            ));
        }
    }
    Ok(changed)
}

/// Confirm every mapping's device path is present as a config disk
/// path (the post-rewrite verification — never assumed).
fn verify_disk_mappings(value: &Value, disks: &[DiskMapping]) -> Result<(), String> {
    let paths = config_disk_paths(value)?;
    for mapping in disks {
        if !paths.contains(&mapping.device_path) {
            return Err(format!(
                "no disk carries the mapped device path {}",
                mapping.device_path
            ));
        }
    }
    Ok(())
}

/// The `path` fields of a parsed snapshot config's `disks` array
/// (structure-checked).
fn config_disk_paths(value: &Value) -> Result<Vec<String>, String> {
    let entries = value
        .get("disks")
        .and_then(Value::as_array)
        .ok_or_else(|| "no \"disks\" array".to_owned())?;
    entries
        .iter()
        .map(|entry| {
            entry
                .get("path")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| "a \"disks\" entry carries no string \"path\"".to_owned())
        })
        .collect()
}

/// Local unix time in seconds (the proof timestamps).
fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

// ------------------------------------------------------------------
// FakeVmm — the TEST-ONLY harness
// ------------------------------------------------------------------

/// The device-open hook of [`FakeVmm`]: called with the VM id, the
/// device paths the VM now holds (or held), and whether they were
/// opened (`true`) or released (`false`).
///
/// The paths are part of the signature by design: the fake-DRBD
/// integration it is wired to (`FakeDrbd.open_devices`, a later
/// slice's tests) is keyed by minor, which derives from the device
/// path — a hook that only saw the VM id could not perform the
/// insertion. Opens are level-triggered and idempotent (the wiring is
/// a set insertion); `false` fires exactly once, at destroy.
pub type DeviceHook = Arc<dyn Fn(&str, &[String], bool) + Send + Sync>;

/// TEST-ONLY per-VM failure injection for [`FakeVmm`] (crash-window
/// tests): one knob per [`VmmController`] operation; a set knob makes
/// that operation fail typed (`INTERNAL`, "injected failure") before
/// any state change or device event.
// One bool per operation is the point: crash-injection tests name the
// exact step they fault.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FakeFailKnobs {
    /// `pause` fails.
    pub pause: bool,
    /// `snapshot` fails.
    pub snapshot: bool,
    /// `destroy` fails.
    pub destroy: bool,
    /// `restore` fails.
    pub restore: bool,
    /// `resume` fails.
    pub resume: bool,
    /// `state` fails.
    pub state: bool,
    /// `resize_disk` fails.
    pub resize_disk: bool,
}

/// One recorded successful `resize_disk` call against [`FakeVmm`]
/// (harness introspection): what the VMM was told, exactly. The
/// engine's tests assert the recorded `(vm_id, disk_id, size)`
/// triples against the attachment-recorded disk id and the grow's
/// effective size — the disk id is opaque to the fake (the fake's
/// VMs model device *paths*, the migration world's key), so the
/// assertion lives on the caller's side, where the expected id is
/// known.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FakeResizeCall {
    /// The VM whose VMM was told.
    pub vm_id: String,
    /// The VMM device id the resize addressed.
    pub disk_id: String,
    /// The size the VMM was told.
    pub new_size_bytes: u64,
}

/// The present (non-`Absent`) states of the fake's model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PresentState {
    /// Defined, never booted.
    Created,
    /// Booted and running.
    Running,
    /// Booted and paused.
    Paused,
}

impl From<PresentState> for VmState {
    fn from(state: PresentState) -> Self {
        match state {
            PresentState::Created => Self::Created,
            PresentState::Running => Self::Running,
            PresentState::Paused => Self::Paused,
        }
    }
}

/// One modeled VM.
#[derive(Clone, Debug)]
struct FakeVm {
    state: PresentState,
    /// The device paths the VM holds open.
    devices: Vec<String>,
}

/// The in-memory world behind [`FakeVmm`].
#[derive(Default)]
struct FakeVmmWorld {
    vms: BTreeMap<String, FakeVm>,
    fail: BTreeMap<String, FakeFailKnobs>,
    /// The ordered controller-call log (see [`FakeVmm::calls`]).
    calls: Vec<(&'static str, String)>,
    /// The successful `resize_disk` calls, in order (see
    /// [`FakeVmm::resize_calls`]).
    resizes: Vec<FakeResizeCall>,
}

/// TEST-ONLY fake VMM (plan §5): an in-memory state machine over real
/// snapshot files, exported for downstream test crates — **never**
/// production.
///
/// It models exactly what the cutover depends on:
///
/// - the [`VmState`] machine with the verified command preconditions
///   (pause only from `Running` — `/vm.pause` requires booted;
///   snapshot only while `Paused`; restore only into an `Absent` VM,
///   landing `Paused`; resume only from `Paused`; destroy from any
///   present state, releasing the devices);
/// - snapshots as **real files** in a directory (the same artifacts
///   the verified surface names: `config.json` with the disks array,
///   `memory-ranges`, `state.json`) — a restore genuinely reads what
///   the snapshot wrote, and an unreadable directory genuinely fails
///   typed;
/// - device-open semantics through a [`DeviceHook`] the test wires to
///   the fake DRBD world: a present VM holds its declared devices
///   open (the hook fires `open=true` when a VM becomes present —
///   create, start, restore landing — and `open=false` at destroy).
///   This makes AGENTS rule 17 *provable in tests*: any coordinator
///   bug that demotes before `destroy` fails against the fake's busy
///   device, exactly as it would on a real host;
/// - per-VM, per-operation failure injection ([`FakeFailKnobs`]) for
///   crash-window tests;
/// - `resize_disk` as a recorded, fault-injectable tell (see
///   [`FakeResizeCall`]): the fake does not model the disk's size —
///   it records exactly what it was told, and the engine's tests
///   assert the recorded triples against the provider's truth (the
///   never-shrink rule is asserted against the backing, not the
///   VMM).
///
/// The re-drive-safety rules match the adapter exactly: destroying an
/// `Absent` VM is a no-op that succeeds without a device event, and
/// `restore` refuses a non-empty VMM typed.
pub struct FakeVmm {
    world: Arc<Mutex<FakeVmmWorld>>,
    snapshot_root: PathBuf,
    device_hook: DeviceHook,
    time: AtomicU64,
}

impl FakeVmm {
    /// Build a fake VMM whose snapshot artifacts live under
    /// `snapshot_root` (a tempdir in tests; per-migration directories
    /// are derived under it by the harness, while the controller
    /// methods honor the `dir` argument verbatim — the same contract
    /// as the adapter).
    #[must_use]
    pub fn new(snapshot_root: impl Into<PathBuf>) -> Self {
        let noop: DeviceHook = Arc::new(|_vm_id: &str, _devices: &[String], _open: bool| {});
        Self {
            world: Arc::new(Mutex::new(FakeVmmWorld::default())),
            snapshot_root: snapshot_root.into(),
            device_hook: noop,
            time: AtomicU64::new(now_unix()),
        }
    }

    /// Attach a device-open hook (see [`DeviceHook`]).
    #[must_use]
    pub fn with_device_hook(mut self, hook: DeviceHook) -> Self {
        self.device_hook = hook;
        self
    }

    /// Pin the clock the pause proofs carry (deterministic tests).
    pub fn set_time(&self, unix_seconds: u64) {
        self.time.store(unix_seconds, Ordering::SeqCst);
    }

    /// The configured snapshot-artifact root.
    #[must_use]
    pub fn snapshot_root(&self) -> &Path {
        &self.snapshot_root
    }

    /// Append one entry to the ordered call log (see
    /// [`Self::calls`]). Called at the entry of every
    /// [`VmmController`] method — before preconditions and before the
    /// injected-failure knobs — so the log proves a call HAPPENED even
    /// when it failed (crash-window tests fault an operation and then
    /// assert the retry re-enters it).
    fn note(&self, method: &'static str, vm_id: &str) -> Result<(), ApiError> {
        let mut world = lock(&self.world)?;
        world.calls.push((method, vm_id.to_owned()));
        Ok(())
    }

    /// The ordered log of [`VmmController`] calls against this fake:
    /// `(method, vm_id)` pairs in call order (e.g. `pause`, `snapshot`,
    /// `destroy`, `restore`, `resume`, `state`, `resize_disk`),
    /// including calls that failed. The harness-side `create`/`start`
    /// seeding acts are NOT logged — the log records what the VMM
    /// controller did, not what the consumer set up.
    pub fn calls(&self) -> Result<Vec<(&'static str, String)>, ApiError> {
        let world = lock(&self.world)?;
        Ok(world.calls.clone())
    }

    /// The successful `resize_disk` calls against this fake, in
    /// order (the what-the-VMM-was-told record; see
    /// [`FakeResizeCall`]). Failed attempts appear only in
    /// [`Self::calls`] — a faulted resize told the VMM nothing.
    pub fn resize_calls(&self) -> Result<Vec<FakeResizeCall>, ApiError> {
        let world = lock(&self.world)?;
        Ok(world.resizes.clone())
    }

    /// Edit one VM's failure-injection knobs (created on first use;
    /// the knobs apply whether or not the VM is present).
    pub fn set_fail(
        &self,
        vm_id: &str,
        edit: impl FnOnce(&mut FakeFailKnobs),
    ) -> Result<(), ApiError> {
        let mut world = lock(&self.world)?;
        let knobs = world.fail.entry(vm_id.to_owned()).or_default();
        edit(knobs);
        Ok(())
    }

    /// The VM's current state (harness introspection).
    pub fn vm_state(&self, vm_id: &str) -> Result<VmState, ApiError> {
        let world = lock(&self.world)?;
        Ok(world
            .vms
            .get(vm_id)
            .map_or(VmState::Absent, |vm| vm.state.into()))
    }

    /// The device paths one VM currently holds open (harness
    /// introspection; empty for an absent VM).
    pub fn vm_devices(&self, vm_id: &str) -> Result<Vec<String>, ApiError> {
        let world = lock(&self.world)?;
        Ok(world
            .vms
            .get(vm_id)
            .map_or_else(Vec::new, |vm| vm.devices.clone()))
    }

    /// Seed a defined (`Created`) VM holding `disks` — the consumer's
    /// act, not the coordinator's (volvisor never launches VMMs, plan
    /// §1). The device hook fires `open=true`: a present VM holds its
    /// devices for its whole lifetime (`/vm.delete` is the only
    /// verified release in the surface — so the fake conservatively
    /// models *any* present state, `Created` included, as
    /// device-holding; a false "closed" could never fail a demote,
    /// a false "open" only fails one that should fail).
    pub fn create(&self, vm_id: &str, disks: &[&str]) -> Result<(), ApiError> {
        let devices: Vec<String> = disks.iter().map(|disk| (*disk).to_owned()).collect();
        {
            let mut world = lock(&self.world)?;
            if world.vms.contains_key(vm_id) {
                return Err(ApiError::new(
                    ApiErrorCode::InvalidState,
                    format!("fake VMM: VM {vm_id} already exists"),
                ));
            }
            world.vms.insert(
                vm_id.to_owned(),
                FakeVm {
                    state: PresentState::Created,
                    devices: devices.clone(),
                },
            );
        }
        (self.device_hook)(vm_id, &devices, true);
        Ok(())
    }

    /// Start a defined VM (`Created` → `Running`) — the consumer's
    /// act. The device hook fires `open=true` again (level-triggered,
    /// idempotent for the set-based wiring).
    pub fn start(&self, vm_id: &str) -> Result<(), ApiError> {
        let devices = {
            let mut world = lock(&self.world)?;
            let Some(vm) = world.vms.get_mut(vm_id) else {
                return Err(ApiError::new(
                    ApiErrorCode::InvalidState,
                    format!("fake VMM: no VM {vm_id} to start"),
                ));
            };
            if vm.state != PresentState::Created {
                return Err(ApiError::new(
                    ApiErrorCode::InvalidState,
                    format!(
                        "fake VMM: start of VM {vm_id} requires a created VM (current state {})",
                        VmState::from(vm.state)
                    ),
                ));
            }
            vm.state = PresentState::Running;
            vm.devices.clone()
        };
        (self.device_hook)(vm_id, &devices, true);
        Ok(())
    }

    /// The typed injected-failure error for one operation.
    fn injected(operation: &str, vm_id: &str) -> ApiError {
        ApiError::new(
            ApiErrorCode::Internal,
            format!("fake VMM injected failure: {operation} of VM {vm_id}"),
        )
    }
}

impl VmmController for FakeVmm {
    fn pause(&self, vm_id: &str) -> Result<PauseProof, ApiError> {
        self.note("pause", vm_id)?;
        let mut world = lock(&self.world)?;
        if world.fail.get(vm_id).is_some_and(|knobs| knobs.pause) {
            return Err(Self::injected("pause", vm_id));
        }
        let Some(vm) = world.vms.get_mut(vm_id) else {
            return Err(ApiError::not_found(format!("fake VMM: no VM {vm_id}")));
        };
        if vm.state != PresentState::Running {
            return Err(ApiError::new(
                ApiErrorCode::InvalidState,
                format!(
                    "fake VMM: pause of VM {vm_id} requires a running VM \
                     (/vm.pause requires the VM booted; current state {})",
                    VmState::from(vm.state)
                ),
            ));
        }
        vm.state = PresentState::Paused;
        let observed_at = self.time.load(Ordering::SeqCst);
        Ok(PauseProof {
            vm_id: vm_id.to_owned(),
            state: VmState::Paused,
            observed_at,
        })
    }

    fn snapshot(&self, vm_id: &str, dir: &Path) -> Result<(), ApiError> {
        self.note("snapshot", vm_id)?;
        let devices = {
            let world = lock(&self.world)?;
            if world.fail.get(vm_id).is_some_and(|knobs| knobs.snapshot) {
                return Err(Self::injected("snapshot", vm_id));
            }
            let Some(vm) = world.vms.get(vm_id) else {
                return Err(ApiError::not_found(format!("fake VMM: no VM {vm_id}")));
            };
            if vm.state != PresentState::Paused {
                return Err(ApiError::new(
                    ApiErrorCode::InvalidState,
                    format!(
                        "fake VMM: snapshot of VM {vm_id} requires a paused VM \
                         (/vm.snapshot requires the VM paused; current state {})",
                        VmState::from(vm.state)
                    ),
                ));
            }
            vm.devices.clone()
        };
        // Real files: a restore genuinely reads what this writes. The
        // artifacts are the ones the verified surface names.
        let config = json!({
            "disks": devices
                .iter()
                .map(|device| json!({ "path": device }))
                .collect::<Vec<_>>(),
        });
        fs::create_dir_all(dir).map_err(|error| {
            ApiError::new(
                ApiErrorCode::Internal,
                format!(
                    "fake VMM: snapshot directory {} could not be created: {error}",
                    dir.display()
                ),
            )
        })?;
        let write = |name: &str, bytes: &[u8]| {
            fs::write(dir.join(name), bytes).map_err(|error| {
                ApiError::new(
                    ApiErrorCode::Internal,
                    format!(
                        "fake VMM: snapshot artifact {}/{} could not be written: {error}",
                        dir.display(),
                        name
                    ),
                )
            })
        };
        let config = serde_json::to_string_pretty(&config).map_err(|error| {
            ApiError::new(
                ApiErrorCode::Internal,
                format!("fake VMM: snapshot config could not be serialized: {error}"),
            )
        })?;
        write("config.json", config.as_bytes())?;
        write("memory-ranges", b"fake-memory-ranges\n")?;
        write("state.json", b"{\"vm_state\":\"Paused\"}\n")?;
        Ok(())
    }

    fn destroy(&self, vm_id: &str) -> Result<(), ApiError> {
        self.note("destroy", vm_id)?;
        let devices = {
            let mut world = lock(&self.world)?;
            if world.fail.get(vm_id).is_some_and(|knobs| knobs.destroy) {
                return Err(Self::injected("destroy", vm_id));
            }
            let Some(vm) = world.vms.remove(vm_id) else {
                // Re-drive safety, identical to the adapter: an absent
                // VM is a no-op — and the device hook does NOT fire.
                return Ok(());
            };
            vm.devices
        };
        (self.device_hook)(vm_id, &devices, false);
        Ok(())
    }

    fn restore(&self, vm_id: &str, dir: &Path, disks: &[DiskMapping]) -> Result<(), ApiError> {
        self.note("restore", vm_id)?;
        {
            let world = lock(&self.world)?;
            if world.fail.get(vm_id).is_some_and(|knobs| knobs.restore) {
                return Err(Self::injected("restore", vm_id));
            }
            if world.vms.contains_key(vm_id) {
                return Err(ApiError::new(
                    ApiErrorCode::InvalidState,
                    format!(
                        "fake VMM: restore of VM {vm_id} refuses a non-empty VMM \
                         (the coordinator destroys the half-restored VM first, plan §3)"
                    ),
                ));
            }
        }
        // Genuinely read the real files the snapshot wrote: an
        // unreadable or malformed snapshot fails typed, exactly as a
        // real restore would refuse.
        let config_path = dir.join("config.json");
        let raw = fs::read_to_string(&config_path).map_err(|error| {
            ApiError::not_found(format!(
                "fake VMM: snapshot config {} unreadable: {error}",
                config_path.display()
            ))
        })?;
        let mut value: Value = serde_json::from_str(&raw).map_err(|error| {
            ApiError::new(
                ApiErrorCode::InvalidState,
                format!(
                    "fake VMM: snapshot config {} is not valid JSON: {error}",
                    config_path.display()
                ),
            )
        })?;
        apply_disk_mappings(&mut value, disks)?;
        for name in ["memory-ranges", "state.json"] {
            fs::read(dir.join(name)).map_err(|error| {
                ApiError::not_found(format!(
                    "fake VMM: snapshot artifact {}/{} unreadable: {error}",
                    dir.display(),
                    name
                ))
            })?;
        }
        let devices = config_disk_paths(&value).map_err(|detail| {
            ApiError::new(
                ApiErrorCode::InvalidState,
                format!(
                    "fake VMM: unexpected snapshot config structure in {}: {detail}",
                    config_path.display()
                ),
            )
        })?;
        {
            let mut world = lock(&self.world)?;
            if world.vms.contains_key(vm_id) {
                return Err(ApiError::new(
                    ApiErrorCode::InvalidState,
                    format!(
                        "fake VMM: restore of VM {vm_id} refuses a non-empty VMM \
                         (the coordinator destroys the half-restored VM first, plan §3)"
                    ),
                ));
            }
            // The verified surface: a restored VM lands paused.
            world.vms.insert(
                vm_id.to_owned(),
                FakeVm {
                    state: PresentState::Paused,
                    devices: devices.clone(),
                },
            );
        }
        (self.device_hook)(vm_id, &devices, true);
        Ok(())
    }

    fn resume(&self, vm_id: &str) -> Result<(), ApiError> {
        self.note("resume", vm_id)?;
        let mut world = lock(&self.world)?;
        if world.fail.get(vm_id).is_some_and(|knobs| knobs.resume) {
            return Err(Self::injected("resume", vm_id));
        }
        let Some(vm) = world.vms.get_mut(vm_id) else {
            return Err(ApiError::not_found(format!("fake VMM: no VM {vm_id}")));
        };
        if vm.state != PresentState::Paused {
            return Err(ApiError::new(
                ApiErrorCode::InvalidState,
                format!(
                    "fake VMM: resume of VM {vm_id} requires a paused VM \
                     (/vm.resume requires the VM paused; current state {})",
                    VmState::from(vm.state)
                ),
            ));
        }
        vm.state = PresentState::Running;
        Ok(())
    }

    fn state(&self, vm_id: &str) -> Result<VmState, ApiError> {
        self.note("state", vm_id)?;
        let world = lock(&self.world)?;
        if world.fail.get(vm_id).is_some_and(|knobs| knobs.state) {
            return Err(Self::injected("state", vm_id));
        }
        Ok(world
            .vms
            .get(vm_id)
            .map_or(VmState::Absent, |vm| vm.state.into()))
    }

    fn resize_disk(&self, vm_id: &str, disk_id: &str, new_size_bytes: u64) -> Result<(), ApiError> {
        self.note("resize_disk", vm_id)?;
        let mut world = lock(&self.world)?;
        if world.fail.get(vm_id).is_some_and(|knobs| knobs.resize_disk) {
            return Err(Self::injected("resize_disk", vm_id));
        }
        if !world.vms.contains_key(vm_id) {
            return Err(ApiError::not_found(format!("fake VMM: no VM {vm_id}")));
        }
        // The fake does not model the disk's size: the tell is the
        // record. Never-shrink is the caller's rule, asserted by the
        // engine's tests against the backing.
        world.resizes.push(FakeResizeCall {
            vm_id: vm_id.to_owned(),
            disk_id: disk_id.to_owned(),
            new_size_bytes,
        });
        Ok(())
    }
}

/// Lock a mutex, mapping poisoning to an `INTERNAL` error (never a
/// panic — the same discipline as the runner's).
fn lock<T>(mutex: &Mutex<T>) -> Result<MutexGuard<'_, T>, ApiError> {
    mutex.lock().map_err(|_| {
        ApiError::new(
            ApiErrorCode::Internal,
            "fake VMM world lock poisoned by a previous failure",
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- Types -----------------------------------------------------

    #[test]
    fn vm_state_serializes_snake_case_and_round_trips() {
        for (state, spelling) in [
            (VmState::Absent, "\"absent\""),
            (VmState::Created, "\"created\""),
            (VmState::Running, "\"running\""),
            (VmState::Paused, "\"paused\""),
        ] {
            let json = serde_json::to_string(&state).expect("serialize");
            assert_eq!(json, spelling);
            assert_eq!(
                serde_json::from_str::<VmState>(&json).expect("deserialize"),
                state
            );
        }
    }

    #[test]
    fn pause_proof_and_disk_mapping_deny_unknown_fields() {
        let proof = PauseProof {
            vm_id: "vm-1".to_owned(),
            state: VmState::Paused,
            observed_at: 1_700_000_000,
        };
        let json = serde_json::to_string(&proof).expect("serialize");
        assert_eq!(
            serde_json::from_str::<PauseProof>(&json).expect("deserialize"),
            proof
        );
        let extended = json.replace("\"vm_id\"", "\"extra\":1,\"vm_id\"");
        assert!(serde_json::from_str::<PauseProof>(&extended).is_err());

        let mapping = DiskMapping {
            declared_path: "/dev/drbd1".to_owned(),
            device_path: "/dev/drbd-by-res/vol-1".to_owned(),
        };
        let json = serde_json::to_string(&mapping).expect("serialize");
        assert_eq!(
            serde_json::from_str::<DiskMapping>(&json).expect("deserialize"),
            mapping
        );
        let extended = json.replace("\"declared_path\"", "\"extra\":1,\"declared_path\"");
        assert!(serde_json::from_str::<DiskMapping>(&extended).is_err());
    }

    // -- Disk-path rewriting (shared by adapter and fake) -----------

    fn mapping(declared: &str, device: &str) -> DiskMapping {
        DiskMapping {
            declared_path: declared.to_owned(),
            device_path: device.to_owned(),
        }
    }

    #[test]
    fn disk_mapping_rewrite_is_a_no_op_when_paths_match() {
        let mut config = json!({
            "cpus": 4,
            "disks": [{"path": "/dev/drbd1"}, {"path": "/dev/drbd2"}]
        });
        let before = config.clone();
        let changed = apply_disk_mappings(&mut config, &[mapping("/dev/drbd1", "/dev/drbd1")])
            .expect("apply");
        assert!(!changed, "a matching mapping must not rewrite anything");
        assert_eq!(config, before, "the config must be untouched");
        verify_disk_mappings(&config, &[mapping("/dev/drbd1", "/dev/drbd1")])
            .expect("the no-op still verifies");
    }

    #[test]
    fn disk_mapping_rewrite_rewrites_only_the_declared_paths() {
        let mut config = json!({
            "cpus": 4,
            "memory": {"size": 1024},
            "disks": [{"path": "/dev/drbd1"}, {"path": "/dev/drbd2"}]
        });
        let changed = apply_disk_mappings(
            &mut config,
            &[mapping("/dev/drbd1", "/dev/drbd-by-res/vol-1")],
        )
        .expect("apply");
        assert!(changed);
        assert_eq!(
            config["disks"][0]["path"],
            json!("/dev/drbd-by-res/vol-1"),
            "the declared path is rewritten"
        );
        assert_eq!(
            config["disks"][1]["path"],
            json!("/dev/drbd2"),
            "the unmapped disk is passed through untouched"
        );
        assert_eq!(config["cpus"], json!(4), "unrelated fields survive");
        verify_disk_mappings(&config, &[mapping("/dev/drbd1", "/dev/drbd-by-res/vol-1")])
            .expect("the rewrite verifies");
    }

    #[test]
    fn disk_mapping_rewrite_is_idempotent_for_already_rewritten_configs() {
        // The crash re-drive: a previous restore attempt already
        // rewrote the config; applying the same mapping again must
        // neither fail (the declared path is gone) nor change it.
        let mut config = json!({"disks": [{"path": "/dev/drbd-by-res/vol-1"}]});
        let changed = apply_disk_mappings(
            &mut config,
            &[mapping("/dev/drbd1", "/dev/drbd-by-res/vol-1")],
        )
        .expect("an already-rewritten config is a satisfied mapping");
        assert!(!changed);
        verify_disk_mappings(&config, &[mapping("/dev/drbd1", "/dev/drbd-by-res/vol-1")])
            .expect("the re-drive still verifies");
    }

    #[test]
    fn disk_mapping_rewrite_refuses_a_mapping_without_a_config_disk() {
        let mut config = json!({"disks": [{"path": "/dev/drbd1"}]});
        let err = apply_disk_mappings(&mut config, &[mapping("/dev/other", "/dev/x")])
            .expect_err("a mapping with no config disk is never a guess");
        assert_eq!(err.code, ApiErrorCode::InvalidState);
        assert!(err.detail.contains("/dev/other"), "{err}");
    }

    #[test]
    fn disk_mapping_rewrite_refuses_unexpected_structures() {
        let cases: Vec<Value> = vec![
            json!([]),                        // top level not an object
            json!({"cpus": 4}),               // no disks array
            json!({"disks": {"path": "x"}}),  // disks not an array
            json!({"disks": ["raw-string"]}), // entry not an object
            json!({"disks": [{"id": 1}]}),    // entry without a path
            json!({"disks": [{"path": 7}]}),  // path not a string
        ];
        for config in cases {
            let mut config = config;
            let err = apply_disk_mappings(&mut config, &[mapping("/dev/drbd1", "/dev/drbd2")])
                .expect_err("an unexpected structure is a typed refusal");
            assert_eq!(err.code, ApiErrorCode::InvalidState, "{err}");
            assert!(
                err.detail.contains("unexpected snapshot config structure"),
                "{err}"
            );
        }
    }

    // -- FakeVmm: the state machine ---------------------------------

    /// Recorded device-hook events (vm id, device paths, open).
    type DeviceEvents = Arc<Mutex<Vec<(String, Vec<String>, bool)>>>;

    #[test]
    fn fake_vmm_models_the_legal_state_machine() {
        let fake = FakeVmm::new("/tmp/unused-fake-vmm-state-machine");
        assert_eq!(fake.state("vm-1").expect("state"), VmState::Absent);

        // Harness seeding: create lands Created, start lands Running.
        fake.create("vm-1", &["/dev/drbd1"]).expect("create");
        assert_eq!(fake.state("vm-1").expect("state"), VmState::Created);
        fake.start("vm-1").expect("start");
        assert_eq!(fake.state("vm-1").expect("state"), VmState::Running);

        // pause only from Running; resume only from Paused.
        let proof = fake.pause("vm-1").expect("pause");
        assert_eq!(proof.state, VmState::Paused);
        assert_eq!(fake.state("vm-1").expect("state"), VmState::Paused);
        let err = fake
            .pause("vm-1")
            .expect_err("a paused VM cannot pause again");
        assert_eq!(err.code, ApiErrorCode::InvalidState);
        fake.resume("vm-1").expect("resume");
        assert_eq!(fake.state("vm-1").expect("state"), VmState::Running);
        let err = fake.resume("vm-1").expect_err("a running VM cannot resume");
        assert_eq!(err.code, ApiErrorCode::InvalidState);

        // snapshot only while Paused.
        let dir = fake.snapshot_root().join("state-machine-snapshot");
        let err = fake
            .snapshot("vm-1", &dir)
            .expect_err("snapshot requires paused");
        assert_eq!(err.code, ApiErrorCode::InvalidState);
        fake.pause("vm-1").expect("pause again");
        fake.snapshot("vm-1", &dir).expect("snapshot while paused");

        // destroy from any present state; ops on an absent VM are
        // typed (absence is not an error for state itself).
        fake.destroy("vm-1").expect("destroy from paused");
        assert_eq!(fake.state("vm-1").expect("state"), VmState::Absent);
        let err = fake
            .pause("vm-1")
            .expect_err("pause of an absent VM must fail");
        assert_eq!(err.code, ApiErrorCode::NotFound, "pause: {err}");
        let err = fake
            .snapshot("vm-1", &dir)
            .expect_err("snapshot of an absent VM must fail");
        assert_eq!(err.code, ApiErrorCode::NotFound, "snapshot: {err}");
        let err = fake
            .resume("vm-1")
            .expect_err("resume of an absent VM must fail");
        assert_eq!(err.code, ApiErrorCode::NotFound, "resume: {err}");

        // destroy from Created and from Running too (delete has no
        // prerequisites).
        fake.create("vm-2", &["/dev/drbd2"]).expect("create");
        fake.destroy("vm-2").expect("destroy from created");
        fake.create("vm-2", &["/dev/drbd2"]).expect("create again");
        fake.start("vm-2").expect("start");
        fake.destroy("vm-2").expect("destroy from running");
    }

    #[test]
    fn fake_vmm_pause_proof_carries_the_pinned_clock() {
        let fake = FakeVmm::new("/tmp/unused-fake-vmm-clock");
        fake.create("vm-1", &["/dev/drbd1"]).expect("create");
        fake.start("vm-1").expect("start");
        fake.set_time(1_700_000_042);
        let proof = fake.pause("vm-1").expect("pause");
        assert_eq!(proof.vm_id, "vm-1");
        assert_eq!(proof.state, VmState::Paused);
        assert_eq!(proof.observed_at, 1_700_000_042);
    }

    #[test]
    fn fake_vmm_call_log_records_every_controller_call_in_order() {
        let root = tempfile::tempdir().expect("tempdir");
        let fake = FakeVmm::new(root.path());
        // The harness-side seeding acts (create/start) are NOT
        // controller calls — the log starts empty.
        assert_eq!(fake.calls().expect("calls"), Vec::<(&str, String)>::new());
        fake.create("vm-1", &["/dev/drbd1"]).expect("create");
        fake.start("vm-1").expect("start");
        assert_eq!(fake.calls().expect("calls"), Vec::<(&str, String)>::new());

        fake.pause("vm-1").expect("pause");
        let dir = root.path().join("mig-1");
        fake.snapshot("vm-1", &dir).expect("snapshot");

        // A faulted call is still logged: the log proves the call
        // HAPPENED — exactly what a crash-window retry asserts
        // against (the re-drive re-enters the operation).
        fake.set_fail("vm-1", |knobs| knobs.destroy = true)
            .expect("knobs");
        fake.destroy("vm-1").expect_err("injected destroy failure");
        fake.set_fail("vm-1", |knobs| knobs.destroy = false)
            .expect("knobs");
        fake.destroy("vm-1").expect("destroy");

        let calls = fake.calls().expect("calls");
        let methods: Vec<&str> = calls.iter().map(|(method, _)| *method).collect();
        assert_eq!(methods, vec!["pause", "snapshot", "destroy", "destroy"]);
        assert!(calls.iter().all(|(_, vm_id)| vm_id == "vm-1"), "{calls:?}");
    }

    #[test]
    fn fake_vmm_snapshot_restore_round_trips_through_real_files() {
        let root = tempfile::tempdir().expect("tempdir");
        let fake = FakeVmm::new(root.path());
        fake.create("vm-1", &["/dev/drbd1", "/dev/drbd2"])
            .expect("create");
        fake.start("vm-1").expect("start");
        fake.pause("vm-1").expect("pause");

        let dir = root.path().join("mig-1");
        fake.snapshot("vm-1", &dir).expect("snapshot");
        for name in ["config.json", "memory-ranges", "state.json"] {
            assert!(dir.join(name).is_file(), "snapshot wrote {name}");
        }
        let written = fs::read_to_string(dir.join("config.json")).expect("config");
        assert!(written.contains("/dev/drbd1"), "{written}");
        assert!(written.contains("/dev/drbd2"), "{written}");

        fake.destroy("vm-1").expect("destroy the source");
        // No mapping: the restore opens exactly the declared paths.
        fake.restore("vm-1", &dir, &[]).expect("restore");
        assert_eq!(fake.state("vm-1").expect("state"), VmState::Paused);
        assert_eq!(
            fake.vm_devices("vm-1").expect("devices"),
            vec!["/dev/drbd1".to_owned(), "/dev/drbd2".to_owned()]
        );
        fake.resume("vm-1").expect("resume the restored VM");
        assert_eq!(fake.state("vm-1").expect("state"), VmState::Running);
    }

    #[test]
    fn fake_vmm_restore_applies_disk_mappings_and_opens_mapped_devices() {
        let root = tempfile::tempdir().expect("tempdir");
        let fake = FakeVmm::new(root.path());
        fake.create("vm-1", &["/dev/drbd1"]).expect("create");
        fake.start("vm-1").expect("start");
        fake.pause("vm-1").expect("pause");
        let dir = root.path().join("mig-1");
        fake.snapshot("vm-1", &dir).expect("snapshot");
        fake.destroy("vm-1").expect("destroy");

        let mapping = DiskMapping {
            declared_path: "/dev/drbd1".to_owned(),
            device_path: "/dev/drbd-by-res/vol-1".to_owned(),
        };
        fake.restore("vm-1", &dir, &[mapping]).expect("restore");
        assert_eq!(fake.state("vm-1").expect("state"), VmState::Paused);
        // The restored VM holds the TARGET's promoted device open —
        // the fact the fake-DRBD wiring derives its minor from.
        assert_eq!(
            fake.vm_devices("vm-1").expect("devices"),
            vec!["/dev/drbd-by-res/vol-1".to_owned()]
        );
    }

    #[test]
    fn fake_vmm_restore_refuses_a_non_empty_vmm() {
        let root = tempfile::tempdir().expect("tempdir");
        let fake = FakeVmm::new(root.path());
        fake.create("vm-1", &["/dev/drbd1"]).expect("create");
        fake.start("vm-1").expect("start");
        fake.pause("vm-1").expect("pause");
        let dir = root.path().join("mig-1");
        fake.snapshot("vm-1", &dir).expect("snapshot");

        // The VM is still present (Paused): a typed refusal, never a
        // destroy-on-the-caller's-behalf.
        let err = fake
            .restore("vm-1", &dir, &[])
            .expect_err("restore refuses a non-empty VMM");
        assert_eq!(err.code, ApiErrorCode::InvalidState);
        assert_eq!(fake.state("vm-1").expect("state"), VmState::Paused);
    }

    #[test]
    fn fake_vmm_restore_fails_typed_on_a_missing_or_malformed_snapshot() {
        let root = tempfile::tempdir().expect("tempdir");
        let fake = FakeVmm::new(root.path());

        let missing = root.path().join("missing");
        let err = fake
            .restore("vm-1", &missing, &[])
            .expect_err("a missing snapshot dir fails typed");
        assert_eq!(err.code, ApiErrorCode::NotFound);

        let malformed = root.path().join("malformed");
        fs::create_dir_all(&malformed).expect("dir");
        fs::write(malformed.join("config.json"), "not json").expect("write");
        let err = fake
            .restore("vm-1", &malformed, &[])
            .expect_err("a malformed config fails typed");
        assert_eq!(err.code, ApiErrorCode::InvalidState);
        assert_eq!(fake.state("vm-1").expect("state"), VmState::Absent);
    }

    #[test]
    fn fake_vmm_destroy_releases_devices_through_the_hook() {
        let root = tempfile::tempdir().expect("tempdir");
        let events: DeviceEvents = Arc::default();
        let recorder = Arc::clone(&events);
        let fake = FakeVmm::new(root.path()).with_device_hook(Arc::new(
            move |vm_id: &str, devices: &[String], open: bool| {
                recorder.lock().expect("event lock").push((
                    vm_id.to_owned(),
                    devices.to_vec(),
                    open,
                ));
            },
        ));

        fake.create("vm-1", &["/dev/drbd1", "/dev/drbd2"])
            .expect("create");
        fake.start("vm-1").expect("start");
        fake.destroy("vm-1").expect("destroy");

        let expected_event = |open: bool| {
            (
                "vm-1".to_owned(),
                vec!["/dev/drbd1".to_owned(), "/dev/drbd2".to_owned()],
                open,
            )
        };
        assert_eq!(
            events.lock().expect("event lock").clone(),
            // create and start open (level-triggered, idempotent for
            // the set-based wiring), destroy closes.
            vec![
                expected_event(true),
                expected_event(true),
                expected_event(false)
            ]
        );
    }

    #[test]
    fn fake_vmm_destroy_of_an_absent_vm_is_a_no_op() {
        let root = tempfile::tempdir().expect("tempdir");
        let events: Arc<Mutex<usize>> = Arc::default();
        let recorder = Arc::clone(&events);
        let fake = FakeVmm::new(root.path()).with_device_hook(Arc::new(
            move |_vm_id: &str, _devices: &[String], _open: bool| {
                *recorder.lock().expect("event lock") += 1;
            },
        ));

        // The crash-reconcile dependency: re-driving destroy of an
        // already-absent VM succeeds and fires no device event.
        fake.destroy("vm-1").expect("destroy of an absent VM");
        fake.destroy("vm-1").expect("destroy again");
        assert_eq!(*events.lock().expect("event lock"), 0);
        assert_eq!(fake.state("vm-1").expect("state"), VmState::Absent);
    }

    #[test]
    fn fake_vmm_fail_knobs_inject_typed_failures_per_operation() {
        let root = tempfile::tempdir().expect("tempdir");
        let fake = FakeVmm::new(root.path());
        fake.create("vm-1", &["/dev/drbd1"]).expect("create");
        fake.start("vm-1").expect("start");

        fake.set_fail("vm-1", |knobs| knobs.pause = true)
            .expect("set fail");
        let err = fake.pause("vm-1").expect_err("injected pause failure");
        assert_eq!(err.code, ApiErrorCode::Internal);
        assert!(err.detail.contains("injected"), "{err}");
        // The injection changed nothing: still Running.
        assert_eq!(fake.state("vm-1").expect("state"), VmState::Running);

        fake.set_fail("vm-1", |knobs| {
            knobs.pause = false;
            knobs.state = true;
        })
        .expect("set fail");
        let err = fake.state("vm-1").expect_err("injected state failure");
        assert_eq!(err.code, ApiErrorCode::Internal);
        // The other VM is unaffected.
        assert_eq!(fake.state("vm-2").expect("state"), VmState::Absent);
    }

    // -- FakeVmm: resize_disk ---------------------------------------

    #[test]
    fn fake_vmm_resize_disk_records_the_tell_and_needs_a_present_vm() {
        let root = tempfile::tempdir().expect("tempdir");
        let fake = FakeVmm::new(root.path());
        fake.create("vm-1", &["/dev/drbd1"]).expect("create");

        // An absent VM is a typed NOT_FOUND, and nothing is recorded.
        let err = fake
            .resize_disk("vm-x", "vol-1", 2048)
            .expect_err("resize of an absent VM must fail");
        assert_eq!(err.code, ApiErrorCode::NotFound, "{err}");
        assert_eq!(
            fake.resize_calls().expect("resizes"),
            Vec::<FakeResizeCall>::new()
        );

        fake.resize_disk("vm-1", "vol-1", 2_147_483_648)
            .expect("resize");
        fake.resize_disk("vm-1", "vol-2", 3_221_225_472)
            .expect("resize");
        assert_eq!(
            fake.resize_calls().expect("resizes"),
            vec![
                FakeResizeCall {
                    vm_id: "vm-1".to_owned(),
                    disk_id: "vol-1".to_owned(),
                    new_size_bytes: 2_147_483_648,
                },
                FakeResizeCall {
                    vm_id: "vm-1".to_owned(),
                    disk_id: "vol-2".to_owned(),
                    new_size_bytes: 3_221_225_472,
                },
            ]
        );
        // The attempt is in the call log whether or not it succeeded.
        let calls = fake.calls().expect("calls");
        let methods: Vec<&str> = calls.iter().map(|(method, _)| *method).collect();
        assert_eq!(
            methods,
            vec!["resize_disk", "resize_disk", "resize_disk"],
            "the faulted absent-VM attempt is logged too"
        );
    }

    #[test]
    fn fake_vmm_resize_disk_fault_is_injected_and_records_nothing() {
        let root = tempfile::tempdir().expect("tempdir");
        let fake = FakeVmm::new(root.path());
        fake.create("vm-1", &["/dev/drbd1"]).expect("create");

        fake.set_fail("vm-1", |knobs| knobs.resize_disk = true)
            .expect("knobs");
        let err = fake
            .resize_disk("vm-1", "vol-1", 2048)
            .expect_err("injected resize failure");
        assert_eq!(err.code, ApiErrorCode::Internal);
        assert!(err.detail.contains("injected"), "{err}");
        // A faulted resize told the VMM nothing.
        assert_eq!(
            fake.resize_calls().expect("resizes"),
            Vec::<FakeResizeCall>::new()
        );
        // Recovery through the same surface production would.
        fake.set_fail("vm-1", |knobs| knobs.resize_disk = false)
            .expect("knobs");
        fake.resize_disk("vm-1", "vol-1", 2048)
            .expect("resize after recovery");
        assert_eq!(
            fake.resize_calls().expect("resizes"),
            vec![FakeResizeCall {
                vm_id: "vm-1".to_owned(),
                disk_id: "vol-1".to_owned(),
                new_size_bytes: 2048,
            }]
        );
    }
}
