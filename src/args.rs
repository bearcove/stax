use facet::Facet;
use figue as args;

pub enum TargetProcess {
    ByPid(u32),
    Launch { program: String, args: Vec<String> },
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

    /// Save the current or most recent queryable run to a v2 directory archive.
    Save(ArchiveArgs),

    /// Open a saved run archive into stax-server's query state.
    Open(ArchiveArgs),

    /// Restore a stopped in-memory run into stax-server's query state.
    SelectRun(SelectRunArgs),

    /// Compare two saved run archives without touching stax-server state.
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
}

#[derive(Facet, Debug)]
pub struct ArchiveArgs {
    /// Archive directory, v2 manifest.json, or legacy v1 archive.json file.
    #[facet(args::positional)]
    pub path: String,
}

#[derive(Facet, Debug)]
pub struct CompareArgs {
    /// Baseline archive directory, v2 manifest.json, or legacy v1 archive.json.
    #[facet(args::positional)]
    pub baseline: String,

    /// Candidate archive directory, v2 manifest.json, or legacy v1 archive.json.
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
    /// Select a stopped in-memory run from `stax list` before querying.
    /// Equivalent to `stax select-run <ID>` followed by `stax diagnose`.
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
    /// Select a stopped in-memory run from `stax list` before querying.
    /// Equivalent to `stax select-run <ID>` followed by `stax top`.
    #[facet(args::named, default)]
    pub run: Option<u64>,

    /// Maximum number of function/span-name entries to return.
    #[facet(args::named, args::short = 'n', default = 20)]
    pub limit: u32,

    /// Sort by `self` (leaf) or `total` (any frame). `total` is useful
    /// for CPU threads with origin-linked target spans because the
    /// dispatch stack owns the target duration as an ancestor.
    #[facet(args::named, default = "self")]
    pub sort: String,

    /// Filter to one thread by tid. Origin-linked target spans are included
    /// for the CPU thread that queued them. Default: all threads.
    #[facet(args::named, default)]
    pub tid: Option<u32>,
}

#[derive(Facet, Debug)]
pub struct ThreadsArgs {
    /// Select a stopped in-memory run from `stax list` before querying.
    /// Equivalent to `stax select-run <ID>` followed by `stax threads`.
    #[facet(args::named, default)]
    pub run: Option<u64>,

    /// Maximum number of threads/lanes to print, sorted by total
    /// activity; target lanes with spans are always included. 0 to
    /// print all.
    #[facet(args::named, args::short = 'n', default = 20)]
    pub limit: u32,
}

#[derive(Facet, Debug)]
pub struct FlameArgs {
    /// Select a stopped in-memory run from `stax list` before querying.
    /// Equivalent to `stax select-run <ID>` followed by `stax flame`.
    #[facet(args::named, default)]
    pub run: Option<u64>,

    /// Maximum tree depth to print. The flamegraph the server returns
    /// is unbounded; this just controls how deep the CLI prints
    /// (children below the cut-off are summarised as `…<N more
    /// frames>`). Cooperating target lanes render as `lane -> span`, or
    /// `CPU stack -> lane -> span` when the target reports origins.
    #[facet(args::named, args::short = 'd', default = 12)]
    pub max_depth: usize,

    /// Hide nodes whose share of total active time falls
    /// below this percent. `0` to print everything.
    #[facet(args::named, default = 1.0)]
    pub threshold_pct: f64,

    /// Filter to one thread by tid. Origin-linked target spans are included
    /// for the CPU thread that queued them. Default: all threads.
    #[facet(args::named, default)]
    pub tid: Option<u32>,
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

    /// Select a stopped in-memory run from `stax list` before querying.
    /// Equivalent to `stax select-run <ID>` followed by `stax annotate`.
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
}
