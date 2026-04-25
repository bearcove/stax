//! Hand-rolled Perfetto trace writer. We emit the small subset of
//! Perfetto's `trace.proto` schema needed for sampled CPU profiles:
//!
//! * `Trace`              -- a flat sequence of `TracePacket`s
//! * `ProcessDescriptor`  -- gives the trace a process name in the UI
//! * `ThreadDescriptor`   -- one per recorded thread; samples are
//!                           attributed to the most-recent ThreadDescriptor
//!                           on the same packet sequence
//! * `InternedData`       -- function names + frames + callstacks, indexed
//!                           by `iid` for compact reuse from samples
//! * `StreamingProfilePacket` -- per-sample callstack + timestamp
//!
//! Schema reference:
//! <https://github.com/google/perfetto/tree/main/protos/perfetto/trace>
//!
//! Future extensions (PMC counter tracks, signposts, async slices) plug
//! into the same TracePacket envelope; the small encoder here is
//! intentionally flat-and-simple so adding them is cheap.

pub mod proto;
pub mod writer;
