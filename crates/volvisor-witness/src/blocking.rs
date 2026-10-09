//! # Synchronous witness boundary
//!
//! [`BlockingWitnessConnection`] is the synchronous mirror of
//! [`crate::client::WitnessConnection`] for engine code whose control
//! paths are synchronous and hold their own state locks end-to-end (the
//! DRBD provider's attach/detach/reconcile methods). [`BlockingWitness`]
//! adapts any async connection onto it.
//!
//! ## Mechanism (and why not `block_in_place`)
//!
//! Each call **spawns** the underlying async operation onto a captured
//! [`tokio::runtime::Handle`] and waits on a standard-library channel
//! with a bounded timeout. This deliberately avoids
//! [`tokio::task::block_in_place`]:
//!
//! - `block_in_place` panics on a current-thread runtime; the
//!   spawn-and-wait design returns a typed
//!   [`WitnessError::Unreachable`] instead (a current-thread runtime
//!   cannot host blocking witness calls — the spawned task would never
//!   run while the caller waits, and the wait expires at the bound).
//! - The wait is bounded by the connection's own per-request timeout
//!   plus `BLOCKING_WAIT_SLACK` for scheduling delay, so a stuck
//!   witness can never pin an engine thread indefinitely.
//! - The runtime `Handle` is captured when the adapter is built (inside a
//!   runtime context, e.g. the daemon's async main); the adapter itself
//!   may then be used from any thread, including outside the runtime.
//!
//! ## Honesty
//!
//! This is a control-plane convenience, not a data-path component: it is
//! meant for lease acquisition/release/renewal and authority inspection
//! — operations whose latency budget is human-scale. Nothing here
//! participates in guest I/O.

use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use volvisor_types::{AuthorityView, VolumeId};

use crate::client::WitnessConnection;
use crate::proto::{
    GrantRequest, GrantResponse, RegisterRequest, RegisterResponse, RenewRequest, RenewResponse,
    RevokeRequest, RevokeResponse, WitnessError,
};

/// Extra wait granted beyond the wrapped connection's per-request
/// timeout, covering runtime scheduling delay before the result lands in
/// the channel. The future itself expires at the request timeout and
/// sends its typed result, so under normal operation the slack is never
/// consumed.
const BLOCKING_WAIT_SLACK: Duration = Duration::from_secs(5);

/// Synchronous mirror of the witness protocol (see the module docs for
/// when to prefer [`crate::client::WitnessConnection`]).
///
/// Every method returns the same typed [`WitnessError`] vocabulary; a
/// transport failure surfaces as [`WitnessError::Unreachable`], exactly
/// like the async client.
pub trait BlockingWitnessConnection: Send + Sync {
    /// Register a volume lineage (idempotent by content).
    ///
    /// # Errors
    /// Typed witness refusals, or [`WitnessError::Unreachable`] on
    /// transport failure.
    fn register(
        &self,
        volume_id: &VolumeId,
        request: RegisterRequest,
    ) -> Result<RegisterResponse, WitnessError>;

    /// Grant writer authority (W1/W2/W7 enforced witness-side).
    ///
    /// # Errors
    /// Typed witness refusals, or [`WitnessError::Unreachable`] on
    /// transport failure.
    fn grant(
        &self,
        volume_id: &VolumeId,
        request: GrantRequest,
    ) -> Result<GrantResponse, WitnessError>;

    /// Renew the current lease (W4/W5).
    ///
    /// # Errors
    /// Typed witness refusals, or [`WitnessError::Unreachable`] on
    /// transport failure.
    fn renew(
        &self,
        volume_id: &VolumeId,
        request: RenewRequest,
    ) -> Result<RenewResponse, WitnessError>;

    /// Revoke/release the current lease (W6).
    ///
    /// # Errors
    /// Typed witness refusals, or [`WitnessError::Unreachable`] on
    /// transport failure.
    fn revoke(
        &self,
        volume_id: &VolumeId,
        request: RevokeRequest,
    ) -> Result<RevokeResponse, WitnessError>;

    /// Read the authority view.
    ///
    /// # Errors
    /// Typed witness refusals, or [`WitnessError::Unreachable`] on
    /// transport failure.
    fn inspect(&self, volume_id: &VolumeId) -> Result<AuthorityView, WitnessError>;
}

/// Adapter running an async [`WitnessConnection`] to completion from
/// synchronous code (spawn onto a captured runtime handle, wait on a
/// channel with a bounded timeout — see the module docs).
pub struct BlockingWitness<C> {
    inner: Arc<C>,
    handle: tokio::runtime::Handle,
    request_timeout: Duration,
}

impl<C> BlockingWitness<C>
where
    C: WitnessConnection + 'static,
{
    /// Capture `handle` (from inside a runtime context) and adapt
    /// `inner`. `request_timeout` must match the wrapped connection's
    /// per-request timeout so the wait bound is honest; the adapter adds
    /// only `BLOCKING_WAIT_SLACK` on top.
    #[must_use]
    pub fn new(inner: Arc<C>, handle: tokio::runtime::Handle, request_timeout: Duration) -> Self {
        Self {
            inner,
            handle,
            request_timeout,
        }
    }

    /// Run one async operation to completion on the captured handle.
    fn wait<T, F>(&self, future: F) -> Result<T, WitnessError>
    where
        F: std::future::Future<Output = Result<T, WitnessError>> + Send + 'static,
        T: Send + 'static,
    {
        let (sender, receiver) = mpsc::channel();
        // Spawn failure is not a shape tokio exposes (`Handle::spawn`
        // returns a `JoinHandle`, never an error); the relevant
        // failure mode is a runtime shut down mid-wait, which drops
        // the task without polling it — the sender drops with it and
        // the `Disconnected` arm below answers immediately, so the
        // caller never burns the blocking bound on a dead runtime.
        self.handle.spawn(async move {
            // A send failure only means the caller gave up waiting; the
            // result is then simply dropped.
            let _ = sender.send(future.await);
        });
        let bound = self.request_timeout + BLOCKING_WAIT_SLACK;
        match receiver.recv_timeout(bound) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => Err(WitnessError::Unreachable(format!(
                "witness call did not complete within the {} ms blocking bound",
                bound.as_millis()
            ))),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(WitnessError::Internal(
                "witness call task terminated without a result".to_owned(),
            )),
        }
    }
}

impl<C> BlockingWitnessConnection for BlockingWitness<C>
where
    C: WitnessConnection + 'static,
{
    fn register(
        &self,
        volume_id: &VolumeId,
        request: RegisterRequest,
    ) -> Result<RegisterResponse, WitnessError> {
        let inner = Arc::clone(&self.inner);
        let volume_id = volume_id.clone();
        self.wait(async move { inner.register(&volume_id, request).await })
    }

    fn grant(
        &self,
        volume_id: &VolumeId,
        request: GrantRequest,
    ) -> Result<GrantResponse, WitnessError> {
        let inner = Arc::clone(&self.inner);
        let volume_id = volume_id.clone();
        self.wait(async move { inner.grant(&volume_id, request).await })
    }

    fn renew(
        &self,
        volume_id: &VolumeId,
        request: RenewRequest,
    ) -> Result<RenewResponse, WitnessError> {
        let inner = Arc::clone(&self.inner);
        let volume_id = volume_id.clone();
        self.wait(async move { inner.renew(&volume_id, request).await })
    }

    fn revoke(
        &self,
        volume_id: &VolumeId,
        request: RevokeRequest,
    ) -> Result<RevokeResponse, WitnessError> {
        let inner = Arc::clone(&self.inner);
        let volume_id = volume_id.clone();
        self.wait(async move { inner.revoke(&volume_id, request).await })
    }

    fn inspect(&self, volume_id: &VolumeId) -> Result<AuthorityView, WitnessError> {
        let inner = Arc::clone(&self.inner);
        let volume_id = volume_id.clone();
        self.wait(async move { inner.inspect(&volume_id).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::WITNESS_PROTOCOL_VERSION;
    use volvisor_types::HostId;

    /// An async double that never answers: proves the bounded wait
    /// returns a typed Unreachable instead of hanging.
    struct StalledConnection;

    #[async_trait::async_trait]
    impl WitnessConnection for StalledConnection {
        async fn register(
            &self,
            _volume_id: &VolumeId,
            _request: RegisterRequest,
        ) -> Result<RegisterResponse, WitnessError> {
            std::future::pending().await
        }

        async fn grant(
            &self,
            _volume_id: &VolumeId,
            _request: GrantRequest,
        ) -> Result<GrantResponse, WitnessError> {
            std::future::pending().await
        }

        async fn renew(
            &self,
            _volume_id: &VolumeId,
            _request: RenewRequest,
        ) -> Result<RenewResponse, WitnessError> {
            std::future::pending().await
        }

        async fn revoke(
            &self,
            _volume_id: &VolumeId,
            _request: RevokeRequest,
        ) -> Result<RevokeResponse, WitnessError> {
            std::future::pending().await
        }

        async fn inspect(&self, _volume_id: &VolumeId) -> Result<AuthorityView, WitnessError> {
            std::future::pending().await
        }
    }

    fn volume() -> VolumeId {
        VolumeId::new("vol-1").expect("valid volume id")
    }

    fn grant_request() -> GrantRequest {
        GrantRequest {
            protocol_version: WITNESS_PROTOCOL_VERSION,
            operation_id: volvisor_types::OperationId::new("op-1").expect("valid op id"),
            host_id: HostId::new("node-a").expect("valid host id"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stalled_connection_times_out_typed() {
        let blocking = BlockingWitness::new(
            Arc::new(StalledConnection),
            tokio::runtime::Handle::current(),
            Duration::from_millis(50),
        );
        let start = std::time::Instant::now();
        let err = blocking
            .grant(&volume(), grant_request())
            .expect_err("stalled call is unreachable");
        assert!(matches!(err, WitnessError::Unreachable(_)));
        // The bound held: the wait expires at the request timeout plus
        // the fixed slack (50 ms + 5 s here), never at an unbounded
        // hang.
        let elapsed = start.elapsed();
        assert!(elapsed >= Duration::from_millis(50));
        assert!(elapsed < Duration::from_secs(7), "wait took {elapsed:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn typed_results_pass_through() {
        // A double that answers with a typed refusal: the blocking
        // boundary must surface it unchanged.
        struct RefusingConnection;
        #[async_trait::async_trait]
        impl WitnessConnection for RefusingConnection {
            async fn register(
                &self,
                _volume_id: &VolumeId,
                _request: RegisterRequest,
            ) -> Result<RegisterResponse, WitnessError> {
                Err(WitnessError::UnknownVolume)
            }
            async fn grant(
                &self,
                _volume_id: &VolumeId,
                _request: GrantRequest,
            ) -> Result<GrantResponse, WitnessError> {
                Err(WitnessError::LeaseHeld {
                    current_epoch: volvisor_types::WriterEpoch(3),
                })
            }
            async fn renew(
                &self,
                _volume_id: &VolumeId,
                _request: RenewRequest,
            ) -> Result<RenewResponse, WitnessError> {
                Err(WitnessError::StaleEpoch {
                    current_epoch: volvisor_types::WriterEpoch(4),
                })
            }
            async fn revoke(
                &self,
                _volume_id: &VolumeId,
                _request: RevokeRequest,
            ) -> Result<RevokeResponse, WitnessError> {
                Err(WitnessError::Unauthorized)
            }
            async fn inspect(&self, _volume_id: &VolumeId) -> Result<AuthorityView, WitnessError> {
                Err(WitnessError::Unreachable("peer reset".to_owned()))
            }
        }
        let blocking = BlockingWitness::new(
            Arc::new(RefusingConnection),
            tokio::runtime::Handle::current(),
            Duration::from_secs(1),
        );
        assert_eq!(
            blocking.grant(&volume(), grant_request()).unwrap_err(),
            WitnessError::LeaseHeld {
                current_epoch: volvisor_types::WriterEpoch(3)
            }
        );
        assert_eq!(
            blocking
                .register(
                    &volume(),
                    RegisterRequest {
                        protocol_version: WITNESS_PROTOCOL_VERSION,
                        operation_id: volvisor_types::OperationId::new("op-2")
                            .expect("valid op id"),
                        content: crate::proto::RegistrationContent {
                            lineage_uuids: vec!["0000000000000004".to_owned()],
                            endpoints: Vec::new(),
                            barrier: None,
                        },
                    },
                )
                .unwrap_err(),
            WitnessError::UnknownVolume
        );
        assert_eq!(
            blocking
                .revoke(
                    &volume(),
                    RevokeRequest {
                        protocol_version: WITNESS_PROTOCOL_VERSION,
                        operation_id: volvisor_types::OperationId::new("op-3")
                            .expect("valid op id"),
                        host_id: HostId::new("node-a").expect("valid host id"),
                        epoch: volvisor_types::WriterEpoch(1),
                        authorization: None,
                        power_off: None,
                    },
                )
                .unwrap_err(),
            WitnessError::Unauthorized
        );
    }
}
