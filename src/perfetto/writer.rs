//! Perfetto trace writer for the small schema subset we use. See `mod.rs`.

use std::collections::HashMap;
use std::io::{self, Write};

use super::proto::*;

/// Field numbers from `protos/perfetto/trace/trace_packet.proto`.
mod tp {
    pub const TIMESTAMP: u32 = 8;
    pub const TRUSTED_PACKET_SEQUENCE_ID: u32 = 10;
    pub const INTERNED_DATA: u32 = 12;
    pub const SEQUENCE_FLAGS: u32 = 13;
    pub const TIMESTAMP_CLOCK_ID: u32 = 58;
    pub const CLOCK_SNAPSHOT: u32 = 36;
    pub const PROCESS_DESCRIPTOR: u32 = 43;
    pub const THREAD_DESCRIPTOR: u32 = 44;
    pub const FIRST_PACKET_ON_SEQUENCE: u32 = 87;
    pub const STREAMING_PROFILE_PACKET: u32 = 54;
}

/// Builtin clock ids from `protos/perfetto/common/builtin_clock.proto`.
const BUILTIN_CLOCK_MONOTONIC: u64 = 3;
const BUILTIN_CLOCK_BOOTTIME: u64 = 6;

/// `sequence_flags` bits.
const SEQ_FLAG_INCREMENTAL_STATE_CLEARED: u64 = 1;
const SEQ_FLAG_NEEDS_INCREMENTAL_STATE: u64 = 2;

/// `Trace.packet` field number.
const FIELD_TRACE_PACKET: u32 = 1;

/// One sample as we want to emit it.
pub struct Sample<'a> {
    pub timestamp_ns: u64,
    /// Frame strings, root-first. The encoder reverses to leaf-first if
    /// Perfetto wants leaf-first ordering for its flame view (it does).
    pub frames_root_first: &'a [String],
}

/// Per-thread state we accumulate, then drain into the output.
pub struct ThreadTrace {
    pub pid: u32,
    pub tid: u32,
    pub thread_name: String,
    /// Per-thread sample list; we own the strings to keep things simple.
    pub samples: Vec<OwnedSample>,
}

pub struct OwnedSample {
    pub timestamp_ns: u64,
    pub frames_root_first: Vec<String>,
}

pub fn write_trace<W: Write>(
    w: &mut W,
    process_pid: u32,
    process_name: &str,
    threads: &[ThreadTrace],
) -> io::Result<()> {
    // ClockSnapshot first: anchor MONOTONIC against BOOTTIME at a known
    // timestamp so Perfetto can convert sample timestamps (which it
    // implicitly attributes to MONOTONIC for stack profile packets) into
    // its trace clock domain. We pick the first sample's timestamp as the
    // anchor; if there are no samples, anchor to zero (Perfetto still
    // wants the snapshot present).
    let anchor_ns = threads
        .iter()
        .flat_map(|t| t.samples.first())
        .map(|s| s.timestamp_ns)
        .min()
        .unwrap_or(0);
    write_clock_snapshot_packet(w, 1, anchor_ns)?;

    // Sequence id 1 is reserved for the process descriptor; threads start
    // at 2 and go up.
    write_process_descriptor_packet(w, 1, process_pid, process_name)?;

    for (idx, t) in threads.iter().enumerate() {
        let sequence_id = 2 + idx as u32;
        write_thread_sequence(w, sequence_id, t)?;
    }
    Ok(())
}

/// Emit a `ClockSnapshot` TracePacket. We map both MONOTONIC and BOOTTIME
/// to the same `anchor_ns` value (we don't have separate clocks on macOS
/// for these -- mach_absolute_time→ns is essentially MONOTONIC, and we
/// treat the same value as BOOTTIME for trace purposes). This satisfies
/// Perfetto's StreamingProfilePacket parser, which complains about clock
/// 3 (MONOTONIC) without a snapshot even when we never explicitly set
/// `timestamp_clock_id` to 3 ourselves.
fn write_clock_snapshot_packet<W: Write>(
    w: &mut W,
    sequence_id: u32,
    anchor_ns: u64,
) -> io::Result<()> {
    write_message(w, FIELD_TRACE_PACKET, |buf| {
        write_uint32(buf, tp::TRUSTED_PACKET_SEQUENCE_ID, sequence_id)?;
        write_message(buf, tp::CLOCK_SNAPSHOT, |snap| {
            // ClockSnapshot.clocks (field 1) -- one Clock per ID.
            write_message(snap, 1, |c| {
                write_uint64(c, 1 /* clock_id */, BUILTIN_CLOCK_BOOTTIME)?;
                write_uint64(c, 2 /* timestamp */, anchor_ns)?;
                Ok(())
            })?;
            write_message(snap, 1, |c| {
                write_uint64(c, 1 /* clock_id */, BUILTIN_CLOCK_MONOTONIC)?;
                write_uint64(c, 2 /* timestamp */, anchor_ns)?;
                Ok(())
            })?;
            // ClockSnapshot.primary_trace_clock (field 2) -- pin trace
            // clock to BOOTTIME so the timestamps on our packets (which
            // we don't tag with timestamp_clock_id, defaulting to BOOTTIME)
            // need no further conversion.
            write_uint64(snap, 2, BUILTIN_CLOCK_BOOTTIME)?;
            Ok(())
        })?;
        Ok(())
    })
}

fn write_process_descriptor_packet<W: Write>(
    w: &mut W,
    sequence_id: u32,
    pid: u32,
    process_name: &str,
) -> io::Result<()> {
    write_message(w, FIELD_TRACE_PACKET, |buf| {
        write_uint32(buf, tp::TRUSTED_PACKET_SEQUENCE_ID, sequence_id)?;
        write_message(buf, tp::PROCESS_DESCRIPTOR, |pd| {
            // `int32 pid = 1`
            write_uint32(pd, 1, pid)?;
            // `string process_name = 6`
            write_string(pd, 6, process_name)?;
            Ok(())
        })?;
        Ok(())
    })
}

fn write_thread_sequence<W: Write>(
    w: &mut W,
    sequence_id: u32,
    thread: &ThreadTrace,
) -> io::Result<()> {
    // First, intern strings, frames, and callstacks for this thread.
    let mut function_names: HashMap<String, u64> = HashMap::new();
    let mut frames: Vec<u64> = Vec::new(); // frame iid -> function_name iid
    let mut frame_idx_by_name: HashMap<String, u64> = HashMap::new(); // we collapse 1 frame == 1 unique function name
    let mut callstack_iids: HashMap<Vec<u64>, u64> = HashMap::new(); // frame_id list -> callstack iid
    let mut callstack_order: Vec<Vec<u64>> = Vec::new();

    let mut next_string_iid: u64 = 1;
    let mut next_frame_iid: u64 = 1;
    let mut next_callstack_iid: u64 = 1;

    let mut sample_callstack_iids: Vec<u64> = Vec::with_capacity(thread.samples.len());

    for sample in &thread.samples {
        // Perfetto's flame graph view wants leaf-first order in the
        // callstack frame list.
        let leaf_first: Vec<&str> = sample
            .frames_root_first
            .iter()
            .rev()
            .map(String::as_str)
            .collect();

        let mut frame_iids: Vec<u64> = Vec::with_capacity(leaf_first.len());
        for name in &leaf_first {
            // intern the function name string
            let _string_iid = *function_names.entry((*name).to_owned()).or_insert_with(|| {
                let iid = next_string_iid;
                next_string_iid += 1;
                iid
            });
            // intern the frame (1:1 with the function name in this minimal
            // encoding; we don't try to disambiguate inlined vs. non-inlined
            // call sites yet)
            let frame_iid = *frame_idx_by_name.entry((*name).to_owned()).or_insert_with(|| {
                let iid = next_frame_iid;
                next_frame_iid += 1;
                frames.push(_string_iid);
                iid
            });
            frame_iids.push(frame_iid);
        }
        let cs_iid = *callstack_iids.entry(frame_iids.clone()).or_insert_with(|| {
            let iid = next_callstack_iid;
            next_callstack_iid += 1;
            callstack_order.push(frame_iids.clone());
            iid
        });
        sample_callstack_iids.push(cs_iid);
    }

    // Build the InternedData payload first; we'll emit it in the same
    // TracePacket as the ThreadDescriptor and the first sample.
    let interned_payload = build_interned_data(
        &function_names,
        &frames,
        &callstack_order,
    )?;

    // Emit the bootstrap TracePacket: thread_descriptor + interned_data,
    // marked `first_packet_on_sequence` and with `INCREMENTAL_STATE_CLEARED`.
    write_message(w, FIELD_TRACE_PACKET, |buf| {
        write_uint64(buf, tp::TIMESTAMP, thread.samples.first().map(|s| s.timestamp_ns).unwrap_or(0))?;
        write_uint32(buf, tp::TRUSTED_PACKET_SEQUENCE_ID, sequence_id)?;
        write_uint64(buf, tp::SEQUENCE_FLAGS, SEQ_FLAG_INCREMENTAL_STATE_CLEARED)?;
        write_uint64(buf, tp::FIRST_PACKET_ON_SEQUENCE, 1)?;
        write_message(buf, tp::THREAD_DESCRIPTOR, |td| {
            // `int32 pid = 1`
            write_uint32(td, 1, thread.pid)?;
            // `int32 tid = 2`
            write_uint32(td, 2, thread.tid)?;
            // `string thread_name = 5`
            write_string(td, 5, &thread.thread_name)?;
            Ok(())
        })?;
        // interned_data goes in the same packet
        write_bytes(buf, tp::INTERNED_DATA, &interned_payload)?;
        Ok(())
    })?;

    // Emit one TracePacket per sample with a single-element
    // StreamingProfilePacket. Each sample packet gets:
    //   * `SEQ_NEEDS_INCREMENTAL_STATE` so Perfetto carries the
    //     InternedData emitted in the bootstrap packet forward to iid
    //     lookups here.
    //   * `timestamp_clock_id = BOOTTIME` explicitly. Without this,
    //     Perfetto's StreamingProfilePacket parser internally references
    //     MONOTONIC (clock 3) when converting timestamps, even when
    //     timestamp_delta_us is zero -- which trips
    //     clock_sync_failure_unknown_source_clock on every sample.
    //     Pinning the clock to BOOTTIME (the trace's primary_trace_clock
    //     from the ClockSnapshot we emit at the start) avoids the
    //     conversion.
    for (sample, &cs_iid) in thread.samples.iter().zip(sample_callstack_iids.iter()) {
        write_message(w, FIELD_TRACE_PACKET, |buf| {
            write_uint64(buf, tp::TIMESTAMP, sample.timestamp_ns)?;
            write_uint64(buf, tp::TIMESTAMP_CLOCK_ID, BUILTIN_CLOCK_BOOTTIME)?;
            write_uint32(buf, tp::TRUSTED_PACKET_SEQUENCE_ID, sequence_id)?;
            write_uint64(buf, tp::SEQUENCE_FLAGS, SEQ_FLAG_NEEDS_INCREMENTAL_STATE)?;
            write_message(buf, tp::STREAMING_PROFILE_PACKET, |sp| {
                write_packed_uint64(sp, 1, &[cs_iid])?;
                write_packed_uint64(sp, 2, &[0])?;
                Ok(())
            })?;
            Ok(())
        })?;
    }

    Ok(())
}

/// `InternedData` field numbers from `protos/perfetto/trace/interned_data/interned_data.proto`.
mod id {
    pub const FUNCTION_NAMES: u32 = 5;
    pub const FRAMES: u32 = 6;
    pub const CALLSTACKS: u32 = 7;
}

/// `Frame` (`protos/perfetto/trace/profiling/profile_common.proto`).
mod frame {
    pub const IID: u32 = 1;
    pub const FUNCTION_NAME_ID: u32 = 2;
}

/// `Callstack`.
mod cs {
    pub const IID: u32 = 1;
    pub const FRAME_IDS: u32 = 2;
}

/// `InternedString`.
mod istr {
    pub const IID: u32 = 1;
    pub const STR: u32 = 2;
}

fn build_interned_data(
    function_names: &HashMap<String, u64>,
    frames: &[u64],
    callstacks: &[Vec<u64>],
) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();

    // `repeated InternedString function_names = 5`
    // Re-emit in iid order so the wire output is deterministic + small.
    let mut by_iid: Vec<(u64, &str)> = function_names
        .iter()
        .map(|(name, &iid)| (iid, name.as_str()))
        .collect();
    by_iid.sort_by_key(|&(iid, _)| iid);
    for (iid, name) in &by_iid {
        write_message(&mut out, id::FUNCTION_NAMES, |s| {
            write_uint64(s, istr::IID, *iid)?;
            write_string(s, istr::STR, name)?;
            Ok(())
        })?;
    }

    // `repeated Frame frames = 6`
    for (idx, &fn_iid) in frames.iter().enumerate() {
        let frame_iid = (idx + 1) as u64;
        write_message(&mut out, id::FRAMES, |f| {
            write_uint64(f, frame::IID, frame_iid)?;
            write_uint64(f, frame::FUNCTION_NAME_ID, fn_iid)?;
            Ok(())
        })?;
    }

    // `repeated Callstack callstacks = 7`
    for (idx, frame_ids) in callstacks.iter().enumerate() {
        let cs_iid = (idx + 1) as u64;
        write_message(&mut out, id::CALLSTACKS, |c| {
            write_uint64(c, cs::IID, cs_iid)?;
            write_packed_uint64(c, cs::FRAME_IDS, frame_ids)?;
            Ok(())
        })?;
    }

    Ok(out)
}
