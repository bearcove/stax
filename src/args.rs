use facet::Facet;
use figue as args;

pub enum TargetProcess {
    ByPid(u32),
    Launch { program: String, args: Vec<String> },
}

/// stax — live profiler frontend that drives the staxd daemon backend
/// over a local socket and streams aggregated samples over WebSocket.
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
    /// Record live profiling data. Forwards events to the running
    /// `stax-server` for the web UI and `stax {top,annotate,…}` to
    /// query.
    Record(RecordArgs),

    /// Codesign this stax binary (or, when run as root, install staxd
    /// as a LaunchDaemon).
    Setup(SetupArgs),

    /// Print the current state of stax-server (active run + history).
    Status,

    /// List every run stax-server has hosted (active + history).
    List,

    /// Block until a condition fires, the active run stops, or the
    /// timeout elapses.
    Wait(WaitArgs),

    /// Ask stax-server to stop the active run cleanly.
    Stop,

    /// Snapshot the top-N functions from the active run.
    Top(TopArgs),

    /// Disassemble + annotate a function from the active run.
    Annotate(AnnotateArgs),
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
    /// Maximum number of entries to return.
    #[facet(args::named, args::short = 'n', default = 20)]
    pub limit: u32,

    /// Sort by `self` (leaf) or `total` (any frame). Default: `self`.
    #[facet(args::named, default = "self")]
    pub sort: String,

    /// Filter to one thread by tid. Default: all threads.
    #[facet(args::named, default)]
    pub tid: Option<u32>,
}

#[derive(Facet, Debug)]
pub struct AnnotateArgs {
    /// Address (hex with `0x` prefix or decimal) of an instruction
    /// inside the function to annotate.
    #[facet(args::positional)]
    pub address: String,

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
