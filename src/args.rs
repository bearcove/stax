use structopt::StructOpt;

pub enum TargetProcess {
    ByPid(u32),
    ByName(String),
}

#[derive(StructOpt, Clone, Debug)]
#[structopt(rename_all = "kebab-case")]
pub struct ProcessFilter {
    /// Profile a process with a given PID (conflicts with --process)
    #[structopt(
        long,
        short = "p",
        raw(required_unless_one = r#"&["process"]"#)
    )]
    pid: Option<u32>,
    /// Profile a process with a given name (conflicts with --pid)
    #[structopt(
        long,
        short = "P",
        raw(required_unless_one = r#"&["pid"]"#)
    )]
    process: Option<String>,
}

impl From<ProcessFilter> for TargetProcess {
    fn from(args: ProcessFilter) -> Self {
        if let Some(process) = args.process {
            TargetProcess::ByName(process)
        } else if let Some(pid) = args.pid {
            TargetProcess::ByPid(pid)
        } else {
            unreachable!();
        }
    }
}

#[derive(StructOpt, Debug)]
#[structopt(rename_all = "kebab-case")]
pub struct RecordArgs {
    /// PET sampling frequency, in Hz.
    #[structopt(long, short = "F", default_value = "900")]
    pub frequency: u32,

    /// Stop sampling after this many seconds. Unlimited by default
    /// (Ctrl-C to stop).
    #[structopt(long, short = "l")]
    pub time_limit: Option<u64>,

    #[structopt(flatten)]
    pub process_filter: ProcessFilter,

    /// Start a live RPC/WebSocket server on the given host:port (e.g.
    /// 127.0.0.1:8080).
    #[structopt(long)]
    pub serve: Option<String>,

    /// Local socket path of the running `staxd` daemon. Defaults to the
    /// path `sudo stax setup` installs.
    #[structopt(long, default_value = "/var/run/staxd.sock")]
    pub daemon_socket: String,

    /// Arguments to pass to the launched child process. Use `--` to
    /// separate stax flags from the target's arguments:
    ///
    ///     stax record --process /bin/foo -- --some-flag bar baz
    #[structopt(raw(last = "true"), name = "PROGRAM_ARGS")]
    pub program_args: Vec<String>,
}

#[derive(StructOpt, Debug)]
#[structopt(
    raw(setting = "structopt::clap::AppSettings::ArgRequiredElseHelp")
)]
pub enum Opt {
    /// Record live profiling data, streamed over `--serve`.
    #[structopt(name = "record")]
    Record(RecordArgs),

    /// Codesign this stax binary (or, when run as root, install staxd
    /// as a LaunchDaemon).
    #[cfg(target_os = "macos")]
    #[structopt(name = "setup")]
    Setup(SetupArgs),
}

#[cfg(target_os = "macos")]
#[derive(StructOpt, Debug)]
#[structopt(rename_all = "kebab-case")]
pub struct SetupArgs {
    /// Skip the confirmation prompt before running `codesign`.
    #[structopt(long, short = "y")]
    pub yes: bool,
}
