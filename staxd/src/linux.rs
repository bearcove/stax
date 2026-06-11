//! Linux staxd — the privileged perf **fd broker**.
//!
//! Unlike macOS (xnu has no descriptor to share, so the daemon streams
//! `KdBuf` records), Linux `perf_event_open` *is* a file descriptor.
//! So this daemon does only the one privileged thing — the per-CPU
//! `perf_event_open` — and hands the resulting descriptors back to the
//! unprivileged caller over a vox local socket (`vox::Fd`, carried in
//! `SCM_RIGHTS` ancillary data). The caller mmaps the rings and runs
//! the entire drain/parse/symbolicate pipeline itself. The daemon is
//! out of the data path the instant it replies.
//!
//! Net effect: on a host with a restrictive `perf_event_paranoid`,
//! `stax record …` still runs unprivileged and profiles exactly as
//! well as on a permissive host — the daemon (running as root via the
//! systemd unit, or `--foreground` for hand-running) is the only thing
//! that needs the privilege.

use std::path::PathBuf;
use std::time::Duration;

use eyre::{Context, Result};
use tracing::{info, warn};

use staxd_proto::{
    DaemonStatus, PerfSessionConfig, PerfSessionError, PerfSessionFds, STAXD_LINUX_SOCKET_DEFAULT,
    StaxdLinux, StaxdLinuxDispatcher,
};

struct Args {
    socket: PathBuf,
    foreground: bool,
}

impl Args {
    fn parse() -> Self {
        let mut socket = PathBuf::from(STAXD_LINUX_SOCKET_DEFAULT);
        let mut foreground = false;
        let mut it = std::env::args().skip(1);
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--socket" => {
                    if let Some(p) = it.next() {
                        socket = PathBuf::from(p);
                    }
                }
                "--foreground" | "-f" => foreground = true,
                _ => {}
            }
        }
        Self { socket, foreground }
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
pub async fn main() -> Result<()> {
    let args = Args::parse();
    init_logging();

    let privileged = is_privileged();
    let paranoid = read_paranoid();
    info!(
        version = env!("CARGO_PKG_VERSION"),
        socket = %args.socket.display(),
        foreground = args.foreground,
        privileged,
        perf_event_paranoid = paranoid,
        "staxd (linux fd broker) starting"
    );
    if !privileged {
        warn!(
            "staxd is not running as root; perf_event_open will only succeed if \
             perf_event_paranoid ({paranoid}) is permissive. The point of the \
             daemon is to be the privileged one — install the systemd unit \
             (`sudo stax setup`, User=root) for locked-down hosts."
        );
    }

    let socket_path = args.socket.clone();
    if socket_path.exists() {
        // Stale socket from a crashed previous run; vox bind would
        // fail otherwise. We own this path as the daemon.
        std::fs::remove_file(&socket_path)
            .with_context(|| format!("removing stale socket {}", socket_path.display()))?;
    }

    let listener =
        vox::transport::local::LocalLinkAcceptor::bind(socket_path.to_string_lossy().into_owned())
            .with_context(|| format!("binding {}", socket_path.display()))?;
    // Permissive perms for hand-running; the systemd deployment can
    // tighten ownership/perms on the socket directory.
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o666));
    info!("staxd listening on local://{}", socket_path.display());

    // Inline accept loop (mirrors macOS): the peer is a process; when
    // it exits the session should end. Vox 0.9 removed resumable
    // sessions, so this is the default stack-wide behavior now.
    let serve = tokio::spawn(async move {
        loop {
            let link = match listener.accept().await {
                Ok(l) => l,
                Err(e) => {
                    warn!("staxd: accept failed: {e}");
                    continue;
                }
            };
            tokio::spawn(async move {
                let dispatcher = StaxdLinuxDispatcher::new(LinuxStaxd);
                let result = vox::acceptor_on(link)
                    .observer(stax_vox_observe::VoxObserverLogger::new(
                        "staxd",
                        "staxd-linux",
                    ))
                    .keepalive(vox::SessionKeepaliveConfig {
                        ping_interval: Duration::from_secs(5),
                        pong_timeout: Duration::from_secs(30),
                    })
                    .on_connection(dispatcher)
                    .establish::<vox::NoopClient>()
                    .await;
                match result {
                    Ok(client) => client.caller.closed().await,
                    Err(e) => warn!("staxd: session establish failed: {e:?}"),
                }
            });
        }
    });

    tokio::select! {
        _ = tokio::signal::ctrl_c() => info!("staxd: SIGINT, shutting down"),
        r = serve => {
            // r: Result<!, JoinError> — the inner future is `loop {}`.
            match r {
                Ok(never) => match never {},
                Err(e) => warn!("serve task panicked: {e}"),
            }
        }
    }

    let _ = std::fs::remove_file(&socket_path);
    Ok(())
}

/// Stateless: every `open_perf_session` mints its own per-CPU fds, so
/// (unlike macOS kperf, which is single-owner machine-wide) there is
/// no global session to serialise — concurrent clients are fine.
#[derive(Clone)]
struct LinuxStaxd;

impl StaxdLinux for LinuxStaxd {
    async fn status(&self) -> DaemonStatus {
        DaemonStatus {
            version: env!("CARGO_PKG_VERSION").to_string(),
            host_arch: host_arch().to_string(),
            privileged: is_privileged(),
            perf_event_paranoid: read_paranoid(),
        }
    }

    async fn open_perf_session(
        &self,
        config: PerfSessionConfig,
    ) -> Result<PerfSessionFds, PerfSessionError> {
        if !std::path::Path::new(&format!("/proc/{}", config.target_pid)).exists() {
            return Err(PerfSessionError::NoSuchTarget(config.target_pid));
        }

        let cpus = stax_linux_capture::online_cpus();
        let root = is_privileged();

        // Sampling rings: required. A privilege failure here is the
        // signal to the client that it must not silently fall back to
        // an in-process open that would fail the same way. We open the
        // HW sibling group eagerly here (while we still hold the
        // leader's `OwnedFd` directly) — that needs the leader's raw
        // fd as `group_fd` and avoids reaching back into a `vox::Fd`
        // wrapper later. The siblings are best-effort; the leader is
        // not.
        use std::os::fd::AsRawFd;
        let mut sampling: Vec<vox::Fd> = Vec::with_capacity(cpus.len());
        let mut pmu: Vec<vox::Fd> = Vec::with_capacity(cpus.len() * 4);
        let mut pmu_ids: Vec<u64> = Vec::with_capacity(cpus.len() * 4);
        let mut pmu_failed = !config.request_pmu;
        for &cpu in &cpus {
            let leader = match stax_linux_capture::open_cpu_fd(
                cpu,
                config.frequency_hz,
                config.kernel_stacks,
                config.request_dwarf_unwind,
            ) {
                Ok(fd) => fd,
                Err(e) => {
                    let errno = e.raw_os_error().unwrap_or(0);
                    if !root && matches!(errno, libc::EACCES | libc::EPERM) {
                        return Err(PerfSessionError::NotPrivileged {
                            detail: format!(
                                "perf_event_open cpu {cpu}: {e} (perf_event_paranoid={})",
                                read_paranoid()
                            ),
                        });
                    }
                    return Err(PerfSessionError::PerfEventOpen {
                        cpu,
                        errno,
                        detail: e.to_string(),
                    });
                }
            };
            if !pmu_failed {
                match stax_linux_capture::open_cpu_pmu_siblings(
                    cpu,
                    leader.as_raw_fd(),
                    config.kernel_stacks,
                ) {
                    Ok(members) => {
                        for m in members {
                            pmu_ids.push(m.id);
                            pmu.push(vox::Fd::new(m.fd));
                        }
                    }
                    Err(e) => {
                        warn!(%e, cpu, "PMU sibling open failed; HW counters disabled");
                        pmu_failed = true;
                    }
                }
            }
            sampling.push(vox::Fd::new(leader));
        }
        if pmu_failed {
            // All-or-nothing: if any CPU couldn't open the full group,
            // ship no PMU at all so the client doesn't have to handle
            // a partial layout.
            pmu.clear();
            pmu_ids.clear();
        }
        let pmu_per_cpu: u32 = if pmu.is_empty() { 0 } else { 4 };

        // Context-switch rings: best-effort. A kernel/host without
        // `context_switch` loses off-CPU attribution but the on-CPU
        // profile still works — don't fail the whole session.
        let mut switch = Vec::with_capacity(cpus.len());
        for &cpu in &cpus {
            match stax_linux_capture::open_cpu_switch_fd(cpu) {
                Ok(fd) => switch.push(vox::Fd::new(fd)),
                Err(e) => {
                    warn!(%e, cpu, "context-switch ring open failed; off-CPU disabled");
                    switch.clear();
                    break;
                }
            }
        }

        // sched:sched_waking tracepoint rings — the wakeup-attribution
        // payload the entire broker exists for on locked-down hosts.
        // Best-effort: a host without tracefs / `sched:sched_waking`
        // (rare, stripped kernels) just loses waker_tid; the on-CPU
        // profile and off-CPU intervals are unaffected.
        let (waking, waking_field_offsets) = if config.request_waking {
            match stax_linux_capture::read_sched_waking_tracepoint() {
                Ok((id, offsets)) => {
                    let mut fds: Vec<vox::Fd> = Vec::with_capacity(cpus.len());
                    let mut failed = false;
                    for &cpu in &cpus {
                        match stax_linux_capture::open_cpu_waking_fd(cpu, id, config.kernel_stacks)
                        {
                            Ok(fd) => fds.push(vox::Fd::new(fd)),
                            Err(e) => {
                                warn!(%e, cpu, "sched_waking open failed; wakeup attribution disabled");
                                failed = true;
                                break;
                            }
                        }
                    }
                    if failed {
                        (Vec::new(), None)
                    } else {
                        (fds, Some(offsets))
                    }
                }
                Err(e) => {
                    warn!(%e, "reading sched_waking tracepoint id/format failed; wakeups disabled");
                    (Vec::new(), None)
                }
            }
        } else {
            (Vec::new(), None)
        };

        let cpu_count = sampling.len() as u32;
        info!(
            pid = config.target_pid,
            cpus = cpu_count,
            off_cpu = !switch.is_empty(),
            wakeups = !waking.is_empty(),
            pmu = pmu_per_cpu > 0,
            "brokered perf fds to client"
        );

        Ok(PerfSessionFds {
            sampling,
            switch,
            waking,
            waking_field_offsets,
            pmu,
            pmu_ids,
            pmu_per_cpu,
            cpu_count,
            page_size: stax_linux_capture::page_size() as u32,
            data_pages: stax_linux_capture::DATA_PAGES as u32,
            target_pid: config.target_pid,
            frequency_hz: config.frequency_hz,
            kernel_stacks: config.kernel_stacks,
        })
    }
}

fn init_logging() {
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new("staxd=info,stax_linux_capture=info,stax_vox_observe=info")
    });
    // Plain fmt to stderr. Under the systemd unit stderr is wired to
    // the journal (`journalctl -u eu.bearcove.staxd`); with
    // `--foreground` it is the terminal.
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .init();
}

/// Can this process `perf_event_open` system-wide regardless of
/// `perf_event_paranoid`? True when running as root (the systemd
/// deployment, `User=root`). A `CAP_PERFMON`-only process also works
/// in practice — the open simply succeeds — so this is a hint for
/// `status()`/diagnostics, not a gate on the attempt.
fn is_privileged() -> bool {
    // SAFETY: geteuid is always-safe on Unix.
    unsafe { libc::geteuid() == 0 }
}

/// `/proc/sys/kernel/perf_event_paranoid`, or `i32::MIN` if unreadable.
fn read_paranoid() -> i32 {
    std::fs::read_to_string("/proc/sys/kernel/perf_event_paranoid")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(i32::MIN)
}

fn host_arch() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else {
        "unknown"
    }
}
