//! Binary framing for journal records.
//!
//! Frame layout (all integers little-endian):
//!
//! ```text
//! +---------+----------+-------------+--------+------------------+
//! | magic   | sequence | payload_len | crc32  | payload bytes    |
//! | u32     | u64      | u32         | u32    | payload_len      |
//! +---------+----------+-------------+--------+------------------+
//! ```
//!
//! - `magic` identifies the start of a frame and lets replay skip garbage.
//! - `sequence` is the monotonic frame counter starting at 1; a gap or repeat
//!   marks the frame as invalid.
//! - `crc32` is the CRC-32 checksum of the payload bytes as computed by
//!   `crc32fast` (IEEE 802.3 polynomial — the workspace-standard CRC crate;
//!   integrity, not a specific CRC variant, is the contract here).
//! - `payload_len` is bounded by [`MAX_PAYLOAD_LEN`] so a corrupt length
//!   field can never make replay allocate unbounded memory.
//!
//! Decoding is fail-closed: [`decode_at`] returns `None` (stop replay, treat
//! the rest of the file as a torn tail) on any structural violation —
//! incomplete header, bad magic, oversized or truncated length, CRC mismatch
//! or a broken sequence chain. Torn tails are expected after power loss and
//! are truncated away by the journal on open.

use volvisor_types::{ApiError, ApiErrorCode};

use crate::io_err;

/// Frame magic: ASCII `VJL1` ("volvisor journal log, format 1").
pub(crate) const MAGIC: u32 = 0x564A_4C31;

/// Frame header size in bytes: magic + sequence + payload_len + crc.
const HEADER_LEN: usize = 4 + 8 + 4 + 4;

/// Upper bound for a single frame payload; guards against corrupt lengths.
pub(crate) const MAX_PAYLOAD_LEN: usize = 8 * 1024 * 1024;

/// A decoded frame borrowing its payload from the replay buffer.
///
/// `sequence`/`payload` are consumed by the test-only rollback walker and
/// the fuzz-only chain helper; production replay streams via
/// [`read_frame`], so they are allowed to be unread in a default build.
#[derive(Clone, Copy, Debug)]
#[allow(dead_code)]
pub(crate) struct RawFrame<'a> {
    /// Byte offset just past the frame (start of the next candidate frame).
    pub(crate) end: usize,
    /// Monotonic frame sequence number.
    pub(crate) sequence: u64,
    /// Frame payload (a serialized record envelope).
    pub(crate) payload: &'a [u8],
}

/// Encode `payload` as one frame with the given sequence number.
///
/// Errors only if the payload exceeds [`MAX_PAYLOAD_LEN`] (a serialized
/// request envelope that large is a bug, not a runtime condition).
pub(crate) fn encode(sequence: u64, payload: &[u8]) -> Result<Vec<u8>, ApiError> {
    if payload.len() > MAX_PAYLOAD_LEN {
        return Err(ApiError::new(
            ApiErrorCode::Internal,
            format!(
                "journal record payload of {} bytes exceeds the {} byte frame limit",
                payload.len(),
                MAX_PAYLOAD_LEN
            ),
        ));
    }
    let len = u32::try_from(payload.len()).map_err(|_| {
        ApiError::new(
            ApiErrorCode::Internal,
            "journal record payload length overflows u32",
        )
    })?;
    let crc = crc32fast::hash(payload);
    let mut frame = Vec::with_capacity(HEADER_LEN + payload.len());
    frame.extend_from_slice(&MAGIC.to_le_bytes());
    frame.extend_from_slice(&sequence.to_le_bytes());
    frame.extend_from_slice(&len.to_le_bytes());
    frame.extend_from_slice(&crc.to_le_bytes());
    frame.extend_from_slice(payload);
    Ok(frame)
}

/// Try to decode the frame starting at `offset`, requiring `expected_sequence`.
///
/// Returns `None` when there is no valid frame at `offset` — because the
/// buffer is fully consumed, or because the bytes there are torn or corrupt.
/// Replay must stop at that point; the journal truncates to `offset`.
pub(crate) fn decode_at(
    data: &[u8],
    offset: usize,
    expected_sequence: u64,
) -> Option<RawFrame<'_>> {
    let rest = data.get(offset..)?;
    if rest.len() < HEADER_LEN {
        return None; // torn: incomplete header
    }
    let (header, body) = rest.split_at(HEADER_LEN);
    let magic = le_u32(header, 0)?;
    if magic != MAGIC {
        return None; // garbage or torn: not a frame boundary
    }
    let sequence = le_u64(header, 4)?;
    let len = le_u32(header, 12)?;
    let crc = le_u32(header, 16)?;
    let payload_len = usize::try_from(len).ok()?;
    let payload = body.get(..payload_len)?;
    if payload_len > MAX_PAYLOAD_LEN || crc32fast::hash(payload) != crc {
        return None; // corrupt: oversized length or CRC mismatch
    }
    if sequence != expected_sequence {
        return None; // broken monotonic chain
    }
    Some(RawFrame {
        end: offset + HEADER_LEN + payload.len(),
        sequence,
        payload,
    })
}

fn le_u32(bytes: &[u8], at: usize) -> Option<u32> {
    let raw: [u8; 4] = bytes.get(at..at + 4)?.try_into().ok()?;
    Some(u32::from_le_bytes(raw))
}

fn le_u64(bytes: &[u8], at: usize) -> Option<u64> {
    let raw: [u8; 8] = bytes.get(at..at + 8)?.try_into().ok()?;
    Some(u64::from_le_bytes(raw))
}

/// One frame read incrementally from a log stream (issue #64: replay no
/// longer buffers the whole log in memory).
#[derive(Debug)]
pub(crate) struct StreamFrame {
    /// Monotonic frame sequence number.
    pub(crate) sequence: u64,
    /// Frame payload (a serialized record envelope).
    pub(crate) payload: Vec<u8>,
    /// Total bytes consumed by this frame (header plus payload).
    pub(crate) frame_len: usize,
}

/// Read the next frame from `reader`, requiring `expected_sequence`.
///
/// Returns `Ok(None)` at a clean end of file **or** at the first torn/corrupt
/// frame; either way replay stops and truncates to the last good offset.
/// Returns `Err` only for a real I/O failure, which must never be mistaken for
/// a torn tail. Memory is bounded by [`MAX_PAYLOAD_LEN`] (one frame).
pub(crate) fn read_frame<R: std::io::Read>(
    reader: &mut R,
    expected_sequence: u64,
) -> Result<Option<StreamFrame>, ApiError> {
    let mut header = [0u8; HEADER_LEN];
    if !read_exact_or_torn(reader, &mut header)
        .map_err(io_err("failed to read journal frame header"))?
    {
        return Ok(None);
    }
    let Some(magic) = le_u32(&header, 0) else {
        return Ok(None);
    };
    if magic != MAGIC {
        return Ok(None);
    }
    let Some(sequence) = le_u64(&header, 4) else {
        return Ok(None);
    };
    if sequence != expected_sequence {
        return Ok(None);
    }
    let (Some(len), Some(crc)) = (le_u32(&header, 12), le_u32(&header, 16)) else {
        return Ok(None);
    };
    let Ok(payload_len) = usize::try_from(len) else {
        return Ok(None);
    };
    if payload_len > MAX_PAYLOAD_LEN {
        return Ok(None);
    }
    let mut payload = vec![0u8; payload_len];
    if !read_exact_or_torn(reader, &mut payload)
        .map_err(io_err("failed to read journal frame payload"))?
    {
        return Ok(None);
    }
    if crc32fast::hash(&payload) != crc {
        return Ok(None);
    }
    Ok(Some(StreamFrame {
        sequence,
        payload,
        frame_len: HEADER_LEN + payload_len,
    }))
}

/// Read exactly `buf.len()` bytes; `Ok(false)` means the stream ended before
/// the buffer was filled (a clean EOF at a boundary or a torn tail).
fn read_exact_or_torn(reader: &mut impl std::io::Read, buf: &mut [u8]) -> std::io::Result<bool> {
    match reader.read_exact(buf) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => Ok(false),
        Err(err) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_single_frame() {
        let frame = encode(1, b"payload-bytes").expect("encode");
        let decoded = decode_at(&frame, 0, 1).expect("decode");
        assert_eq!(decoded.sequence, 1);
        assert_eq!(decoded.payload, b"payload-bytes");
        assert_eq!(decoded.end, frame.len());
    }

    #[test]
    fn round_trip_chained_frames() {
        let first = encode(1, b"one").expect("encode");
        let second = encode(2, b"two").expect("encode");
        let mut data = first.clone();
        data.extend_from_slice(&second);

        let f1 = decode_at(&data, 0, 1).expect("first frame");
        assert_eq!(f1.payload, b"one");
        let f2 = decode_at(&data, f1.end, 2).expect("second frame");
        assert_eq!(f2.payload, b"two");
        // Clean EOF after the last frame.
        assert!(decode_at(&data, f2.end, 3).is_none());
    }

    #[test]
    fn empty_payload_round_trips() {
        let frame = encode(3, b"").expect("encode");
        let decoded = decode_at(&frame, 0, 3).expect("decode");
        assert_eq!(decoded.payload, b"");
        assert_eq!(decoded.end, HEADER_LEN);
    }

    #[test]
    fn stops_on_truncated_header() {
        let frame = encode(1, b"payload").expect("encode");
        let cut = frame.len() - 1;
        assert!(decode_at(&frame[..cut], 0, 1).is_none());
        assert!(decode_at(&frame[..4], 0, 1).is_none());
    }

    #[test]
    fn stops_on_truncated_payload() {
        let frame = encode(1, b"payload").expect("encode");
        let cut = frame.len() - 2;
        assert!(decode_at(&frame[..cut], 0, 1).is_none());
    }

    #[test]
    fn stops_on_bad_magic() {
        let frame = encode(1, b"payload").expect("encode");
        let mut corrupt = frame;
        corrupt[0] ^= 0xFF;
        assert!(decode_at(&corrupt, 0, 1).is_none());
    }

    #[test]
    fn stops_on_bad_crc() {
        let frame = encode(1, b"payload").expect("encode");
        let mut corrupt = frame;
        let last = corrupt.len() - 1;
        corrupt[last] ^= 0xFF;
        assert!(decode_at(&corrupt, 0, 1).is_none());
    }

    #[test]
    fn stops_on_sequence_gap_or_repeat() {
        let frame = encode(5, b"payload").expect("encode");
        assert!(decode_at(&frame, 0, 4).is_none());
        assert!(decode_at(&frame, 0, 6).is_none());
        assert!(decode_at(&frame, 0, 5).is_some());
    }

    #[test]
    fn stops_on_oversized_length() {
        let mut frame = encode(1, b"p").expect("encode");
        // Overwrite the payload_len field with a huge value.
        frame[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(decode_at(&frame, 0, 1).is_none());
    }

    #[test]
    fn encode_rejects_oversized_payload() {
        let payload = vec![0u8; MAX_PAYLOAD_LEN + 1];
        assert!(encode(1, &payload).is_err());
    }

    #[test]
    fn decode_after_garbage_prefix_fails() {
        // Garbage before any valid frame is indistinguishable from a torn
        // tail: replay must stop at offset 0.
        let garbage = b"not a journal, just debris from a crash";
        assert!(decode_at(garbage, 0, 1).is_none());
    }
}
