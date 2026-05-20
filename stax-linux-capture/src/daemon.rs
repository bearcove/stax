//! Daemon path: get the per-CPU perf fds from the privileged staxd
//! over vox (SCM_RIGHTS), then drain/parse them with the exact same
//! core as the in-process path.
//!
//! This is the unprivileged half of the Linux fd broker. The daemon
//! does the one privileged thing (`perf_event_open` per CPU) and
//! replies with the descriptors; from there everything —
//! `/proc/kallsyms`, the `/proc/<pid>` synthesis, enabling the events,
//! the poll/drain/parse loop, the wchan off-CPU attribution — is
//! unprivileged and identical to `record()`. So a host with a
//! restrictive `perf_event_paranoid` profiles exactly as well as a
//! permissive one, with `stax record …` still running unprivileged.

use std::sync::atomic::AtomicBool;

use eyre::Context;
use staxd_proto::{PerfSessionConfig, StaxdLinuxClient};

use crate::sys::{PerfRing, ring_from_fd};
use crate::{RecordOptions, RecordSummary};

/// Connect to the privileged staxd at `daemon_socket`, ask it to
/// `perf_event_open` the per-CPU rings for `opts.pid`, take ownership
/// of the returned descriptors, and run the capture loop locally.
///
/// `async` because the vox handshake/RPC is async; the subsequent
/// drain loop is synchronous and blocking — the caller already runs
/// this on a dedicated thread whose only job is this recording (same
/// contract as [`crate::record`]).
pub async fn record_via_daemon(
    daemon_socket: &str,
    opts: &RecordOptions,
    sink: &mut dyn crate::SampleSink,
    should_stop: &AtomicBool,
) -> eyre::Result<RecordSummary> {
    let url = format!("local://{daemon_socket}");
    tracing::info!(%url, pid = opts.pid, "connecting to staxd fd broker");

    let client: StaxdLinuxClient = vox::connect(&url)
        .await
        .with_context(|| format!("connecting to staxd at {url}"))?;

    let session = client
        .open_perf_session(PerfSessionConfig {
            target_pid: opts.pid,
            frequency_hz: opts.frequency_hz,
            kernel_stacks: opts.kernel_stacks,
        })
        .await
        // vox folds the method's `Result<_, PerfSessionError>` into
        // `VoxError<PerfSessionError>` (transport/protocol *or* the
        // app refusal); `VoxError` isn't `std::error::Error`, so map
        // it by hand rather than `.context()`.
        .map_err(|e| eyre::eyre!("staxd open_perf_session failed: {e:?}"))?;

    tracing::info!(
        cpus = session.cpu_count,
        sampling = session.sampling.len(),
        switch = session.switch.len(),
        page_size = session.page_size,
        data_pages = session.data_pages,
        "received perf fds from staxd"
    );

    // Materialise the descriptors into owned fds *before* dropping the
    // vox client — the `vox::Fd`s already carry live, dup'd
    // descriptors (SCM_RIGHTS delivered them as part of the completed
    // reply), so the daemon is out of the picture from here.
    let mut rings: Vec<PerfRing> = Vec::with_capacity(session.sampling.len());
    for (cpu, fd) in session.sampling.into_iter().enumerate() {
        let owned = fd
            .into_owned_fd()
            .ok_or_else(|| eyre::eyre!("staxd sent a sampling Fd with no descriptor (cpu {cpu})"))?;
        rings.push(ring_from_fd(owned).with_context(|| format!("mmap sampling ring cpu {cpu}"))?);
    }
    let mut switch_rings: Vec<PerfRing> = Vec::with_capacity(session.switch.len());
    for (cpu, fd) in session.switch.into_iter().enumerate() {
        let owned = fd
            .into_owned_fd()
            .ok_or_else(|| eyre::eyre!("staxd sent a switch Fd with no descriptor (cpu {cpu})"))?;
        switch_rings
            .push(ring_from_fd(owned).with_context(|| format!("mmap switch ring cpu {cpu}"))?);
    }

    // Connection no longer needed — the kernel ring buffers are the
    // data path now. Closing it frees the daemon's per-connection
    // task while we profile (which can run for minutes).
    drop(client);

    // No PMU group on the daemon-brokered path yet (the broker hands
    // over sampling + switch fds but not HW counter siblings). The
    // leader's SAMPLE_READ entry has an id the parser doesn't
    // recognise, so cycles/instructions/etc. stay 0 — matching the
    // SampleEvent contract. Brokering the PMU group is a follow-up.
    crate::session::run_with_rings(
        opts,
        sink,
        should_stop,
        rings,
        switch_rings,
        crate::sys::PmuGroup::default(),
    )
}
