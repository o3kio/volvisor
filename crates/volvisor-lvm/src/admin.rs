//! Administrative (operator-facing) operations on the LVM provider.
//!
//! The inherent methods on [`LvmProvider`] implement the privileged
//! enrollment concerns (discovery, device claiming, release,
//! reconciliation reporting); [`AdminSurface`] is implemented on top of
//! them so the API layer can treat the provider polymorphically. They are
//! deliberately not part of the [`VolumeProvider`](volvisor_provider::VolumeProvider)
//! trait: these are host-administration concerns rather than tenant-facing
//! volume operations.
//!
//! Claiming and releasing a device are privileged, destructive operations:
//! both require the scoped destructive-authorization token carried by the
//! request (compared against the token configured at construction, never
//! logged). A mismatch fails with `UNSUPPORTED_CLASS_OR_POLICY`.
//!
//! Crash windows are handled so state always matches observed reality:
//! a claim whose `vgcreate` fails undoes its `pvcreate` (best effort, with
//! a manual-remediation hint when the undo fails); a release whose
//! `vgremove` fails *but whose VG is verifiably absent from `vgs`*
//! reconciles forward (the claim is dropped and `pvremove` proceeds —
//! a VG removed behind the daemon's back must not pin the claim
//! forever); a release whose `pvremove` fails keeps the claim *removed*
//! (the VG is already gone) and reports the exact manual remediation
//! instead of silently diverging. Startup reconciliation drops device
//! claims whose VG is verifiably gone (never on a failed query).

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use volvisor_provider::AdminSurface;
use volvisor_types::domain::{DeviceRole, Health, Pool, PoolProtection, VolumeClass};
use volvisor_types::{
    ApiError, ApiErrorCode, ClaimDeviceRequest, DeviceId, DeviceListResponse, PoolId,
    ReleaseDeviceRequest,
};

use crate::discover;
use crate::provider::{LvmProvider, VG_HEADROOM_BYTES, command_failed};
use crate::state::DeviceEntry;

/// Result of comparing observed LVM state against the provider state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// State entries whose logical volume is absent from `lvs` output.
    pub missing_volumes: Vec<volvisor_types::VolumeId>,
    /// LVM LVs under our volume groups that the state does not know about.
    /// Reported, never adopted or destroyed (AGENTS rule 7).
    pub foreign_lvs: Vec<String>,
}

impl LvmProvider {
    /// Read-only device discovery (delegates to [`discover`]).
    ///
    /// Health is `Unknown` and ownership fields are empty: claim state
    /// lives in the provider state file, not in discovery output.
    pub fn discover(&self) -> Result<DeviceListResponse, ApiError> {
        Ok(DeviceListResponse {
            devices: discover::discover_devices(self.runner.as_ref(), &self.sysfs_root)?,
        })
    }

    /// Claim a discovered device as a native-local pool.
    ///
    /// Requires the destructive-authorization token carried by `request`.
    /// The device must be discoverable with a stable identity, must not
    /// already be claimed, and `pvs` must show **no existing physical
    /// volume** on its path — foreign LVM state is never adopted
    /// (`FOREIGN_DEVICE_STATE`). On success the device holds a fresh
    /// `pvcreate` + `vgcreate` volume group named
    /// `<vg_prefix>-<8 hex of sha256(device_id)>`, the claim is persisted,
    /// and the resulting [`Pool`] is returned.
    ///
    /// If `vgcreate` fails after `pvcreate` succeeded, the `pvcreate` is
    /// undone with a best-effort `pvremove` (and a failing undo surfaces a
    /// manual-remediation hint): state never claims a device whose volume
    /// group was not created.
    pub fn claim_device(
        &self,
        device_id: &DeviceId,
        request: &ClaimDeviceRequest,
    ) -> Result<Pool, ApiError> {
        request.validate()?;
        self.require_token(&request.authorization_token)?;
        let mut state = self.lock_state()?;

        if state.device(device_id).is_some() {
            return Err(ApiError::new(
                ApiErrorCode::InvalidState,
                format!("device {device_id} is already claimed"),
            ));
        }
        // Resolve the opaque identity back to a path via read-only
        // discovery; an identity that cannot be resolved is not claimable.
        let scanned = discover::scan(self.runner.as_ref(), &self.sysfs_root)?;
        let device = scanned
            .iter()
            .find(|device| device.device_id == *device_id)
            .ok_or_else(|| {
                ApiError::not_found(format!(
                    "device {device_id} was not discovered on this host"
                ))
            })?;

        // Foreign PV state: never adopt, never wipe (AGENTS rule 7).
        let kernel_path = format!("/dev/{}", device.kernel_name);
        for pv in self.list_pvs()? {
            if pv.pv_name.as_deref() == Some(device.path.as_str())
                || pv.pv_name.as_deref() == Some(kernel_path.as_str())
            {
                // The remediation names both steps in order: an
                // interrupted claim can leave a full VG behind (crash
                // after vgcreate), and pvremove fails while the VG
                // exists.
                return Err(ApiError::new(
                    ApiErrorCode::ForeignDeviceState,
                    format!(
                        "device path {} already hosts a physical volume; foreign state is \
                         never adopted (if this is a leftover from an interrupted volvisor \
                         claim, remove it manually with vgremove {} then pvremove {})",
                        device.path,
                        self.claim_vg_name(device_id),
                        device.path
                    ),
                ));
            }
        }

        let vg_name = self.claim_vg_name(device_id);
        let output = self.runner.run("pvcreate", &["--yes", &device.path])?;
        if !output.success {
            return Err(command_failed("pvcreate", &output));
        }
        let output = self
            .runner
            .run("vgcreate", &["--yes", &vg_name, &device.path])?;
        if !output.success {
            // The pvcreate above left a PV on the device: undo it so no
            // half-claimed device remains. State has not been touched yet.
            let mut detail = format!("vgcreate failed: {}", output.stderr_excerpt());
            let undone = self
                .runner
                .run("pvremove", &["--yes", &device.path])
                .is_ok_and(|undo| undo.success);
            if !undone {
                detail.push_str("; pvcreate undo failed: manual pvremove ");
                detail.push_str(&device.path);
                detail.push_str(" required");
            }
            return Err(ApiError::new(ApiErrorCode::Internal, detail));
        }
        let capacity = self
            .list_vgs()?
            .into_iter()
            .find(|row| row.vg_name.as_deref() == Some(vg_name.as_str()))
            .and_then(|row| row.size_bytes())
            .ok_or_else(|| {
                ApiError::new(
                    ApiErrorCode::Internal,
                    format!(
                        "vgcreate reported success but vgs does not report the size of \
                         {vg_name}"
                    ),
                )
            })?;

        let owner_generation = state
            .device(device_id)
            .map_or(0, |entry| entry.owner_generation)
            + 1;
        state.insert_device(
            device_id.clone(),
            DeviceEntry {
                stable_identity: device.stable_identity.clone(),
                path: device.path.clone(),
                vg_name: vg_name.clone(),
                role: DeviceRole::NativePool,
                owner_generation,
            },
        );
        state.save(&self.state_path)?;

        Ok(Pool {
            id: pool_id_for(device_id)?,
            backend_class: VolumeClass::NativeLocal,
            device_ids: vec![device_id.clone()],
            host_or_ceph_cluster: "local".to_owned(),
            capacity_bytes: capacity,
            allocatable_bytes: capacity.saturating_sub(VG_HEADROOM_BYTES),
            protection: PoolProtection::default(),
            // Unknown until proven; never a healthy default.
            health: Health::Unknown,
        })
    }

    /// Release a claimed device back to unclaimed state.
    ///
    /// Requires the destructive-authorization token carried by `request`.
    /// The device's volume group must hold zero volumes in provider state
    /// (`INVALID_STATE` otherwise). `vgremove` runs first; if it fails
    /// but `vgs` shows the VG is *verifiably absent* (a crash between
    /// `vgremove` success and the state save, or the VG was removed
    /// while the daemon was down), the release reconciles forward: the
    /// claim removal is persisted and `pvremove` proceeds. If the VG is
    /// still present — or `vgs` cannot be queried, an honest unknown —
    /// the claim stays put (state matches reality) and the error names
    /// the manual remediation (`vgremove` then `pvremove`, in that
    /// order: `pvremove` fails while the VG exists).
    ///
    /// Once the VG is gone the claim removal is persisted **immediately**,
    /// then `pvremove` runs: if it fails, the claim is *not* restored —
    /// the VG really is gone — and the error names the exact manual
    /// remediation. Success is never pretended.
    pub fn release_device(
        &self,
        device_id: &DeviceId,
        request: &ReleaseDeviceRequest,
    ) -> Result<(), ApiError> {
        request.validate()?;
        self.require_token(&request.authorization_token)?;
        let mut state = self.lock_state()?;

        let entry = state
            .device(device_id)
            .ok_or_else(|| ApiError::not_found(format!("device {device_id} is not claimed")))?
            .clone();
        let holds_volumes = state
            .volumes()
            .values()
            .any(|volume| volume.entry.vg_name == entry.vg_name);
        if holds_volumes {
            return Err(ApiError::new(
                ApiErrorCode::InvalidState,
                format!(
                    "device {device_id} still hosts volumes in {}; delete them first",
                    entry.vg_name
                ),
            ));
        }

        let vgremove = self.runner.run("vgremove", &["--yes", &entry.vg_name])?;
        if !vgremove.success && !self.vg_verifiably_absent(&entry.vg_name) {
            // The VG still exists (or its absence cannot be verified —
            // an honest unknown): keep the claim so state matches the
            // observed reality (an honest error, never a silent
            // diverge). The remediation names both steps in order —
            // pvremove fails while the VG exists.
            return Err(ApiError::new(
                ApiErrorCode::Internal,
                format!(
                    "vgremove failed: {}; manual remediation: vgremove {} then pvremove {} \
                     (then retry the release, which reconciles forward)",
                    vgremove.stderr_excerpt(),
                    entry.vg_name,
                    entry.path
                ),
            ));
        }

        // The VG is gone (removed above, or verifiably absent after a
        // failed vgremove): durably drop the claim *now*, before
        // pvremove. If pvremove fails below, the claim is not restored
        // (that would re-claim a device whose VG no longer exists); the
        // operator gets the exact manual remediation instead.
        state.remove_device(device_id);
        state.save(&self.state_path)?;

        let pvremove = self.runner.run("pvremove", &["--yes", &entry.path])?;
        if !pvremove.success {
            return Err(ApiError::new(
                ApiErrorCode::Internal,
                format!(
                    "VG removed; manual pvremove {} required (pvremove failed: {})",
                    entry.path,
                    pvremove.stderr_excerpt()
                ),
            ));
        }
        Ok(())
    }

    /// Whether a *successful* `vgs` query reports the VG as absent.
    ///
    /// Only verifiable absence counts as evidence: a failed query is an
    /// honest unknown and yields `false`, so a destructive decision is
    /// never made on missing data (the claim stays, the caller reports
    /// the original `vgremove` failure).
    fn vg_verifiably_absent(&self, vg_name: &str) -> bool {
        match self.list_vgs() {
            Ok(rows) => !rows
                .iter()
                .any(|row| row.vg_name.as_deref() == Some(vg_name)),
            Err(_) => false,
        }
    }

    /// Compare provider state against observed LVM state.
    ///
    /// Missing LVs (state entries without a backing LV) and foreign LVs
    /// (LVs under our volume groups unknown to state) are reported; the
    /// caller decides what to investigate. Nothing is adopted or destroyed
    /// here. Returns `Err` when `lvs` cannot be queried — an empty report
    /// would dishonestly imply consistency.
    pub fn reconcile_report(&self) -> Result<ReconcileReport, ApiError> {
        let state = self.lock_state()?;
        let present: Vec<String> = self
            .list_lvs()?
            .into_iter()
            .filter_map(|row| row.vg_slash_lv())
            .collect();
        let our_volumes: Vec<String> = state
            .volumes()
            .values()
            .map(|volume| format!("{}/{}", volume.entry.vg_name, volume.entry.lv_name))
            .collect();
        let our_vgs: Vec<String> = state
            .devices()
            .values()
            .map(|entry| entry.vg_name.clone())
            .collect();

        let mut report = ReconcileReport::default();
        for (volume_id, volume) in state.volumes() {
            let key = format!("{}/{}", volume.entry.vg_name, volume.entry.lv_name);
            if !present.contains(&key) {
                report.missing_volumes.push(volume_id.clone());
            }
        }
        for lv in present {
            let Some((vg, _)) = lv.split_once('/') else {
                continue;
            };
            if our_vgs.iter().any(|ours| ours == vg) && !our_volumes.contains(&lv) {
                report.foreign_lvs.push(lv);
            }
        }
        Ok(report)
    }

    /// The volume-group name this provider derives for a claimed device.
    fn claim_vg_name(&self, device_id: &DeviceId) -> String {
        format!("{}-{}", self.vg_prefix, short_hash(device_id.as_str()))
    }

    /// Verify the destructive-authorization token.
    ///
    /// The token value is never included in the error (or any log line);
    /// only the fact of the mismatch is reported.
    fn require_token(&self, authorization_token: &str) -> Result<(), ApiError> {
        if authorization_token == self.expected_auth_token {
            Ok(())
        } else {
            Err(ApiError::new(
                ApiErrorCode::UnsupportedClassOrPolicy,
                "destructive authorization token mismatch",
            ))
        }
    }
}

/// The pool identity derived from a device identity.
fn pool_id_for(device_id: &DeviceId) -> Result<PoolId, ApiError> {
    PoolId::new(format!("pool-{}", short_hash(device_id.as_str()))).map_err(|e| {
        ApiError::new(
            ApiErrorCode::Internal,
            format!("derived an invalid pool identity: {e}"),
        )
    })
}

/// The first 8 hex characters of SHA-256(input).
fn short_hash(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    crate::discover::hex_prefix(&digest, 4)
}

#[async_trait]
impl AdminSurface for LvmProvider {
    async fn discover_devices(&self) -> Result<DeviceListResponse, ApiError> {
        // Fully qualified: an inherent `discover` method with the same
        // shape exists on LvmProvider.
        LvmProvider::discover(self)
    }

    async fn claim_device(
        &self,
        device_id: &DeviceId,
        request: &ClaimDeviceRequest,
    ) -> Result<Pool, ApiError> {
        LvmProvider::claim_device(self, device_id, request)
    }

    async fn release_device(
        &self,
        device_id: &DeviceId,
        request: &ReleaseDeviceRequest,
    ) -> Result<(), ApiError> {
        LvmProvider::release_device(self, device_id, request)
    }
}
