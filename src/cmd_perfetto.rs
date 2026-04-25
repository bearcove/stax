//! `nperf perfetto <archive> -o trace.perfetto-trace`
//!
//! Walks an nperf archive's samples and emits a Perfetto-protobuf trace
//! that loads in <https://ui.perfetto.dev/>. The output is sized roughly
//! 2-3x our input archive (we duplicate function-name strings per
//! thread sequence today; that's fine for v1, can be deduplicated later).

use std::collections::HashMap;
use std::error::Error;
use std::fs::File;
use std::io::{self, BufWriter};

use crate::args::PerfettoArgs;
use crate::data_reader::{
    read_data, repack_cli_args, DecodeOpts, EventKind, FrameKind,
};
use crate::interner::StringInterner;
use crate::perfetto::writer::{write_trace, OwnedSample, ThreadTrace};

pub fn main(args: PerfettoArgs) -> Result<(), Box<dyn Error>> {
    let (omit_regex, read_data_args) = repack_cli_args(&args.collation_args);
    let opts = DecodeOpts {
        omit_regex,
        // Perfetto has explicit thread/process tracks, so we don't want
        // synthetic process/thread frames cluttering the stack -- the
        // frame stack should be the *call stack only*.
        emit_kernel_frames: false,
        emit_thread_frames: false,
        emit_process_frames: false,
        granularity: crate::args::Granularity::Function,
    };

    let mut interner = StringInterner::new();
    let mut threads_by_tid: HashMap<u32, ThreadTrace> = HashMap::new();
    let mut process_pid: Option<u32> = None;
    let mut process_name: Option<String> = None;

    let _state = read_data(read_data_args, |event| {
        match event.kind {
            EventKind::Sample(ref sample) => {
                if process_pid.is_none() {
                    process_pid = Some(sample.process.pid());
                    process_name = Some(sample.process.executable().to_owned());
                }

                let state = event.state;
                let frames = match sample.decode(state, &opts, &mut interner) {
                    Some(f) => f,
                    None => return,
                };

                let names: Vec<String> = frames
                    .iter()
                    .filter_map(|f| frame_name(state, &interner, f))
                    .collect();
                if names.is_empty() {
                    return;
                }

                let pid = sample.process.pid();
                let tid = sample.tid;
                let timestamp = sample.timestamp;
                let thread = threads_by_tid.entry(tid).or_insert_with(|| ThreadTrace {
                    pid,
                    tid,
                    thread_name: state
                        .get_thread_name(tid)
                        .map(|s| s.to_owned())
                        .unwrap_or_else(|| format!("tid {}", tid)),
                    samples: Vec::new(),
                });
                thread.samples.push(OwnedSample {
                    timestamp_ns: timestamp,
                    frames_root_first: names,
                });
            }
            _ => {}
        }
    })?;

    let mut threads: Vec<ThreadTrace> = threads_by_tid.into_values().collect();
    threads.sort_by_key(|t| t.tid);

    if threads.iter().all(|t| t.samples.is_empty()) {
        return Err("no samples in archive; nothing to emit".into());
    }

    let out_path = &args.output;
    let f = File::create(out_path)?;
    let mut w = BufWriter::new(f);

    write_trace(
        &mut w,
        process_pid.unwrap_or(0),
        process_name.as_deref().unwrap_or(""),
        &threads,
    )?;

    use io::Write;
    w.flush()?;

    let total_samples: usize = threads.iter().map(|t| t.samples.len()).sum();
    info!(
        "Wrote Perfetto trace to {} ({} samples across {} threads)",
        std::path::Path::new(out_path).display(),
        total_samples,
        threads.len()
    );
    Ok(())
}

/// Extract a printable function name from a `FrameKind`. Returns `None`
/// for kinds we don't want in the output (synthetic process/thread
/// frames -- those are emitted as Perfetto descriptors instead).
fn frame_name(
    state: &crate::data_reader::State,
    interner: &StringInterner,
    frame: &FrameKind,
) -> Option<String> {
    match *frame {
        FrameKind::Process(_) | FrameKind::Thread(_) | FrameKind::MainThread => None,
        FrameKind::User(addr) => Some(format!("0x{:x}", addr)),
        FrameKind::UserBinary(ref binary_id, addr) => {
            let binary = state.get_binary(binary_id);
            Some(format!("0x{:x} [{}]", addr, binary.basename()))
        }
        FrameKind::UserByAddress { ref binary_id, symbol, .. }
        | FrameKind::UserByFunction { ref binary_id, symbol, .. }
        | FrameKind::UserByLine { ref binary_id, symbol, .. } => {
            let name = interner.resolve(symbol)?;
            let bin = state.get_binary(binary_id).basename();
            Some(format!("{} [{}]", name, bin))
        }
        FrameKind::UserByFunctionJit { symbol } => {
            let name = interner.resolve(symbol)?;
            Some(format!("{} [JIT]", name))
        }
        // Kernel frames shouldn't appear here -- DecodeOpts.emit_kernel_frames
        // is set to false. If they do leak through, surface as opaque text.
        FrameKind::Kernel(addr) => Some(format!("0x{:x} [kernel]", addr)),
        FrameKind::KernelSymbol(_) => {
            let _ = state;
            Some("kernel:?".to_owned())
        }
    }
}
