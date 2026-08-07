use facet::Facet;
use figue as args;

pub enum TargetProcess {
    ByPid(u32),
    Launch { program: String, args: Vec<String> },
}

/// A recording-time window, shared by the query subcommands. Parsed
/// once here so `flame`, `top`, and `threads` accept identical syntax
/// and produce identical ranges.
///
/// Forms (`D` is a duration like `500ms`, `30s`, `1m`, or a bare
/// number of seconds):
///
/// - `--window D`      → the last D of the recording
/// - `--window A..B`   → the absolute slice [A, B) from recording start
/// - `--window A..`    → from A to the end
/// - `--window ..B`    → from the start to B
#[derive(Facet, Debug, Default)]
pub struct WindowArgs {
    /// Restrict the query to a time window within the recording.
    /// `500ms`, `30s`, `1m` select the trailing window; `A..B`,
    /// `A..`, and `..B` select an absolute slice relative to
    /// recording start.
    #[facet(args::named, default)]
    pub window: Option<String>,
}

impl WindowArgs {
    /// Resolve the window string into recording-relative `[start_ns,
    /// end_ns)` bounds. `end_ns` is the recording's duration; an open
    /// end returns `None` so the server keeps "to the latest event."
    ///
    /// Errors with a precise message rather than guessing: a window
    /// that is silently dropped reads as "the filter didn't work."
    pub fn resolve(
        &self,
        recording_duration_ns: u64,
    ) -> Result<Option<(u64, Option<u64>)>, String> {
        let Some(spec) = self.window.as_deref() else {
            return Ok(None);
        };
        let spec = spec.trim();
        if spec.is_empty() {
            return Ok(None);
        }
        if let Some((a, b)) = spec.split_once("..") {
            let start = if a.is_empty() { 0 } else { parse_duration(a)? };
            let end = if b.is_empty() {
                None
            } else {
                Some(parse_duration(b)?)
            };
            if let Some(end_ns) = end {
                if end_ns <= start {
                    return Err(format!("window end must be after start (got {spec:?})"));
                }
            }
            return Ok(Some((start, end)));
        }
        let dur = parse_duration(spec)?;
        let start = recording_duration_ns.saturating_sub(dur);
        Ok(Some((start, None)))
    }
}

/// Parse `500ms`, `30s`, `1m`, or a bare number (seconds) into ns.
/// Public so the CLI can resolve marker-window bounds with identical
/// units instead of carrying a second parser that would drift.
pub fn parse_duration(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let (digits, mult) = if let Some(d) = s.strip_suffix("ms") {
        (d, 1_000_000u64)
    } else if let Some(d) = s.strip_suffix('s') {
        (d, 1_000_000_000u64)
    } else if let Some(d) = s.strip_suffix('m') {
        (d, 60 * 1_000_000_000u64)
    } else {
        (s, 1_000_000_000u64)
    };
    let value: f64 = digits
        .trim()
        .parse()
        .map_err(|_| format!("invalid duration {s:?} (expected e.g. 500ms, 30s, 1m)"))?;
    if value < 0.0 {
        return Err(format!("duration must be non-negative (got {s:?})"));
    }
    Ok((value * mult as f64) as u64)
}

/// stax — live profiler frontend for CPU stacks, off-CPU waits, and
/// cooperating target spans, streamed through stax-server.
#[derive(Facet, Debug)]
pub struct Cli {
    #[facet(args::subcommand)]
    pub command: Command,

    #[facet(flatten)]
    pub builtins: args::FigueBuiltins,
}

#[derive(Facet, Debug)]
#[repr(u8)]
pub enum Command {
    /// Record CPU stacks, off-CPU waits, and cooperating target spans.
    /// Forwards events to the running `stax-server` for the web UI and
    /// `stax {top,annotate,…}` to query.
    Record(RecordArgs),

    /// Codesign this stax binary (or, when run as root, install staxd
    /// as a LaunchDaemon).
    Setup(SetupArgs),

    /// Print the current state of stax-server (active run + history).
    Status,

    /// List every run stax-server has hosted (active + history).
    List,

    /// Dump stax-server diagnostics, including target-span ingest,
    /// origin-link reasons, and PET origin-distance counters.
    Diagnose(DiagnoseArgs),

    /// Ask running stax processes to dump SIGUSR1 telemetry/debug
    /// snapshots into unified logging.
    Dump,

    /// Block until a condition fires, the active run stops, or the
    /// timeout elapses.
    Wait(WaitArgs),

    /// Ask stax-server to stop the active run cleanly.
    Stop,

    /// Save the current or most recent queryable run to a v2 archive.
    /// Paths ending in .stax create a single-file package; other paths create
    /// a directory with aggregate chunks, blobs, and an events.jsonl replay stream.
    Save(SaveArgs),

    /// Open a saved run archive into stax-server's query state.
    /// V2 archives replay their saved event stream when present.
    Open(OpenArgs),

    /// Restore a stopped in-memory run into stax-server's query state.
    SelectRun(SelectRunArgs),

    /// Compare two saved run archives without touching stax-server state.
    /// V2 archives replay their saved event stream when present.
    Compare(CompareArgs),

    /// Snapshot top functions or target-span names from the current query state.
    /// Output includes active time, target-executor time, PET samples,
    /// and target span counts.
    Top(TopArgs),

    /// Disassemble + annotate a function from the current query state.
    Annotate(AnnotateArgs),

    /// Print the current flamegraph as an indented tree, with
    /// target-executor time/spans broken out per node.
    Flame(FlameArgs),

    /// List current real threads and synthetic target lanes with CPU/target/off-CPU breakdown.
    Threads(ThreadsArgs),

    /// Inspect cooperating target lanes and span/shader rankings.
    Target(TargetArgs),

    /// Drop a named marker into the active run at the current
    /// recording time. For stall forensics: `stax mark freeze` when a
    /// stall is observed, then `stax flame --window freeze..` reads
    /// what the process was doing from that moment.
    Mark(MarkArgs),

    /// Find whole-process work gaps: wall-time bins where on-CPU
    /// throughput collapses below a fraction of the run's median.
    /// The "it stalled a whole second, just show me that" command --
    /// no manual markers needed. Ranks detected stalls by duration and
    /// prints a ready-to-paste `flame --window` for each.
    Stalls(StallsArgs),
    /// List application signposts reported on the recording monotonic clock.
    Events(EventsArgs),
    /// Summarize or print numeric application counter samples.
    Counters(CountersArgs),
    /// List declarative application latency/liveness contracts.
    Contracts(ContractsArgs),
    /// List derived contract violations.
    Violations(ViolationsArgs),
    /// Print scheduler/PET/application evidence joined to one violation.
    Incident(IncidentArgs),
}

#[derive(Facet, Debug)]
pub struct SaveArgs {
    /// Directory archive to create, or .stax package file to write.
    #[facet(args::positional)]
    pub path: String,
}

#[derive(Facet, Debug)]
pub struct OpenArgs {
    /// Archive directory, .stax package, v2 manifest.json, or legacy v1 archive.json file.
    #[facet(args::positional)]
    pub path: String,
}

#[derive(Facet, Debug)]
pub struct CompareArgs {
    /// Print a machine-readable facet-json report instead of the human table.
    #[facet(args::named, default)]
    pub json: bool,

    /// Fail if candidate total active time increases by more than this many ms.
    #[facet(args::named, default)]
    pub fail_active_delta_ms: Option<f64>,

    /// Fail if candidate target time increases by more than this many ms.
    #[facet(args::named, default)]
    pub fail_target_delta_ms: Option<f64>,

    /// Fail if candidate off-CPU time increases by more than this many ms.
    #[facet(args::named, default)]
    pub fail_off_cpu_delta_ms: Option<f64>,

    /// Fail if candidate target time increases by more than this percent
    /// relative to the baseline.
    #[facet(args::named, default)]
    pub fail_target_delta_pct: Option<f64>,

    /// Fail if candidate unlinked-origin count increases by more than this.
    #[facet(args::named, default)]
    pub fail_unlinked_origins_delta: Option<u64>,

    /// Fail if candidate missing-origin count increases by more than this.
    #[facet(args::named, default)]
    pub fail_missing_origins_delta: Option<u64>,

    /// Fail if candidate bad-duration drop count increases by more than this.
    #[facet(args::named, default)]
    pub fail_bad_duration_drops_delta: Option<u64>,

    /// Fail if candidate target-side queue-drop count increases by more than this.
    #[facet(args::named, default)]
    pub fail_target_queue_drops_delta: Option<u64>,

    /// Fail if candidate worker-disconnect drop count increases by more than this.
    #[facet(args::named, default)]
    pub fail_worker_disconnect_drops_delta: Option<u64>,

    /// Baseline archive directory, .stax package, v2 manifest.json, or legacy v1 archive.json.
    #[facet(args::positional)]
    pub baseline: String,

    /// Candidate archive directory, .stax package, v2 manifest.json, or legacy v1 archive.json.
    #[facet(args::positional)]
    pub candidate: String,
}

#[derive(Facet, Debug)]
pub struct SelectRunArgs {
    /// Run id from `stax list`.
    #[facet(args::positional)]
    pub run_id: u64,
}

#[derive(Facet, Debug)]
pub struct DiagnoseArgs {
    /// Query a run from `stax list` without changing the selected query state.
    #[facet(args::named, default)]
    pub run: Option<u64>,
}

#[derive(Facet, Debug)]
pub struct WaitArgs {
    /// Stop waiting after at least this many PET samples have landed.
    /// Mutually exclusive with --for-seconds and --until-symbol.
    #[facet(args::named, default)]
    pub for_samples: Option<u64>,

    /// Stop waiting after this many seconds, even if the run is still
    /// recording. Mutually exclusive with --for-samples and
    /// --until-symbol.
    #[facet(args::named, default)]
    pub for_seconds: Option<u64>,

    /// Stop waiting once a symbol containing this substring has been
    /// observed (case-sensitive). Mutually exclusive with the others.
    #[facet(args::named, default)]
    pub until_symbol: Option<String>,

    /// Hard deadline for the whole wait, in milliseconds. Returns
    /// `TimedOut` if exceeded.
    #[facet(args::named, default)]
    pub timeout_ms: Option<u64>,
}

#[derive(Facet, Debug)]
pub struct TopArgs {
    /// Query a run from `stax list` without changing the selected query state.
    #[facet(args::named, default)]
    pub run: Option<u64>,

    /// Maximum number of function/span-name entries to return.
    #[facet(args::named, args::short = 'n', default = 20)]
    pub limit: u32,

    /// Sort by `self` (leaf) or `total` (any frame). Target lanes are
    /// parallel execution lanes; origins are provenance links, not CPU
    /// execution containment.
    #[facet(args::named, default = "self")]
    pub sort: String,

    /// Filter to one thread by tid. For CPU tids, target spans whose
    /// origins link to that tid are included as parallel lane work.
    /// Default: all threads.
    #[facet(args::named, default)]
    pub tid: Option<u32>,

    #[facet(flatten, default)]
    pub window: WindowArgs,
}

#[derive(Facet, Debug)]
pub struct ThreadsArgs {
    /// Query a run from `stax list` without changing the selected query state.
    #[facet(args::named, default)]
    pub run: Option<u64>,

    /// Maximum number of threads/lanes to print, sorted by total
    /// activity; target lanes with spans are always included. 0 to
    /// print all.
    #[facet(args::named, args::short = 'n', default = 20)]
    pub limit: u32,
}

#[derive(Facet, Debug)]
pub struct TargetArgs {
    #[facet(args::subcommand)]
    pub command: TargetCommand,
}

#[derive(Facet, Debug)]
#[repr(u8)]
pub enum TargetCommand {
    /// List cooperating target lanes with exact target time and span counts.
    Lanes(TargetLanesArgs),

    /// Rank target span/shader names by exact target duration or invocation count.
    Top(TargetTopArgs),
}

#[derive(Facet, Debug)]
pub struct TargetLanesArgs {
    /// Query a run from `stax list` without changing the selected query state.
    #[facet(args::named, default)]
    pub run: Option<u64>,

    /// Maximum number of target lanes to print. 0 to print all.
    #[facet(args::named, args::short = 'n', default = 20)]
    pub limit: u32,
}

#[derive(Facet, Debug)]
pub struct TargetTopArgs {
    /// Query a run from `stax list` without changing the selected query state.
    #[facet(args::named, default)]
    pub run: Option<u64>,

    /// Maximum number of target span/shader rows to print. 0 to print all.
    #[facet(args::named, args::short = 'n', default = 20)]
    pub limit: u32,

    /// Rank by `time`, `count`, `avg`, or `max`.
    #[facet(args::named, default = "time")]
    pub by: String,

    /// Filter to one target lane tid, or to target spans linked to one CPU tid.
    #[facet(args::named, default)]
    pub tid: Option<u32>,
}

#[derive(Facet, Debug)]
pub struct FlameArgs {
    /// Query a run from `stax list` without changing the selected query state.
    #[facet(args::named, default)]
    pub run: Option<u64>,

    /// Maximum tree depth to print. The flamegraph the server returns
    /// is unbounded; this just controls how deep the CLI prints
    /// (children below the cut-off are summarised as `…<N more
    /// frames>`). Cooperating target lanes render as `lane -> span`.
    /// Origins link those spans back to the CPU stack that queued them.
    #[facet(args::named, args::short = 'd', default = 12)]
    pub max_depth: usize,

    /// Hide nodes whose share of total active time falls
    /// below this percent. `0` to print everything.
    #[facet(args::named, default = 1.0)]
    pub threshold_pct: f64,

    /// Filter to one thread by tid. For CPU tids, target spans whose
    /// origins link to that tid are included as parallel lane work.
    /// Default: all threads.
    #[facet(args::named, default)]
    pub tid: Option<u32>,

    /// Drive the flame from off-CPU time instead of on-CPU time. Use
    /// this to find where threads *parked* rather than where they ran:
    /// the tree is laid out by off-CPU duration and each node reports
    /// its dominant blocking reason (lock, sleep, io, …). The answer
    /// to "what was this thread stuck on during the freeze?", which an
    /// on-CPU flame cannot give (a parked thread yields ~no samples).
    #[facet(args::named, default)]
    pub off_cpu: bool,

    #[facet(flatten, default)]
    pub window: WindowArgs,
}

#[derive(Facet, Debug)]
pub struct AnnotateArgs {
    /// Function to annotate. Either a hex address (`0x10004ad60`)
    /// or a substring of the demangled symbol name; the substring
    /// is matched against the current run's top-N leaf samples and
    /// the hottest match wins. Operates on the server's current query state,
    /// or on a stopped in-memory run selected with `--run`.
    #[facet(args::positional)]
    pub target: String,

    /// Query a run from `stax list` without changing the selected query state.
    #[facet(args::named, default)]
    pub run: Option<u64>,

    /// Filter to one thread by tid. Default: all threads.
    #[facet(args::named, default)]
    pub tid: Option<u32>,
}

#[derive(Facet, Debug)]
pub struct RecordArgs {
    /// PET sampling frequency, in Hz.
    #[facet(args::named, args::short = 'F', default = 900)]
    pub frequency: u32,

    /// Stop sampling after this many seconds. Unlimited by default
    /// (Ctrl-C to stop).
    #[facet(args::named, args::short = 'l', default)]
    pub time_limit: Option<u64>,

    /// Profile an existing process by PID instead of launching one.
    #[facet(args::named, args::short = 'p', default)]
    pub pid: Option<u32>,

    /// Local socket path of the running `staxd` daemon. Defaults to the
    /// path `sudo stax setup` installs.
    #[facet(args::named, default = "/var/run/staxd.sock")]
    pub daemon_socket: String,

    /// Disable `.eh_frame` DWARF unwinding of user stacks.
    ///
    /// On x86_64 Linux, stax replays user stacks from `.eh_frame` CFI
    /// by default, because the system libc is built
    /// `-fomit-frame-pointer` — so the kernel's frame-pointer
    /// `CALLCHAIN` truncates for any sample landing in libc. Pass this
    /// to fall back to the kernel walk, saving a per-sample register +
    /// 8 KiB stack copy. No effect on macOS (kperf already walks full
    /// stacks) or aarch64 (frame-pointer ABI). `STAX_DWARF_UNWIND=0`
    /// in the environment does the same thing.
    #[facet(args::named, default)]
    pub no_dwarf_unwind: bool,

    /// Command to launch and profile. Use `--` to keep the target's
    /// flags from being interpreted by stax:
    ///
    ///     stax record -- /bin/foo --some-flag bar baz
    #[facet(args::positional, default)]
    pub command: Vec<String>,
}

impl RecordArgs {
    pub fn target(&self) -> Result<TargetProcess, String> {
        match (self.pid, self.command.split_first()) {
            (Some(_), Some(_)) => {
                Err("specify either --pid or a command to launch, not both".to_owned())
            }
            (Some(pid), None) => Ok(TargetProcess::ByPid(pid)),
            (None, Some((program, rest))) => Ok(TargetProcess::Launch {
                program: program.clone(),
                args: rest.to_vec(),
            }),
            (None, None) => Err("specify either --pid <PID> or a command to launch".to_owned()),
        }
    }
}

#[derive(Facet, Debug)]
pub struct SetupArgs {
    /// Skip the confirmation prompt before running `codesign`.
    #[facet(args::named, args::short = 'y', default)]
    pub yes: bool,

    /// Linux only, with `sudo`: also install `stax-server` as a root
    /// systemd service on `/run/stax-server-root.sock`, for profiling
    /// root-owned targets (a display/compositor, a daemon) that the
    /// per-user server cannot read `/proc/<pid>/maps` for. Off by
    /// default; the per-user server covers ordinary user targets.
    #[facet(args::named, default)]
    pub server_root: bool,
}

#[derive(Facet, Debug)]
pub struct MarkArgs {
    /// Marker label, e.g. `freeze`. Referenced later as a window
    /// anchor: `--window freeze..` starts at this marker.
    #[facet(args::positional)]
    pub label: String,
}

#[derive(Facet, Debug)]
pub struct StallsArgs {
    /// Query a run from `stax list` without changing the selected query state.
    #[facet(args::named, default)]
    pub run: Option<u64>,

    /// Minimum stall duration to report. Shorter gaps are ignored;
    /// use this to filter noise on a long recording.
    #[facet(args::named, default = 100_000_000)]
    pub min_duration_ns: u64,

    /// Flag a bin as "stalled" when its on-CPU ns falls below this
    /// fraction of the run's median bin. 0.2 = a bin must be under
    /// 20% of typical throughput to count.
    #[facet(args::named, default = 0.2)]
    pub threshold: f64,

    /// Maximum number of stalls to print, ranked by duration.
    #[facet(args::named, args::short = 'n', default = 10)]
    pub limit: usize,

    /// Re-bucket only this window at a caller-chosen resolution instead of
    /// the run-wide adaptive bucket. Use with `--bucket-ns` to catch
    /// sub-second stalls that the coarse default buckets alias away.
    /// Accepts the same specs as `flame --window` (durations, `@video`
    /// timestamps, or marker labels).
    #[facet(flatten, default)]
    pub window: WindowArgs,

    /// Override the timeline bucket width in ns (e.g. 10_000_000 = 10 ms).
    /// Without this, the server sizes buckets to ~200 per run (min 50 ms),
    /// which is too coarse to resolve sub-second stalls on a long run.
    #[facet(args::named, default)]
    pub bucket_ns: Option<u64>,
}

#[derive(Facet, Debug)]
pub struct EventsArgs {
    #[facet(args::named, default)]
    pub run: Option<u64>,
    #[facet(args::named, default)]
    pub tid: Option<u32>,
    #[facet(flatten, default)]
    pub window: WindowArgs,
    #[facet(args::named, default)]
    pub name: Option<String>,
    #[facet(args::named, args::short = 'n', default = 100)]
    pub limit: u32,
    #[facet(args::named, default)]
    pub json: bool,
}

#[derive(Facet, Debug)]
pub struct CountersArgs {
    #[facet(args::named, default)]
    pub run: Option<u64>,
    #[facet(flatten, default)]
    pub window: WindowArgs,
    #[facet(args::named, default)]
    pub name: Option<String>,
    #[facet(args::named, default)]
    pub samples: bool,
    #[facet(args::named, args::short = 'n', default = 100)]
    pub limit: u32,
    #[facet(args::named, default)]
    pub json: bool,
}

#[derive(Facet, Debug)]
pub struct ContractsArgs {
    #[facet(args::named, default)]
    pub run: Option<u64>,
    #[facet(args::named, default)]
    pub json: bool,
}

#[derive(Facet, Debug)]
pub struct ViolationsArgs {
    #[facet(args::named, default)]
    pub run: Option<u64>,
    #[facet(flatten, default)]
    pub window: WindowArgs,
    #[facet(args::named, default)]
    pub severity: Option<String>,
    #[facet(args::named, args::short = 'n', default = 100)]
    pub limit: u32,
    #[facet(args::named, default)]
    pub json: bool,
}

#[derive(Facet, Debug)]
pub struct IncidentArgs {
    #[facet(args::positional)]
    pub violation_id: u64,
    #[facet(args::named, default)]
    pub run: Option<u64>,
    #[facet(args::named, default)]
    pub margin_ms: Option<u64>,
    #[facet(args::named, default)]
    pub json: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(spec: &str) -> WindowArgs {
        WindowArgs {
            window: Some(spec.to_owned()),
        }
    }

    #[test]
    fn trailing_window_anchors_to_end() {
        // 60s recording; `30s` selects [30s, end).
        let (start, end) = window("30s").resolve(60_000_000_000).unwrap().unwrap();
        assert_eq!(start, 30_000_000_000);
        assert_eq!(end, None);
    }

    #[test]
    fn absolute_slice() {
        let (start, end) = window("10s..20s").resolve(60_000_000_000).unwrap().unwrap();
        assert_eq!(start, 10_000_000_000);
        assert_eq!(end, Some(20_000_000_000));
    }

    #[test]
    fn open_ended_and_leading() {
        let (start, end) = window("5s..").resolve(60_000_000_000).unwrap().unwrap();
        assert_eq!((start, end), (5_000_000_000, None));
        let (start, end) = window("..5s").resolve(60_000_000_000).unwrap().unwrap();
        assert_eq!((start, end), (0, Some(5_000_000_000)));
    }

    #[test]
    fn duration_units() {
        assert_eq!(parse_duration("500ms").unwrap(), 500_000_000);
        assert_eq!(parse_duration("30s").unwrap(), 30_000_000_000);
        assert_eq!(parse_duration("1m").unwrap(), 60_000_000_000);
        assert_eq!(parse_duration("2").unwrap(), 2_000_000_000);
    }

    #[test]
    fn trailing_window_clamps_to_recording() {
        // Window longer than the recording must not underflow.
        let (start, end) = window("10m").resolve(30_000_000_000).unwrap().unwrap();
        assert_eq!((start, end), (0, None));
    }

    #[test]
    fn rejects_bad_input() {
        assert!(window("bogus").resolve(1_000_000_000).is_err());
        assert!(window("20s..10s").resolve(60_000_000_000).is_err());
        assert!(
            WindowArgs { window: None }
                .resolve(1_000)
                .unwrap()
                .is_none()
        );
    }
}
