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

use crate::sys::{PerfRing, PerfRingKind, PmuGroup, PmuKind, PmuMember, ring_from_fd};
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
            // Wakeup attribution is the whole reason this broker
            // exists on locked-down hosts; ask for it. Daemon falls
            // back to an empty `waking` if tracefs is unavailable.
            request_waking: true,
            // HW counter group is the *other* thing only the daemon
            // can open on locked-down hosts. Daemon returns an empty
            // `pmu` if any CPU couldn't host the full group (cycles,
            // instructions, L1D read misses, branch mispredicts).
            request_pmu: true,
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
        waking = session.waking.len(),
        pmu_per_cpu = session.pmu_per_cpu,
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
        rings.push(
            ring_from_fd(owned, PerfRingKind::Sampling)
                .with_context(|| format!("mmap sampling ring cpu {cpu}"))?,
        );
    }
    let mut switch_rings: Vec<PerfRing> = Vec::with_capacity(session.switch.len());
    for (cpu, fd) in session.switch.into_iter().enumerate() {
        let owned = fd
            .into_owned_fd()
            .ok_or_else(|| eyre::eyre!("staxd sent a switch Fd with no descriptor (cpu {cpu})"))?;
        switch_rings.push(
            ring_from_fd(owned, PerfRingKind::Switch)
                .with_context(|| format!("mmap switch ring cpu {cpu}"))?,
        );
    }
    let mut waking_rings: Vec<PerfRing> = Vec::with_capacity(session.waking.len());
    for (cpu, fd) in session.waking.into_iter().enumerate() {
        let owned = fd
            .into_owned_fd()
            .ok_or_else(|| eyre::eyre!("staxd sent a waking Fd with no descriptor (cpu {cpu})"))?;
        waking_rings.push(
            ring_from_fd(owned, PerfRingKind::Waking)
                .with_context(|| format!("mmap waking ring cpu {cpu}"))?,
        );
    }
    let waking_offsets = session.waking_field_offsets;

    // PMU sibling group: the daemon shipped `pmu_per_cpu` fds per CPU
    // in canonical `PmuKind::index()` order (cycles, instructions,
    // L1d misses, branch mispredicts). All-or-nothing on the daemon
    // side, so an empty `pmu` == "no HW counters this session".
    let mut pmu = PmuGroup::default();
    let per_cpu = session.pmu_per_cpu as usize;
    if per_cpu > 0 {
        let expected = per_cpu * session.cpu_count as usize;
        if session.pmu.len() != expected || session.pmu_ids.len() != expected {
            eyre::bail!(
                "staxd: PMU layout mismatch (per_cpu={} cpus={} fds={} ids={})",
                per_cpu,
                session.cpu_count,
                session.pmu.len(),
                session.pmu_ids.len()
            );
        }
        const KINDS: [PmuKind; 4] = [
            PmuKind::Cycles,
            PmuKind::Instructions,
            PmuKind::L1dMisses,
            PmuKind::BranchMisses,
        ];
        let mut id_iter = session.pmu_ids.into_iter();
        let mut fd_iter = session.pmu.into_iter();
        for cpu in 0..session.cpu_count as usize {
            let mut siblings = Vec::with_capacity(per_cpu);
            for slot in 0..per_cpu {
                let id = id_iter.next().unwrap();
                let fd = fd_iter
                    .next()
                    .unwrap()
                    .into_owned_fd()
                    .ok_or_else(|| eyre::eyre!("staxd sent a PMU Fd with no descriptor (cpu {cpu})"))?;
                let kind = KINDS[slot];
                pmu.id_to_kind.insert(id, kind);
                siblings.push(PmuMember { kind, id, fd });
            }
            pmu.siblings.push(siblings);
        }
    }

    // Connection no longer needed — the kernel ring buffers are the
    // data path now. Closing it frees the daemon's per-connection
    // task while we profile (which can run for minutes).
    drop(client);

    crate::session::run_with_rings(
        opts,
        sink,
        should_stop,
        rings,
        switch_rings,
        waking_rings,
        waking_offsets,
        pmu,
    )
}
