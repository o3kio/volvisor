//! Privileged device-enrollment surface (SPEC-0002 section 3).
//!
//! Device claiming is destructive (disk header rewrite) and therefore lives
//! outside [`crate::VolumeProvider`]: it is exposed as a separate optional
//! trait so the API layer can gate it behind admin authentication and route
//! it through the journal pipeline (durable intent before the destructive
//! action) like every other mutation.
//!
//! Implementations must honor the same fail-closed rules: a device showing
//! foreign signatures is never adopted ([`ApiErrorCode::ForeignDeviceState`]);
//! the scoped destructive-authorization token is compared and never logged;
//! release refuses devices still backing volumes.

use async_trait::async_trait;
use volvisor_types::DeviceListResponse;
use volvisor_types::domain::{
    DeviceRole, Generation, Health, PhysicalDevice, Pool, PoolProtection, VolumeClass,
};

use volvisor_types::{ApiError, ApiErrorCode, DeviceId, PoolId};

use crate::VolumeProvider;

/// Privileged enrollment operations on a provider that manages local devices.
#[async_trait]
pub trait AdminSurface: Send + Sync {
    /// Read-only device discovery. Never mutates anything: no signature
    /// scans, no wipes, no claiming (SPEC-0002 section 3).
    ///
    /// # Errors
    /// Returns [`ApiError`] when discovery cannot be performed honestly
    /// (the device list is then unknown, not empty).
    async fn discover_devices(&self) -> Result<DeviceListResponse, ApiError>;

    /// Claim a discovered device for a physical pool role: verify the device
    /// is unclaimed and shows no foreign signature, then create the backing
    /// pool. `authorization_token` is the scoped destructive-authorization
    /// token; a mismatch fails closed with
    /// [`ApiErrorCode::UnsupportedClassOrPolicy`] and is never logged.
    ///
    /// # Errors
    /// Returns [`ApiError`] on token mismatch, unknown device, foreign
    /// signature present or backend failure.
    async fn claim_device(
        &self,
        device_id: &DeviceId,
        request: &volvisor_types::ClaimDeviceRequest,
    ) -> Result<Pool, ApiError>;

    /// Release a claimed device. Refuses while any volume still resides on
    /// its pool. After a successful release the device is unclaimed.
    ///
    /// # Errors
    /// Returns [`ApiError`] on token mismatch, unknown device, volumes still
    /// present or backend failure.
    async fn release_device(
        &self,
        device_id: &DeviceId,
        request: &volvisor_types::ReleaseDeviceRequest,
    ) -> Result<(), ApiError>;
}

/// The single fake device exposed by [`crate::FakeProvider`]'s admin surface.
///
/// Note on the two-token model: the HTTP `Authorization: Bearer` admin token
/// authenticates the caller to the API; the request-body `authorization_token`
/// is the provider's scoped destructive-authorization for the device
/// operation itself. They are independent credentials. The fake does not
/// validate the body token (the LVM provider does) — production providers
/// must compare it against their configured token and never log it.
pub(crate) const FAKE_DEVICE_ID: &str = "dev-fake-1";
/// Capacity of the fake device.
pub(crate) const FAKE_DEVICE_BYTES: u64 = 1 << 40;

pub(crate) fn fake_device(claimed: bool) -> Result<PhysicalDevice, ApiError> {
    Ok(PhysicalDevice {
        id: DeviceId::new(FAKE_DEVICE_ID)?,
        host_id: volvisor_types::HostId::new("fake-host-1")?,
        namespace_ids: Vec::new(),
        capacity_bytes: FAKE_DEVICE_BYTES,
        health: Health::Unknown,
        owner_role: claimed.then_some(DeviceRole::NativePool),
        owner_generation: if claimed {
            Generation(1)
        } else {
            Generation(0)
        },
    })
}

pub(crate) fn fake_pool() -> Result<Pool, ApiError> {
    Ok(Pool {
        id: PoolId::new("pool-fake-1")?,
        backend_class: VolumeClass::NativeLocal,
        device_ids: vec![DeviceId::new(FAKE_DEVICE_ID)?],
        host_or_ceph_cluster: "fake-host-1".to_owned(),
        capacity_bytes: FAKE_DEVICE_BYTES,
        allocatable_bytes: FAKE_DEVICE_BYTES,
        protection: PoolProtection::default(),
        health: Health::Unknown,
    })
}

#[async_trait]
impl AdminSurface for crate::FakeProvider {
    async fn discover_devices(&self) -> Result<DeviceListResponse, ApiError> {
        let claimed = self.admin_device_claimed().await;
        Ok(DeviceListResponse {
            devices: vec![fake_device(claimed)?],
        })
    }

    async fn claim_device(
        &self,
        device_id: &DeviceId,
        request: &volvisor_types::ClaimDeviceRequest,
    ) -> Result<Pool, ApiError> {
        request.validate()?;
        if device_id.as_str() != FAKE_DEVICE_ID {
            return Err(ApiError::not_found(format!("unknown device {device_id}")));
        }
        if self.admin_device_claimed().await {
            return Err(ApiError::new(
                ApiErrorCode::InvalidState,
                "device is already claimed",
            ));
        }
        self.set_admin_device_claimed(true).await;
        fake_pool()
    }

    async fn release_device(
        &self,
        device_id: &DeviceId,
        request: &volvisor_types::ReleaseDeviceRequest,
    ) -> Result<(), ApiError> {
        request.validate()?;
        if device_id.as_str() != FAKE_DEVICE_ID {
            return Err(ApiError::not_found(format!("unknown device {device_id}")));
        }
        if !self.admin_device_claimed().await {
            return Err(ApiError::new(
                ApiErrorCode::InvalidState,
                "device is not claimed",
            ));
        }
        // Refuse while volumes still reside on the pool.
        let volumes = self.list_volumes(None).await?;
        if !volumes.is_empty() {
            return Err(ApiError::new(
                ApiErrorCode::InvalidState,
                format!(
                    "device still backs {} volume(s); delete them before release",
                    volumes.len()
                ),
            ));
        }
        self.set_admin_device_claimed(false).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FakeProvider;

    fn claim_req() -> volvisor_types::ClaimDeviceRequest {
        serde_json::from_value(serde_json::json!({
            "api_version": "volvisor.volume.v2",
            "operation_id": "op-claim",
            "authorization_token": "scoped"
        }))
        .expect("valid")
    }

    fn release_req() -> volvisor_types::ReleaseDeviceRequest {
        serde_json::from_value(serde_json::json!({
            "api_version": "volvisor.volume.v2",
            "operation_id": "op-release",
            "authorization_token": "scoped"
        }))
        .expect("valid")
    }

    #[tokio::test]
    async fn claim_release_lifecycle() {
        let provider = FakeProvider::new();
        let device = DeviceId::new(FAKE_DEVICE_ID).expect("id");

        let discovered = provider.discover_devices().await.expect("discover");
        assert_eq!(discovered.devices.len(), 1);
        assert_eq!(discovered.devices[0].owner_role, None);

        let pool = provider
            .claim_device(&device, &claim_req())
            .await
            .expect("claim");
        assert_eq!(pool.backend_class, VolumeClass::NativeLocal);

        // Double claim fails closed.
        assert_eq!(
            provider
                .claim_device(&device, &claim_req())
                .await
                .expect_err("double claim")
                .code,
            ApiErrorCode::InvalidState
        );

        provider
            .release_device(&device, &release_req())
            .await
            .expect("release");
        assert_eq!(
            provider
                .release_device(&device, &release_req())
                .await
                .expect_err("double release")
                .code,
            ApiErrorCode::InvalidState
        );
    }

    #[tokio::test]
    async fn unknown_device_not_found() {
        let provider = FakeProvider::new();
        let unknown = DeviceId::new("dev-other").expect("id");
        assert_eq!(
            provider
                .claim_device(&unknown, &claim_req())
                .await
                .expect_err("unknown device")
                .code,
            ApiErrorCode::NotFound
        );
    }
}
