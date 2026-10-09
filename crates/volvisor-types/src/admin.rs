//! Admin-surface request and response shapes (device enrollment).
//!
//! Device claiming is a privileged, destructive operation: it rewrites disk
//! headers (`pvcreate`/`vgcreate`). Requests therefore carry an explicit
//! scoped destructive-authorization token (operator-provided, compared by the
//! provider, never logged) and are routed through the journal pipeline like
//! every other mutation (intent durable before the destructive action).

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::ApiError;
use crate::id::{DeviceId, OperationId};

/// ClaimDevice request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaimDeviceRequest {
    /// Must equal `volvisor.volume.v2`.
    pub api_version: String,
    /// Idempotency key.
    pub operation_id: OperationId,
    /// Scoped destructive-authorization token; must match the provider's
    /// configured token. Never logged or journaled verbatim.
    pub authorization_token: String,
}

impl ClaimDeviceRequest {
    /// Validate the envelope.
    ///
    /// # Errors
    /// Returns [`ApiError`] when `api_version` is unsupported or the
    /// authorization token is empty.
    pub fn validate(&self) -> Result<(), ApiError> {
        crate::validate_api_version(&self.api_version)?;
        if self.authorization_token.is_empty() {
            return Err(ApiError::invalid_request(
                "authorization_token must not be empty",
            ));
        }
        Ok(())
    }

    /// Canonical request hash for idempotency, folded with the target device.
    /// The authorization token is excluded: it is credential material, and a
    /// re-issued claim with a rotated token for the same device and operation
    /// is a legitimate replay.
    #[must_use]
    pub fn request_hash(&self, device_id: &DeviceId) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(b"volvisor.volume.v2:claim_device:");
        hasher.update(device_id.as_str().as_bytes());
        hasher.update(b":");
        hasher.update(self.operation_id.as_str().as_bytes());
        hasher.finalize().into()
    }
}

/// ReleaseDevice request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseDeviceRequest {
    /// Must equal `volvisor.volume.v2`.
    pub api_version: String,
    /// Idempotency key.
    pub operation_id: OperationId,
    /// Scoped destructive-authorization token; never logged or journaled.
    pub authorization_token: String,
}

impl ReleaseDeviceRequest {
    /// Validate the envelope.
    ///
    /// # Errors
    /// Returns [`ApiError`] when `api_version` is unsupported or the
    /// authorization token is empty.
    pub fn validate(&self) -> Result<(), ApiError> {
        crate::validate_api_version(&self.api_version)?;
        if self.authorization_token.is_empty() {
            return Err(ApiError::invalid_request(
                "authorization_token must not be empty",
            ));
        }
        Ok(())
    }

    /// Canonical request hash for idempotency, folded with the target device.
    /// The authorization token is excluded (see
    /// [`ClaimDeviceRequest::request_hash`]).
    #[must_use]
    pub fn request_hash(&self, device_id: &DeviceId) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(b"volvisor.volume.v2:release_device:");
        hasher.update(device_id.as_str().as_bytes());
        hasher.update(b":");
        hasher.update(self.operation_id.as_str().as_bytes());
        hasher.finalize().into()
    }
}

/// DeviceList response (read-only discovery).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceListResponse {
    /// Discovered physical devices (health `Unknown` until proven).
    pub devices: Vec<crate::domain::PhysicalDevice>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claim_req() -> ClaimDeviceRequest {
        serde_json::from_value(serde_json::json!({
            "api_version": "volvisor.volume.v2",
            "operation_id": "op-claim-1",
            "authorization_token": "scoped-token"
        }))
        .expect("valid")
    }

    #[test]
    fn claim_validates() {
        assert!(claim_req().validate().is_ok());
        let mut req = claim_req();
        req.authorization_token = String::new();
        assert!(req.validate().is_err());
        req.api_version = "volvisor.volume.v1".to_owned();
        assert!(req.validate().is_err());
    }

    #[test]
    fn unknown_fields_rejected() {
        let json = serde_json::json!({
            "api_version": "volvisor.volume.v2",
            "operation_id": "op-r",
            "authorization_token": "t",
            "surprise": 1
        });
        assert!(serde_json::from_value::<ReleaseDeviceRequest>(json).is_err());
    }

    #[test]
    fn hash_excludes_token_and_folds_device() {
        let device = DeviceId::new("dev-abc").expect("id");
        let a = claim_req();
        let mut rotated = a.clone();
        rotated.authorization_token = "rotated".to_owned();
        assert_eq!(a.request_hash(&device), rotated.request_hash(&device));
        let other = DeviceId::new("dev-xyz").expect("id");
        assert_ne!(a.request_hash(&device), a.request_hash(&other));
    }
}
