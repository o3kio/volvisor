//! Property-based state-machine test: random legal-ish operation sequences
//! against [`FakeProvider`] must never violate the binding invariants of the
//! [`VolumeProvider`] contract, regardless of which individual operations
//! fail (typed failures are expected and allowed; invariant violations are
//! not).
//!
//! Invariants checked after every step:
//! 1. a live volume has at most one attachment and at most one writer;
//! 2. a volume's generation never decreases;
//! 3. an attach replay never fabricates a second attachment;
//! 4. after a successful delete, inspect reports `NOT_FOUND`;
//! 5. health is never reported `Healthy` (the fake never proves health).

// Property-test code: invariant assertions may use expect.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use proptest::prelude::*;
use volvisor_provider::conformance::{
    fixture_attach_request, fixture_create_request, fixture_delete_request, fixture_detach_request,
    fixture_grow_request,
};
use volvisor_provider::{FakeProvider, VolumeProvider};
use volvisor_types::{ApiErrorCode, VolumeId};

/// Volume ids the sequence draws from.
const VOLUME_IDS: [&str; 3] = ["vol-prop-1", "vol-prop-2", "vol-prop-3"];

/// One randomly chosen client operation. `expected_generation` and sizes are
/// also randomized so stale-generation and grow-only rejections occur.
#[derive(Clone, Debug)]
enum Op {
    Create {
        volume: usize,
        mebibytes: u64,
    },
    Attach {
        volume: usize,
        expected_generation: u64,
    },
    Detach {
        volume: usize,
        expected_generation: u64,
    },
    Grow {
        volume: usize,
        mebibytes: u64,
    },
    Delete {
        volume: usize,
        expected_generation: u64,
    },
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        (0usize..VOLUME_IDS.len(), 1u64..64)
            .prop_map(|(volume, mebibytes)| Op::Create { volume, mebibytes }),
        (0usize..VOLUME_IDS.len(), 0u64..8).prop_map(|(volume, expected_generation)| Op::Attach {
            volume,
            expected_generation,
        }),
        (0usize..VOLUME_IDS.len(), 0u64..8).prop_map(|(volume, expected_generation)| Op::Detach {
            volume,
            expected_generation,
        }),
        (0usize..VOLUME_IDS.len(), 1u64..128)
            .prop_map(|(volume, mebibytes)| Op::Grow { volume, mebibytes }),
        (0usize..VOLUME_IDS.len(), 0u64..8).prop_map(|(volume, expected_generation)| Op::Delete {
            volume,
            expected_generation,
        }),
    ]
}

#[derive(Default)]
struct Invariants {
    /// Highest generation ever observed per volume id, within the current
    /// incarnation. A successful delete ends an incarnation: a re-created
    /// volume legitimately restarts at generation 1 (the resulting ABA
    /// exposure for stale clients is recorded as a plan follow-up).
    max_generation: std::collections::BTreeMap<String, u64>,
    /// Volume ids successfully deleted (must stay NOT_FOUND afterwards).
    deleted: std::collections::BTreeSet<String>,
}

impl Invariants {
    async fn check(
        &mut self,
        provider: &FakeProvider,
        last_op: &str,
    ) -> Result<(), proptest::test_runner::TestCaseError> {
        for raw in VOLUME_IDS {
            let id = VolumeId::new(raw).expect("valid id");
            match provider.inspect_volume(&id).await {
                Ok(volume) => {
                    prop_assert!(
                        !self.deleted.contains(raw),
                        "{last_op}: deleted volume {raw} resurrected"
                    );
                    prop_assert!(
                        volume.attachment_ids.len() <= 1,
                        "{last_op}: volume {raw} has {} attachments",
                        volume.attachment_ids.len()
                    );
                    prop_assert!(
                        volume.current_writer.is_none() || volume.attachment_ids.len() == 1,
                        "{last_op}: writer without exactly one attachment on {raw}"
                    );
                    let seen = self.max_generation.entry(raw.to_owned()).or_insert(0);
                    prop_assert!(
                        volume.generation >= *seen,
                        "{last_op}: generation went backwards on {raw}: {} < {seen}",
                        volume.generation
                    );
                    *seen = volume.generation;
                    prop_assert!(
                        volume.health != volvisor_types::Health::Healthy,
                        "{last_op}: unproven health reported Healthy on {raw}"
                    );
                }
                Err(e) => {
                    // A missing volume must be a typed NOT_FOUND, never a
                    // generic internal error.
                    prop_assert!(
                        e.code == ApiErrorCode::NotFound,
                        "{last_op}: inspect of {raw} failed with {e}"
                    );
                }
            }
        }
        Ok(())
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn random_operation_sequences_preserve_invariants(ops in prop::collection::vec(op_strategy(), 0..40)) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("tokio runtime");
        runtime.block_on(async move {
            let provider = FakeProvider::new();
            let mut invariants = Invariants::default();

            for (step, op) in ops.iter().enumerate() {
                let volume = VOLUME_IDS[op.volume()];
                let id = VolumeId::new(volume).expect("valid id");
                let label = format!("step {step}: {op:?}");
                match op {
                    Op::Create { mebibytes, .. } => {
                        let req = fixture_create_request(volume, mebibytes * 1024 * 1024);
                        if let Ok(_v) = provider.create_volume(&req).await {
                            invariants.deleted.remove(volume);
                        }
                    }
                    Op::Attach { expected_generation, .. } => {
                        let req = fixture_attach_request(volume, "vm-prop", *expected_generation);
                        let _ = provider
                            .attach_volume(&id, &req)
                            .await;
                    }
                    Op::Detach { expected_generation, .. } => {
                        // Detach targets the attachment recorded for this
                        // volume, if any, with a randomized generation.
                        if let Ok(current) = provider.inspect_volume(&id).await {
                            if let Some(att) = current.attachment_ids.first() {
                                let req =
                                    fixture_detach_request(att.as_str(), *expected_generation);
                                let _ = provider.detach_volume(&id, att, &req).await;
                            }
                        }
                    }
                    Op::Grow { mebibytes, .. } => {
                        let req = fixture_grow_request(volume, mebibytes * 1024 * 1024, 1);
                        let _ = provider.grow_volume(&id, &req).await;
                    }
                    Op::Delete { expected_generation, .. } => {
                        let req = fixture_delete_request(volume, *expected_generation);
                        if provider.delete_volume(&id, &req).await.is_ok() {
                            invariants.deleted.insert(volume.to_owned());
                            // The incarnation ended; generation tracking
                            // restarts with the next create.
                            invariants.max_generation.remove(volume);
                        }
                    }
                }
                invariants.check(&provider, &label).await?;
            }
            Ok::<(), proptest::test_runner::TestCaseError>(())
        })?;
    }
}

impl Op {
    /// Index of the targeted volume.
    fn volume(&self) -> usize {
        match self {
            Self::Create { volume, .. }
            | Self::Attach { volume, .. }
            | Self::Detach { volume, .. }
            | Self::Grow { volume, .. }
            | Self::Delete { volume, .. } => *volume,
        }
    }
}
