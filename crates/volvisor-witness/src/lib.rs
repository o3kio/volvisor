//! # volvisor-witness
//!
//! Third-party writer-authority witness for nearline-replicated volumes
//! (nearline contract v2 §2: "A third witness may hold authority metadata
//! without holding tenant blocks"). The witness is the volvisor-level
//! arbiter that linearizes writer epochs and leases for a volume lineage;
//! it never touches tenant data.
//!
//! ## Correctness model (P4a plan §2, invariants W1–W7)
//!
//! - **W1** at most one live lease per volume at any time (`LEASE_HELD`
//!   otherwise);
//! - **W2** granting epoch `e+1` durably retires all epochs `≤ e` before
//!   the response is returned — the grant record *is* the fencing proof;
//! - **W3** state is replay-derived from the journal, so epochs and commit
//!   indices never shrink across a crash; **W3a** responses are sent only
//!   after the outcome record is fsynced, and **W3b** an intent without an
//!   outcome (crash between append and response) is rolled forward at
//!   startup from the intent's embedded, byte-identical computed response;
//! - **W4** renewing a retired epoch is a typed `STALE_EPOCH` refusal
//!   carrying the current epoch, so a fenced writer *learns* it is fenced;
//! - **W5** lease deadlines are returned as **durations from the
//!   response**, never absolute timestamps, so the residual cross-host
//!   skew is bounded by response latency (the configured grace) instead of
//!   free-running clock drift;
//! - **W6** revoking a *live* lease held by another host requires a
//!   recorded operator authorization; a power-off attestation shortens the
//!   fence wait only with positive evidence;
//! - **W7** a new grant is delayed until the previous lease's **recorded
//!   end** plus grace plus the suspend budget has passed (`FENCE_PENDING`
//!   with a retry-after duration) — keyed on the lease end because an
//!   alive-but-partitioned writer serves until its local deadline no
//!   matter how often it retries renewal. A holder's own self-release
//!   starts no wait.
//!
//! ## Layout
//!
//! - [`registry`]: [`WitnessCore`] — the durable epoch/lease registry over
//!   [`volvisor_journal::Journal`], with deterministic explicit time;
//! - [`proto`]: the versioned wire protocol types and the typed
//!   [`proto::WitnessError`] vocabulary;
//! - [`server`]: the axum HTTP surface (bearer token, fail-closed, on the
//!   same conventions as the Volume API daemon);
//! - [`client`]: the engine-neutral [`client::WitnessConnection`] trait
//!   with an HTTP implementation, which storage daemons and tests use to
//!   talk to a witness;
//! - [`blocking`]: the synchronous mirror boundary
//!   ([`blocking::BlockingWitnessConnection`]) for engine code whose
//!   control paths are synchronous;
//! - [`config`]: the witness daemon's [`config::WitnessConfig`].
//!
//! ## Honesty
//!
//! `evidence_status: PrototypeOnly` — no production, RPO or availability
//! claims (AGENTS rule 12). A single witness is a single point of failure
//! for *new* authority; established writers keep serving until their lease
//! deadline and then self-fence (the contract's documented safety/availability
//! tradeoff, never a silent weakening of fencing).

#![deny(unsafe_code)]
// Tests may use expect/unwrap for invariant assertions; production code may not.
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

pub mod blocking;
pub mod client;
pub mod config;
pub mod proto;
pub mod registry;
pub mod server;

pub use blocking::{BlockingWitness, BlockingWitnessConnection};
pub use config::WitnessConfig;
pub use proto::WITNESS_PROTOCOL_VERSION;
pub use registry::{WitnessCore, WitnessCoreConfig};
