//! Administrative (operator-facing) operations on the LVM provider.
//!
//! These are *inherent* methods on [`LvmProvider`], deliberately not part
//! of the [`VolumeProvider`](volvisor_provider::VolumeProvider) trait: they
//! are host-administration concerns (discovery, device claiming, release,
//! reconciliation reporting) rather than tenant-facing volume operations.
//!
//! Claiming and releasing a device are privileged, destructive operations:
//! both require the scoped destructive-authorization token configured at
//! construction. A mismatch fails with `UNSUPPORTED_CLASS_OR_POLICY` and
//! the token itself is never included in any error or log line.

use sha2::{Digest, Sha256};
use volvisor_types::domain::{DeviceRole, Health, Pool, PoolProtection, VolumeClass};
use volvisor_types::{ApiError, ApiErrorCode, DeviceId, PoolId};

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
    pub fn discover(&self) -> Result<Vec<volvisor_types::domain::PhysicalDevice>, ApiError> {
        discover::discover_devices(self.runner.as_ref(), &self.sysfs_root)
    }

    /// Claim a discovered device as a native-local pool.
    ///
    /// Requires the destructive-authorization token. The device must be
    /// discoverable with a stable identity, must not already be claimed,
    /// and `pvs` must show **no existing physical volume** on its path —
    /// foreign LVM state is never adopted (`FOREIGN_DEVICE_STATE`).
    /// On success the device holds a fresh `pvcreate` + `vgcreate` volume
    /// group named `<vg_prefix>-<8 hex of sha256(device_id)>`, the claim is
    /// persisted, and the resulting [`Pool`] is returned.
    pub fn claim_device(
        &self,
        device_id: &DeviceId,
        authorization_token: &str,
    ) -> Result<Pool, ApiError> {
        self.require_token(authorization_token)?;
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
                return Err(ApiError::new(
                    ApiErrorCode::ForeignDeviceState,
                    format!(
                        "device path {} already hosts a physical volume; foreign state is \
                         never adopted",
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
            return Err(command_failed("vgcreate", &output));
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
    /// Requires the destructive-authorization token. The device's volume
    /// group must hold zero volumes in provider state (`INVALID_STATE`
    /// otherwise); then the claim is durably removed (intent before
    /// mutate) and `vgremove` + `pvremove` are executed. If the LVM
    /// commands fail, the claim is restored to state and the honest
    /// `INTERNAL` error is returned — success is never pretended.
    pub fn release_device(
        &self,
        device_id: &DeviceId,
        authorization_token: &str,
    ) -> Result<(), ApiError> {
        self.require_token(authorization_token)?;
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

        // Intent before mutate: durably drop the claim, then destroy the
        // LVM objects. On failure the claim is restored below.
        state.remove_device(device_id);
        state.save(&self.state_path)?;

        let vgremove = self.runner.run("vgremove", &["--yes", &entry.vg_name]);
        let pvremove = vgremove
            .and_then(|output| {
                if output.success {
                    Ok(output)
                } else {
                    Err(command_failed("vgremove", &output))
                }
            })
            .and_then(|_| self.runner.run("pvremove", &["--yes", &entry.path]));
        if let Err(error) = pvremove.and_then(|output| {
            if output.success {
                Ok(())
            } else {
                Err(command_failed("pvremove", &output))
            }
        }) {
            // Best-effort restore so the claim is not lost to a failed
            // release; the error still reports the truth.
            state.insert_device(device_id.clone(), entry);
            drop(state.save(&self.state_path));
            return Err(error);
        }
        Ok(())
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
