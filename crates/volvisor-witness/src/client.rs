//! # Witness client
//!
//! [`WitnessConnection`] is the engine-neutral boundary the storage
//! daemon and the DRBD provider's authority integration program against
//! (P4a plan §3): one trait, implemented by [`HttpWitnessConnection`]
//! over real HTTP/JSON and by test doubles that mirror the same
//! semantics. [`WitnessError::Unreachable`] is the transport-failure
//! signal a writer uses to keep serving until its W5 local deadline —
//! it is never produced by the server, only by the transport.

use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request, Uri};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde::Serialize;
use serde::de::DeserializeOwned;
use volvisor_types::error::ApiErrorBody;
use volvisor_types::{AuthorityView, VolumeId};

use crate::proto::{
    GrantRequest, GrantResponse, GrantSetRequest, GrantSetResponse, RecordBarrierRequest,
    RecordBarrierResponse, RegisterRequest, RegisterResponse, RenewRequest, RenewResponse,
    RevokeRequest, RevokeResponse, RevokeSetRequest, RevokeSetResponse, VoidBarrierRequest,
    VoidBarrierResponse, WitnessError,
};

/// The witness protocol surface, as seen by a storage daemon.
#[async_trait]
pub trait WitnessConnection: Send + Sync {
    /// Register a volume lineage (P4a plan §3; idempotent by content).
    ///
    /// # Errors
    /// Typed witness refusals, or [`WitnessError::Unreachable`] on
    /// transport failure.
    async fn register(
        &self,
        volume_id: &VolumeId,
        request: RegisterRequest,
    ) -> Result<RegisterResponse, WitnessError>;

    /// Grant writer authority (W1/W2/W7 enforced witness-side).
    ///
    /// # Errors
    /// Typed witness refusals, or [`WitnessError::Unreachable`] on
    /// transport failure.
    async fn grant(
        &self,
        volume_id: &VolumeId,
        request: GrantRequest,
    ) -> Result<GrantResponse, WitnessError>;

    /// Renew the current lease (W4/W5).
    ///
    /// # Errors
    /// Typed witness refusals, or [`WitnessError::Unreachable`] on
    /// transport failure.
    async fn renew(
        &self,
        volume_id: &VolumeId,
        request: RenewRequest,
    ) -> Result<RenewResponse, WitnessError>;

    /// Revoke/release the current lease (W6).
    ///
    /// # Errors
    /// Typed witness refusals, or [`WitnessError::Unreachable`] on
    /// transport failure.
    async fn revoke(
        &self,
        volume_id: &VolumeId,
        request: RevokeRequest,
    ) -> Result<RevokeResponse, WitnessError>;

    /// Record a migration barrier (P4b plan §4 W9). The witness stamps
    /// the boundary commit index and recording time; the caller must
    /// present the epoch holder's host credential (W8 — the server
    /// resolves the identity from the bearer token this connection was
    /// built with).
    ///
    /// # Errors
    /// Typed witness refusals (including
    /// [`WitnessError::IdentityRequired`] when the connection's token
    /// is not bound to the asserted holder), or
    /// [`WitnessError::Unreachable`] on transport failure.
    async fn record_barrier(
        &self,
        volume_id: &VolumeId,
        request: RecordBarrierRequest,
    ) -> Result<RecordBarrierResponse, WitnessError>;

    /// Void a recorded barrier (P4b plan §4 W9) — the abort path's
    /// evidence-hygiene step, only from the recording holder before
    /// the epoch retires.
    ///
    /// # Errors
    /// Typed witness refusals (including
    /// [`WitnessError::IdentityRequired`]), or
    /// [`WitnessError::Unreachable`] on transport failure.
    async fn void_barrier(
        &self,
        volume_id: &VolumeId,
        request: VoidBarrierRequest,
    ) -> Result<VoidBarrierResponse, WitnessError>;

    /// Batch self-release (P4b plan §4 W10 `revoke-set`): one host
    /// releasing every member lease in one journaled, all-or-nothing
    /// mutation.
    ///
    /// # Errors
    /// Typed witness refusals (including
    /// [`WitnessError::IdentityRequired`]), or
    /// [`WitnessError::Unreachable`] on transport failure.
    async fn revoke_set(
        &self,
        request: RevokeSetRequest,
    ) -> Result<RevokeSetResponse, WitnessError>;

    /// Batch grant (P4b plan §4 W10 `grant-set`): one host acquiring
    /// writer authority for every member volume in one journaled,
    /// all-or-nothing mutation.
    ///
    /// # Errors
    /// Typed witness refusals (including
    /// [`WitnessError::IdentityRequired`]), or
    /// [`WitnessError::Unreachable`] on transport failure.
    async fn grant_set(&self, request: GrantSetRequest) -> Result<GrantSetResponse, WitnessError>;

    /// Read the authority view (P4a plan §3 `inspect`).
    ///
    /// # Errors
    /// Typed witness refusals, or [`WitnessError::Unreachable`] on
    /// transport failure.
    async fn inspect(&self, volume_id: &VolumeId) -> Result<AuthorityView, WitnessError>;
}

/// HTTP implementation of [`WitnessConnection`] (hyper legacy client,
/// JSON bodies, optional bearer token, per-request timeout).
pub struct HttpWitnessConnection {
    base_url: String,
    token: Option<String>,
    client: Client<hyper_util::client::legacy::connect::HttpConnector, Full<Bytes>>,
    timeout: Duration,
}

impl HttpWitnessConnection {
    /// Build a connection to `base_url` (e.g. `http://127.0.0.1:9101`)
    /// with an optional bearer token and per-request timeout.
    #[must_use]
    pub fn new(base_url: impl Into<String>, token: Option<String>, timeout: Duration) -> Self {
        Self {
            base_url: base_url.into(),
            token,
            client: Client::builder(TokioExecutor::new()).build_http(),
            timeout,
        }
    }

    /// Issue one request and decode the typed result.
    ///
    /// Success bodies decode as `T`; error bodies decode through the
    /// contract error shape back into [`WitnessError`] (including the
    /// `retry_after_secs` sub-format for `FENCE_PENDING`).
    async fn request<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<&(impl Serialize + Sync)>,
    ) -> Result<T, WitnessError> {
        let uri = format!("{}{path}", self.base_url)
            .parse::<Uri>()
            .map_err(|err| WitnessError::Unreachable(format!("invalid witness base URL: {err}")))?;
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(token) = &self.token {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        let payload =
            match body {
                Some(body) => Some(serde_json::to_vec(body).map_err(|err| {
                    WitnessError::Internal(format!("serialization failure: {err}"))
                })?),
                None => None,
            };
        let request = builder
            .header("content-type", "application/json")
            .body(Full::new(Bytes::from(payload.unwrap_or_default())))
            .map_err(|err| WitnessError::Unreachable(format!("request build failure: {err}")))?;
        let response = tokio::time::timeout(self.timeout, self.client.request(request))
            .await
            .map_err(|_| {
                WitnessError::Unreachable(format!(
                    "witness request timed out after {} ms",
                    self.timeout.as_millis()
                ))
            })?
            .map_err(|err| {
                WitnessError::Unreachable(format!("witness transport failure: {err}"))
            })?;
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .map_err(|err| WitnessError::Unreachable(format!("witness body failure: {err}")))?
            .to_bytes();
        if status.is_success() {
            return serde_json::from_slice(&bytes).map_err(|err| {
                WitnessError::Internal(format!("unparseable witness response: {err}"))
            });
        }
        if status == hyper::StatusCode::UNAUTHORIZED {
            return Err(WitnessError::Unauthorized);
        }
        let body = serde_json::from_slice::<ApiErrorBody>(&bytes).map_err(|err| {
            WitnessError::Internal(format!("unparseable witness error body: {err}"))
        })?;
        Err(WitnessError::from_wire(false, &body))
    }
}

#[async_trait]
impl WitnessConnection for HttpWitnessConnection {
    async fn register(
        &self,
        volume_id: &VolumeId,
        request: RegisterRequest,
    ) -> Result<RegisterResponse, WitnessError> {
        self.request(
            Method::POST,
            &format!("/v1/volumes/{}/register", volume_id.as_str()),
            Some(&request),
        )
        .await
    }

    async fn grant(
        &self,
        volume_id: &VolumeId,
        request: GrantRequest,
    ) -> Result<GrantResponse, WitnessError> {
        self.request(
            Method::POST,
            &format!("/v1/volumes/{}/grant", volume_id.as_str()),
            Some(&request),
        )
        .await
    }

    async fn renew(
        &self,
        volume_id: &VolumeId,
        request: RenewRequest,
    ) -> Result<RenewResponse, WitnessError> {
        self.request(
            Method::POST,
            &format!("/v1/volumes/{}/renew", volume_id.as_str()),
            Some(&request),
        )
        .await
    }

    async fn revoke(
        &self,
        volume_id: &VolumeId,
        request: RevokeRequest,
    ) -> Result<RevokeResponse, WitnessError> {
        self.request(
            Method::POST,
            &format!("/v1/volumes/{}/revoke", volume_id.as_str()),
            Some(&request),
        )
        .await
    }

    async fn record_barrier(
        &self,
        volume_id: &VolumeId,
        request: RecordBarrierRequest,
    ) -> Result<RecordBarrierResponse, WitnessError> {
        self.request(
            Method::POST,
            &format!("/v1/volumes/{}/record-barrier", volume_id.as_str()),
            Some(&request),
        )
        .await
    }

    async fn void_barrier(
        &self,
        volume_id: &VolumeId,
        request: VoidBarrierRequest,
    ) -> Result<VoidBarrierResponse, WitnessError> {
        self.request(
            Method::POST,
            &format!("/v1/volumes/{}/void-barrier", volume_id.as_str()),
            Some(&request),
        )
        .await
    }

    async fn revoke_set(
        &self,
        request: RevokeSetRequest,
    ) -> Result<RevokeSetResponse, WitnessError> {
        self.request(Method::POST, "/v1/batch/revoke-set", Some(&request))
            .await
    }

    async fn grant_set(&self, request: GrantSetRequest) -> Result<GrantSetResponse, WitnessError> {
        self.request(Method::POST, "/v1/batch/grant-set", Some(&request))
            .await
    }

    async fn inspect(&self, volume_id: &VolumeId) -> Result<AuthorityView, WitnessError> {
        self.request(
            Method::GET,
            &format!("/v1/volumes/{}", volume_id.as_str()),
            None::<&RegisterRequest>,
        )
        .await
    }
}
