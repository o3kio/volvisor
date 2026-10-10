//! The write-trace oracle (P5 plan §2.2/§2.3): the campaign's
//! independent model of the guest's I/O and the verdicts it
//! derives from bytes, never from the coordinator's records.
//!
//! - [`WriterHandle`]: a continuous writer of 4 KiB
//!   tagged blocks (monotonic sequence, crc32, writer id) through
//!   the kit's enforced [`DeviceHandle`] path, participating in the
//!   openers model as guest I/O — it holds the device open while
//!   the VM is Running and releases it when the VM pauses or the
//!   data path refuses (a held handle would block the cut's demote,
//!   exactly as a real guest's open device does; rule 17).
//! - The acknowledged journal (in-memory, outside the daemons):
//!   every write that returned an ack, with the shared frozen
//!   clock's value at ack time (the boundary cross-check's input).
//! - The **data-path boundary** (§2.3, rule 1): the last write
//!   sequence that succeeded — derived from what the device
//!   accepted, never from coordinator records. The writer's I/O
//!   ends at the VM pause (guest I/O stops) or at the first
//!   refused write (the suspension or role flip the migration
//!   actually caused).
//! - [`verify_against`]: the terminal-state verdict —
//!   acknowledged-prefix-present-and-crc-correct at the verified
//!   side, corruption as a distinct failure class, and the tail
//!   (acknowledged writes absent at the peer) as the honest
//!   quantification abort paths report, never called loss and never
//!   silently zero.
//!
//! Rule 16 discipline: the ack model is source-side (Protocol
//! C-shaped up to the source's own map); no zero-RPO claim is made
//! or tested for asynchronous acknowledgement — the peer-apply
//! window makes the tail explicit.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use volvisor_drbd_testkit::{BLOCK_SIZE, DeviceHandle, FakeDrbd, open_device, read_raw};
use volvisor_provider::{FakeVmm, VmState};

/// The oracle's writer identity (part of every block tag).
pub const WRITER_ID: u32 = 0x5245_4C49; // "RELI"

/// The writer's loop tick (§2.2's bounded rate, split from the write
/// cadence): the VM-state notice runs every millisecond so the
/// handle release lands well inside a real cut's pause-to-demote
/// span (rule 17: a held-open source device refuses the demote),
/// while the write cadence below keeps the rate bounded.
const NOTICE_STEP: Duration = Duration::from_millis(1);
/// One write attempt every [`NOTICE_STEP`] ticks — ~250 writes/s,
/// far below the device's 262 144-block surface, so every write's
/// block index is unique.
const WRITE_EVERY_TICKS: u64 = 4;

/// How far the writer may advance the shared frozen clock (one tick
/// per ack): the boundary cross-check needs the coordinator's
/// `BARRIER_DURABLE` timestamp to be comparable to ack times, and
/// the total advance stays far under the 100 s lease TTL so no
/// lease window is disturbed.
const CLOCK_ADVANCE_CAP: u64 = 40;

/// The block tag's magic (`VCW1` — volvisor campaign writer v1).
const MAGIC: [u8; 4] = *b"VCW1";

/// One acknowledged write: the writer's tag sequence, the device's
/// ack sequence (the drain identity `apply_peer_writes` models at
/// the peer), the block index and the shared frozen clock at ack
/// time.
#[derive(Clone, Debug)]
pub struct AckedWrite {
    /// The writer's monotonic tag sequence (in the payload).
    pub seq: u64,
    /// The device's own write-sequence (the queue's ack identity).
    pub ack: u64,
    /// The block index the write landed at (unique per scenario).
    pub index: u64,
    /// The shared frozen clock at ack time.
    pub clock: u64,
}

/// Why the writer's I/O ended (the boundary's provenance — reported
/// in the evidence record, never assumed).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopReason {
    /// The VM paused — guest I/O stops at the pause (§2.1).
    Paused,
    /// The data path refused the write (the suspension or role flip
    /// the migration actually caused — the §2.3 boundary), with the
    /// typed refusal's detail.
    Refused(String),
    /// The rig stopped the writer (scenario end; no boundary exists).
    StoppedByRig,
}

/// Build one tagged block: magic, sequence, writer id, crc32 over
/// the payload fill, and a sequence-derived fill so a stale or
/// wrong-content block cannot pass verification by accident.
#[must_use]
pub fn tag_block(seq: u64, writer_id: u32) -> [u8; BLOCK_SIZE] {
    let mut block = [0_u8; BLOCK_SIZE];
    block[0..4].copy_from_slice(&MAGIC);
    block[4..12].copy_from_slice(&seq.to_le_bytes());
    block[12..16].copy_from_slice(&writer_id.to_le_bytes());
    for (index, byte) in block[20..].iter_mut().enumerate() {
        *byte = (seq.wrapping_add(index as u64) & 0xff) as u8 ^ 0x5a;
    }
    let crc = crc32fast::hash(&block[20..]);
    block[16..20].copy_from_slice(&crc.to_le_bytes());
    block
}

/// Check one block's content against the expected tag: `Ok(())`
/// when present and crc-correct, `Err(Corrupted)` when present with
/// wrong content.
fn check_payload(payload: &[u8; BLOCK_SIZE], seq: u64, writer_id: u32) -> Result<(), ()> {
    if payload[0..4] != MAGIC
        || payload[4..12] != seq.to_le_bytes()
        || payload[12..16] != writer_id.to_le_bytes()
    {
        return Err(());
    }
    let crc = crc32fast::hash(&payload[20..]);
    if payload[16..20] != crc.to_le_bytes() {
        return Err(());
    }
    Ok(())
}

/// The terminal-state verdict of one verified side (§2.2): what the
/// bytes say, in the three classes the contracts distinguish.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Verdict {
    /// Acknowledged writes the verdict covers.
    pub acknowledged: u64,
    /// Present and crc-correct.
    pub present: u64,
    /// Present with wrong content — the distinct corruption class.
    pub corrupted: u64,
    /// Absent (the sparse zero fill — never written, or never
    /// applied at this side).
    pub missing: u64,
}

impl Verdict {
    /// The honest tail (peer-apply lag): acknowledged writes absent
    /// at the verified side. Corruption is never reported as tail —
    /// it is the louder failure class.
    #[must_use]
    pub fn tail(&self) -> u64 {
        self.missing
    }

    /// The acknowledged-prefix property (COMPLETE paths): every
    /// acknowledged write present and crc-correct.
    #[must_use]
    pub fn prefix_intact(&self) -> bool {
        self.present == self.acknowledged && self.corrupted == 0 && self.missing == 0
    }
}

/// Verify every acknowledged write against one world's device bytes
/// (§2.2): `COMPLETE` paths verify the destination, abort paths the
/// source; the tail check verifies the peer replica. Reads go
/// through `read_raw` — post-mortem bytes are the ground truth, and
/// a fenced, demoted or suspended resource must still be readable
/// (the escape hatch exists for exactly this).
#[must_use]
pub fn verify_against(
    world: &Arc<Mutex<FakeDrbd>>,
    minor: u32,
    acked: &[AckedWrite],
    writer_id: u32,
) -> Verdict {
    let mut verdict = Verdict {
        acknowledged: acked.len() as u64,
        ..Verdict::default()
    };
    for write in acked {
        let payload = read_raw(world, minor, write.index)
            .unwrap_or_else(|error| panic!("oracle read of block {}: {error}", write.index))
            .payload;
        if payload.iter().all(|byte| *byte == 0) {
            verdict.missing += 1;
        } else if check_payload(&payload, write.seq, writer_id).is_ok() {
            verdict.present += 1;
        } else {
            verdict.corrupted += 1;
        }
    }
    verdict
}

/// The coordinator's `BARRIER_DURABLE` history timestamp, from the
/// public observation route's summary (the cross-check's other
/// side; §2.3 rule 1).
#[must_use]
pub fn barrier_timestamp(summary: &serde_json::Value) -> Option<u64> {
    summary["state_history"]
        .as_array()?
        .iter()
        .find_map(|entry| {
            (entry["state"] == "barrier_durable")
                .then(|| entry["at"].as_u64())
                .flatten()
        })
}

/// The boundary cross-check (§2.3 rule 1): the coordinator recorded
/// the barrier durable at `barrier_at` — no acknowledged write may
/// carry a LATER clock value than the barrier it should be covered
/// by (a coordinator that records the barrier before the writes it
/// should have covered is a boundary-skew violation; a negative
/// result fails the check).
#[must_use]
pub fn boundary_skew(acked: &[AckedWrite], barrier_at: u64) -> Option<i64> {
    let last = acked.last().map(|write| write.clock)?;
    Some(
        i64::try_from(barrier_at).expect("the clock fits an i64")
            - i64::try_from(last).expect("the clock fits an i64"),
    )
}

// ------------------------------------------------------------ the writer

/// The writer's interior state (owned by the spawned task's loop,
/// read through the handle's snapshot).
struct GuestWriter {
    world: Arc<Mutex<FakeDrbd>>,
    vmm: Arc<FakeVmm>,
    vm_id: String,
    minor: u32,
    clock: Arc<std::sync::atomic::AtomicU64>,
    /// How many clock ticks the writer has advanced (capped).
    clock_advances: u64,
    /// The acknowledged journal.
    acked: Vec<AckedWrite>,
    /// The notice-tick counter (the write cadence's divisor).
    ticks: u64,
    /// The next tag sequence to write.
    next_seq: u64,
    /// The open device handle (held while the VM is Running).
    handle: Option<DeviceHandle>,
    /// Whether the writer has stopped (and why).
    stopped: Option<StopReason>,
}

impl GuestWriter {
    /// One notice tick: the guest-I/O model of §2.1 — the VM state is
    /// checked every tick (the handle releases within one
    /// millisecond of the pause, rule 17), a write is attempted every
    /// [`WRITE_EVERY_TICKS`]th tick, and the writer stops at the
    /// pause or the first refused write (the data-path boundary),
    /// releasing the handle either way (a held handle would block
    /// the cut's demote).
    fn tick(&mut self) {
        if self.stopped.is_some() {
            return;
        }
        let running = self
            .vmm
            .vm_state(&self.vm_id)
            .is_ok_and(|state| state == VmState::Running);
        if !running {
            // Guest I/O stops when the VM leaves Running (paused or
            // destroyed); the device releases with the guest.
            self.handle = None;
            self.stopped = Some(StopReason::Paused);
            return;
        }
        self.ticks += 1;
        if self.ticks % WRITE_EVERY_TICKS != 0 {
            return;
        }
        if self.handle.is_none() {
            match open_device(&self.world, self.minor) {
                Ok(handle) => self.handle = Some(handle),
                Err(error) => {
                    self.stopped = Some(StopReason::Refused(error.detail));
                    return;
                }
            }
        }
        let seq = self.next_seq;
        let payload = tag_block(seq, WRITER_ID);
        // The handle is held open above; `expect` is the crate's
        // test-support convention.
        let handle = self.handle.as_ref().expect("the handle is open");
        match handle.write(seq, &payload) {
            Ok(ack) => {
                self.next_seq += 1;
                if self.clock_advances < CLOCK_ADVANCE_CAP {
                    self.clock_advances += 1;
                    self.clock.fetch_add(1, Ordering::SeqCst);
                }
                self.acked.push(AckedWrite {
                    seq,
                    ack,
                    index: seq,
                    clock: self.clock.load(Ordering::SeqCst),
                });
            }
            Err(error) => {
                // Writes began failing: this IS the data-path
                // boundary (§2.3) — the suspension or role flip the
                // migration caused. The last acknowledged sequence
                // is the boundary; the refusal is reported honestly.
                self.handle = None;
                self.stopped = Some(StopReason::Refused(error.detail));
            }
        }
    }
}

/// One running continuous writer (§2.2): the spawned task writes
/// while the VM is Running; the handle exposes the acknowledged
/// journal, the stop reason and a deterministic stop.
pub struct WriterHandle {
    inner: Arc<Mutex<GuestWriter>>,
    stop: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
}

impl WriterHandle {
    /// Start the continuous writer for `vm` on `minor` (the source
    /// world's device), advancing the shared frozen clock by one
    /// tick per ack (capped — see the `CLOCK_ADVANCE_CAP` constant).
    pub fn start(
        world: &Arc<Mutex<FakeDrbd>>,
        vmm: &Arc<FakeVmm>,
        vm: &str,
        minor: u32,
        clock: &Arc<std::sync::atomic::AtomicU64>,
    ) -> WriterHandle {
        let inner = Arc::new(Mutex::new(GuestWriter {
            world: Arc::clone(world),
            vmm: Arc::clone(vmm),
            vm_id: vm.to_owned(),
            minor,
            clock: Arc::clone(clock),
            clock_advances: 0,
            acked: Vec::new(),
            ticks: 0,
            next_seq: 1,
            handle: None,
            stopped: None,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let task = {
            let inner = Arc::clone(&inner);
            let stop = Arc::clone(&stop);
            tokio::spawn(async move {
                loop {
                    if stop.load(Ordering::SeqCst) {
                        let mut writer = inner.lock().expect("writer");
                        if writer.stopped.is_none() {
                            writer.stopped = Some(StopReason::StoppedByRig);
                        }
                        break;
                    }
                    {
                        let mut writer = inner.lock().expect("writer");
                        writer.tick();
                        if writer.stopped.is_some() {
                            break;
                        }
                    }
                    tokio::time::sleep(NOTICE_STEP).await;
                }
            })
        };
        WriterHandle { inner, stop, task }
    }

    /// Ask the writer to stop (idempotent); [`WriterHandle::join`]
    /// observes the exit.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    /// Stop and await the writer's exit, returning the frozen
    /// acknowledged journal and the data-path boundary (the last
    /// acknowledged sequence with why the writer's I/O ended —
    /// `None` for both parts only when nothing was ever acked).
    pub async fn join(self) -> (Vec<AckedWrite>, Option<(u64, StopReason)>) {
        self.stop();
        let _ = self.task.await;
        let mut writer = self.inner.lock().expect("writer");
        if writer.stopped.is_none() {
            writer.stopped = Some(StopReason::StoppedByRig);
        }
        let boundary = writer
            .stopped
            .clone()
            .and_then(|reason| writer.acked.last().map(|last| (last.seq, reason)));
        (writer.acked.clone(), boundary)
    }

    /// The acknowledged journal (frozen after [`WriterHandle::join`]).
    #[must_use]
    pub fn acknowledged(&self) -> Vec<AckedWrite> {
        self.inner.lock().expect("writer").acked.clone()
    }

    /// The data-path boundary: the last acknowledged tag sequence,
    /// with why the writer's I/O ended (`None` while still running).
    #[must_use]
    pub fn boundary(&self) -> Option<(u64, StopReason)> {
        let writer = self.inner.lock().expect("writer");
        let reason = writer.stopped.clone()?;
        writer.acked.last().map(|last| (last.seq, reason))
    }
}
