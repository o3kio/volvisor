//! # volvisor-api
//!
//! HTTP/JSON server for the [Volume API v2](https://github.com/o3kio/volvisor)
//! contract surface (P0 scope), on top of [`volvisor_provider::VolumeProvider`]
//! backends and the [`volvisor_journal::Journal`] intent journal.
//!
//! ## Endpoints
//!
//! | Method & path | Operation |
//! |---|---|
//! | `POST /v2/volumes` | CreateVolume |
//! | `GET /v2/volumes` | ListVolumes (`?project_id=` optional filter) |
//! | `GET /v2/volumes/{volume_id}` | InspectVolume |
//! | `POST /v2/volumes/{volume_id}/attach` | AttachVolume |
//! | `POST /v2/volumes/{volume_id}/detach` | DetachVolume (body carries `attachment_id`) |
//! | `POST /v2/volumes/{volume_id}/grow` | GrowVolume |
//! | `DELETE /v2/volumes/{volume_id}` | DeleteVolume |
//! | `GET /v2/admin/devices` | device discovery (admin surface; admin token required) |
//! | `POST /v2/admin/devices/{device_id}/claim` | claim a device for a pool (admin surface) |
//! | `POST /v2/admin/devices/{device_id}/release` | release a claimed device (admin surface) |
//! | `GET /v2/capabilities` | provider name, capability set, served classes |
//! | `GET /healthz` (also `/v2/healthz`) | daemon liveness only — never volume health |
//! | `GET /metrics` | Prometheus text exposition |
//!
//! ## Idempotency and journal-before-mutate
//!
//! Every mutating endpoint — volume operations *and* the admin-surface
//! claim/release — funnels through one pipeline (the `ops` module,
//! `execute`):
//!
//! 1. typed request validation (rejecting before anything is journaled);
//! 2. journal lookup: a recorded outcome for the same `operation_id` and the
//!    same immutable request hash is replayed byte-for-byte without
//!    re-executing; a same-hash intent without an outcome fails closed with
//!    `OPERATION_IN_DOUBT`; a different hash for the same `operation_id` is an
//!    `IDEMPOTENCY_CONFLICT` (Volume API v2 section 7);
//! 3. the intent — with credential material (`encryption.key_ref`,
//!    `authorization_token`) redacted — is journaled and fsynced *before*
//!    the provider mutation;
//! 4. the provider mutation runs and its outcome — success response or typed
//!    error — is journaled and then returned, so replays are byte-compatible
//!    with the first caller's response.
//!
//! Admin claim/release request hashes exclude the authorization token by
//! design: a replayed claim after token rotation must still return the
//! recorded outcome.
//!
//! ## Error and auth model
//!
//! Every rejection carries the contract's JSON error shape
//! `{"code": ..., "message": ...}` with the status derived from the typed
//! code; axum's default rejection bodies never leak. Mutating endpoints and
//! the whole `/v2/admin` surface require `Authorization: Bearer <token>`
//! matching the configured admin token. **Fail closed**: when no admin token
//! is configured, those endpoints are rejected with `401` (the tokenless
//! mode is loopback-only dev/test; `volvisord` refuses non-loopback binds
//! without a token). Read-only volume `GET` routes stay open in P0
//! (documented on the auth extractor).
//!
//! ## Scope notes (P0)
//!
//! - The journal mutex is a `std::sync::Mutex` (synchronous fs appends);
//!   critical sections are short and never held across an `.await`.
//! - Provider calls are expected not to panic; panic-safety of the
//!   journal/mutation ordering is out of scope for P0 (see the `ops` module).
//! - Prototype evidence only (AGENTS rule 12): no production-support claims.

#![deny(unsafe_code)]
// Tests may use expect/unwrap for invariant assertions; production code may not.
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

mod config;
mod error;
mod extract;
mod handlers;
mod metrics;
mod ops;
mod router;
mod state;
#[cfg(test)]
mod tests;

pub use config::ApiConfig;
pub use metrics::Metrics;
pub use router::router;
pub use state::{AppState, SharedState};
