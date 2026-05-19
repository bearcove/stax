//! Off-CPU classifier: turn the leaf user-space frame at the moment
//! a thread blocked into an `OffCpuReason`.
//!
//! Every off-CPU interval has a calling card: the leaf user PC at
//! the moment the thread parked. `__psynch_cvwait` means cond-var
//! wait, `__psynch_mutexwait` means contention, `read` means IO,
//! and so on. Knowing *why* a thread blocked is the difference
//! between "boring scheduler noise" and "this is the bottleneck."
//!
//! The matcher is pattern-based on the demangled symbol name, which
//! we already resolve through the binary registry. Symbols we don't
//! recognise (or off-CPU intervals with no PET stack to look up)
//! land in `OffCpuReason::Other`; that bucket is the "needs more
//! taxonomy" signal.

use stax_live_proto::OffCpuReason;

/// Strip glibc/kernel/compiler decorations so a blocked leaf like
/// `__GI___libc_read`, `read@@GLIBC_2.2.5`, `poll.localalias`,
/// `__mutex_lock.constprop.0`, or `futex_wait_queue+0x42` reduces to
/// its core name (`read`, `poll`, `mutex_lock`, `futex_wait_queue`).
/// C library and kernel symbols don't contain `.`/`@`/`+`/space, so
/// cutting at the first of those is safe; then we peel the `__GI_` /
/// `__libc_` internal-alias prefixes and any leading underscores
/// (`____sys_recvmsg` -> `sys_recvmsg`).
fn glibc_core(name: &str) -> &str {
    let n = name.split(['@', '.', '+', ' ']).next().unwrap_or(name);
    let n = n.strip_prefix("__GI_").unwrap_or(n);
    let n = n.strip_prefix("__libc_").unwrap_or(n);
    n.trim_start_matches('_')
}

/// Linux *kernel* blocking sites — the symbol `/proc/<tid>/wchan`
/// reports, which the recorder leads the off-CPU stack with. Keyed on
/// the decoration-stripped core (see `glibc_core`). futex covers
/// GIL/mutex/cond on Linux (one primitive); we bucket it as the
/// actionable `LockWait`. Block-layer page waits are lumped into
/// `IoRead` (dominant; the breakdown can't see direction here).
fn classify_linux_kernel(core: &str) -> Option<OffCpuReason> {
    Some(match core {
        "futex_wait_queue" | "futex_wait" | "futex_wait_setup" | "do_futex"
        | "futex_do_wait" | "mutex_lock" | "mutex_lock_slowpath"
        | "rwsem_down_read_slowpath" | "rwsem_down_write_slowpath" | "down_read"
        | "down_write" | "down" | "down_common" | "rt_mutex_slowlock"
        | "rt_mutex_slowlock_block" => OffCpuReason::LockWait,
        "hrtimer_nanosleep" | "do_nanosleep" | "schedule_hrtimeout_range"
        | "schedule_hrtimeout_range_clock" => OffCpuReason::Sleep,
        "do_select" | "core_sys_select" | "do_sys_poll" | "do_poll"
        | "do_epoll_wait" | "ep_poll" | "do_epoll_pwait"
        | "poll_schedule_timeout" => OffCpuReason::Readiness,
        "pipe_read" | "tcp_recvmsg" | "tcp_recvmsg_locked" | "sock_recvmsg"
        | "sys_recvmsg" | "unix_stream_read_generic" | "unix_dgram_recvmsg"
        | "inet_recvmsg" | "skb_wait_for_more_packets" | "skb_recv_datagram"
        | "io_schedule" | "bit_wait_io" | "folio_wait_bit_common" | "lock_page"
        | "wait_on_page_bit" => OffCpuReason::IoRead,
        "pipe_write" | "tcp_sendmsg" | "tcp_sendmsg_locked" | "sock_sendmsg"
        | "sys_sendmsg" | "sk_stream_wait_memory" => OffCpuReason::IoWrite,
        "inet_csk_accept" | "inet_csk_wait_for_connect" | "inet_wait_for_connect"
        | "inet_stream_connect" => OffCpuReason::ConnectionSetup,
        "wait_for_completion" | "wait_for_completion_state"
        | "wait_for_completion_timeout" | "pipe_wait" | "do_wait"
        | "do_sigtimedwait" | "sigsuspend" | "worker_thread" | "kthread"
        | "rescuer_thread" | "smpboot_thread_fn" | "kthread_worker_fn" => {
            OffCpuReason::Idle
        }
        _ => return None,
    })
}

/// Linux glibc/NPTL blocking leaves, keyed on the decoration-stripped
/// core (see `glibc_core`). Runs before the macOS arms; every shared
/// core (`read`, `poll`, `connect`, `pthread_cond_wait`, …) maps to
/// the *same* bucket the macOS matcher would pick, so this never
/// changes macOS behaviour — it only fills the Linux gaps
/// (`futex`/`lll`/`pthread_*`/`sem_*`/`epoll`/`clock_nanosleep` and
/// the `__GI_`/`__libc_` decorated forms).
fn classify_linux(core: &str) -> Option<OffCpuReason> {
    Some(match core {
        // Condition variables / futex park == "waiting for work".
        "pthread_cond_wait" | "pthread_cond_timedwait" | "pthread_cond_wait64"
        | "futex_wait" | "futex_abstimed_wait" | "futex_abstimed_wait_common"
        | "futex_abstimed_wait_common64" | "futex_abstimed_wait_cancelable" => {
            OffCpuReason::Idle
        }
        // Mutex / rwlock contention — the off-CPU you want to chase.
        "lll_lock_wait" | "lll_lock_wait_private" | "pthread_mutex_lock"
        | "pthread_mutex_timedlock" | "pthread_mutex_clocklock"
        | "pthread_rwlock_rdlock" | "pthread_rwlock_wrlock"
        | "pthread_rwlock_timedrdlock" | "pthread_rwlock_timedwrlock" => {
            OffCpuReason::LockWait
        }
        // POSIX semaphores.
        "sem_wait" | "sem_timedwait" | "sem_clockwait" | "new_sem_wait"
        | "new_sem_wait_slow" | "new_sem_wait_slow64" => OffCpuReason::SemaphoreWait,
        // fd readiness multiplexing.
        "epoll_wait" | "epoll_pwait" | "epoll_pwait2" | "epoll_wait_nocancel"
        | "poll" | "ppoll" | "ppoll64" | "poll_nocancel" | "select"
        | "pselect" | "pselect6" | "pselect32" | "select_nocancel" => {
            OffCpuReason::Readiness
        }
        // Explicit sleeps.
        "nanosleep" | "nanosleep64" | "nanosleep_nocancel" | "clock_nanosleep"
        | "clock_nanosleep64" | "clock_nanosleep_time64" | "usleep" | "sleep" => {
            OffCpuReason::Sleep
        }
        // Blocking reads.
        "read" | "read_nocancel" | "pread" | "pread64" | "preadv" | "preadv64"
        | "readv" | "recv" | "recvfrom" | "recvmsg" | "recvmmsg" => {
            OffCpuReason::IoRead
        }
        // Blocking writes.
        "write" | "write_nocancel" | "pwrite" | "pwrite64" | "pwritev"
        | "pwritev64" | "writev" | "send" | "sendto" | "sendmsg" | "sendmmsg" => {
            OffCpuReason::IoWrite
        }
        // Connection / fd setup that can block.
        "connect" | "connect_nocancel" | "accept" | "accept4" | "open"
        | "open64" | "openat" | "openat64" | "open_nocancel"
        | "openat_nocancel" => OffCpuReason::ConnectionSetup,
        _ => return None,
    })
}

/// Classify an off-CPU interval from the leaf-frame symbol name.
///
/// `function_name` is what `BinaryRegistry::lookup_symbol` returned
/// for the leaf address (already demangled). `None` here means the
/// frame couldn't be resolved at all; we still try a few patterns
/// against the empty string (always fall through to `Other`).
pub fn classify_offcpu(function_name: Option<&str>) -> OffCpuReason {
    let Some(name) = function_name else {
        return OffCpuReason::Other;
    };

    // Linux leaves first (decoration-normalised): the kernel wait site
    // (off-CPU leaf == wchan) then the glibc/NPTL user wrapper. Returns
    // only on a definite hit; otherwise fall through to the macOS
    // matchers below unchanged.
    let core = glibc_core(name);
    if let Some(reason) = classify_linux_kernel(core) {
        return reason;
    }
    if let Some(reason) = classify_linux(core) {
        return reason;
    }

    // Pattern matches are ordered by specificity: more-specific
    // pthread / kqueue functions before broad fallbacks. The
    // matchers all use `starts_with` / `==` rather than `contains`
    // because Rust's mangling sometimes embeds these names as
    // substrings (e.g. `<some::wrapper as Trait>::write`) and we
    // don't want a Rust function named "writer_loop" classified as
    // an IO syscall.

    // -- pthread / ulock synchronisation primitives --------------------
    // pthread_cond_wait & friends; the syscall stub is
    // `__psynch_cvwait`. ulock_wait is the libsystem-internal
    // futex-style primitive used by os_unfair_lock and dispatch.
    if name == "__psynch_cvwait"
        || name == "__ulock_wait"
        || name == "__ulock_wait2"
        || name == "__workq_kernreturn"
        || name == "_pthread_cond_wait"
        || name == "_pthread_cond_timedwait"
        || name == "_dispatch_workloop_worker_thread"
    {
        return OffCpuReason::Idle;
    }
    // Mutex / rwlock contention (lock owned by someone else; thread
    // wants to run but has to wait for the holder). This is the
    // off-CPU you usually want to chase down.
    if name == "__psynch_mutexwait"
        || name == "__psynch_rw_rdlock"
        || name == "__psynch_rw_wrlock"
        || name == "__psynch_rw_yieldwrlock"
        || name == "__psynch_rw_upgrade"
        || name == "__psynch_rw_downgrade"
        || name == "_pthread_mutex_firstfit_lock_wait"
        || name == "_pthread_mutex_lock"
        || name == "_pthread_mutex_lock_wait"
    {
        return OffCpuReason::LockWait;
    }

    // -- Semaphores ----------------------------------------------------
    if name == "__semwait_signal"
        || name == "__semwait_signal_nocancel"
        || name == "semaphore_wait_trap"
        || name == "semaphore_timedwait_trap"
        || name == "_dispatch_semaphore_wait"
    {
        return OffCpuReason::SemaphoreWait;
    }

    // -- Mach IPC ------------------------------------------------------
    // Threads blocked here are typically waiting for a Mach reply
    // port, which is either RPC or a dispatch-source delivery.
    if name == "mach_msg2_trap"
        || name == "mach_msg_trap"
        || name == "mach_msg_overwrite_trap"
        || name == "mach_msg2"
        || name == "mach_msg"
        || name == "mach_msg_overwrite"
    {
        return OffCpuReason::IpcWait;
    }

    // -- fd readiness --------------------------------------------------
    // Order matters: kevent goes first so kqueue waits don't fall
    // into the IO bucket.
    if name == "kevent"
        || name == "kevent_id"
        || name == "kevent_qos"
        || name == "select"
        || name == "select$DARWIN_EXTSN"
        || name == "select$DARWIN_EXTSN$NOCANCEL"
        || name == "pselect"
        || name == "poll"
        || name == "ppoll"
    {
        return OffCpuReason::Readiness;
    }

    // -- Explicit sleeps ----------------------------------------------
    if name == "nanosleep"
        || name == "__semwait_signal_nocancel"
        || name == "__nanosleep"
        || name == "usleep"
    {
        return OffCpuReason::Sleep;
    }

    // -- IO reads / writes --------------------------------------------
    // Match the libsystem syscall stubs *and* a few common cancellable
    // variants. We use `==` so user code named e.g. "writer" doesn't
    // get caught.
    if name == "read"
        || name == "__read_nocancel"
        || name == "recv"
        || name == "__recvfrom"
        || name == "recvfrom"
        || name == "__recvfrom_nocancel"
        || name == "recvmsg"
        || name == "__recvmsg_nocancel"
        || name == "pread"
        || name == "__pread_nocancel"
        || name == "readv"
    {
        return OffCpuReason::IoRead;
    }
    if name == "write"
        || name == "__write_nocancel"
        || name == "send"
        || name == "__sendto"
        || name == "sendto"
        || name == "__sendto_nocancel"
        || name == "sendmsg"
        || name == "__sendmsg_nocancel"
        || name == "pwrite"
        || name == "__pwrite_nocancel"
        || name == "writev"
    {
        return OffCpuReason::IoWrite;
    }

    // -- Connection setup ---------------------------------------------
    if name == "connect"
        || name == "__connect_nocancel"
        || name == "accept"
        || name == "__accept_nocancel"
        || name == "open"
        || name == "__open_nocancel"
        || name == "openat"
        || name == "__openat_nocancel"
    {
        return OffCpuReason::ConnectionSetup;
    }

    OffCpuReason::Other
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_paths() {
        assert_eq!(classify_offcpu(Some("__psynch_cvwait")), OffCpuReason::Idle);
        assert_eq!(
            classify_offcpu(Some("__workq_kernreturn")),
            OffCpuReason::Idle
        );
        assert_eq!(classify_offcpu(Some("__ulock_wait")), OffCpuReason::Idle);
    }

    #[test]
    fn lock_contention() {
        assert_eq!(
            classify_offcpu(Some("__psynch_mutexwait")),
            OffCpuReason::LockWait
        );
        assert_eq!(
            classify_offcpu(Some("__psynch_rw_wrlock")),
            OffCpuReason::LockWait
        );
    }

    #[test]
    fn ipc_and_readiness() {
        assert_eq!(
            classify_offcpu(Some("mach_msg2_trap")),
            OffCpuReason::IpcWait
        );
        assert_eq!(classify_offcpu(Some("kevent_id")), OffCpuReason::Readiness);
        assert_eq!(classify_offcpu(Some("poll")), OffCpuReason::Readiness);
    }

    #[test]
    fn io_split() {
        assert_eq!(classify_offcpu(Some("read")), OffCpuReason::IoRead);
        assert_eq!(
            classify_offcpu(Some("__read_nocancel")),
            OffCpuReason::IoRead
        );
        assert_eq!(classify_offcpu(Some("write")), OffCpuReason::IoWrite);
    }

    #[test]
    fn rust_function_named_write_does_not_match() {
        // A Rust function whose demangled name happens to contain
        // "write" must NOT be classified as an IO write -- the
        // matcher uses `==`, not `contains`.
        assert_eq!(
            classify_offcpu(Some("my_crate::Buffer::writer_loop")),
            OffCpuReason::Other
        );
        assert_eq!(classify_offcpu(Some("std::io::write")), OffCpuReason::Other);
    }

    #[test]
    fn no_symbol_is_other() {
        assert_eq!(classify_offcpu(None), OffCpuReason::Other);
        assert_eq!(classify_offcpu(Some("")), OffCpuReason::Other);
    }

    #[test]
    fn glibc_core_strips_decorations() {
        assert_eq!(glibc_core("__GI___libc_read"), "read");
        assert_eq!(glibc_core("read@@GLIBC_2.2.5"), "read");
        assert_eq!(glibc_core("poll.localalias"), "poll");
        assert_eq!(glibc_core("__lll_lock_wait"), "lll_lock_wait");
        assert_eq!(glibc_core("__pthread_cond_wait"), "pthread_cond_wait");
    }

    #[test]
    fn linux_blocking_leaves() {
        // futex / cond-var park -> waiting for work.
        assert_eq!(
            classify_offcpu(Some("__futex_abstimed_wait_common64")),
            OffCpuReason::Idle
        );
        assert_eq!(
            classify_offcpu(Some("pthread_cond_timedwait@@GLIBC_2.3.2")),
            OffCpuReason::Idle
        );
        // lock contention.
        assert_eq!(
            classify_offcpu(Some("__lll_lock_wait")),
            OffCpuReason::LockWait
        );
        assert_eq!(
            classify_offcpu(Some("__GI___pthread_mutex_lock")),
            OffCpuReason::LockWait
        );
        // semaphore, readiness, sleep.
        assert_eq!(
            classify_offcpu(Some("__new_sem_wait_slow64")),
            OffCpuReason::SemaphoreWait
        );
        assert_eq!(
            classify_offcpu(Some("epoll_wait")),
            OffCpuReason::Readiness
        );
        assert_eq!(
            classify_offcpu(Some("__GI___clock_nanosleep")),
            OffCpuReason::Sleep
        );
        // io split via decorated glibc names.
        assert_eq!(
            classify_offcpu(Some("__libc_recv")),
            OffCpuReason::IoRead
        );
        assert_eq!(classify_offcpu(Some("__sendmsg")), OffCpuReason::IoWrite);
        assert_eq!(
            classify_offcpu(Some("accept4")),
            OffCpuReason::ConnectionSetup
        );
    }

    #[test]
    fn linux_kernel_wait_sites() {
        // wchan leaves the recorder leads the off-CPU stack with.
        assert_eq!(
            classify_offcpu(Some("futex_wait_queue")),
            OffCpuReason::LockWait
        );
        assert_eq!(
            classify_offcpu(Some("__mutex_lock.constprop.0")),
            OffCpuReason::LockWait
        );
        assert_eq!(
            classify_offcpu(Some("hrtimer_nanosleep")),
            OffCpuReason::Sleep
        );
        assert_eq!(classify_offcpu(Some("pipe_read")), OffCpuReason::IoRead);
        assert_eq!(
            classify_offcpu(Some("tcp_sendmsg_locked")),
            OffCpuReason::IoWrite
        );
        assert_eq!(classify_offcpu(Some("ep_poll")), OffCpuReason::Readiness);
        assert_eq!(
            classify_offcpu(Some("inet_csk_wait_for_connect")),
            OffCpuReason::ConnectionSetup
        );
        assert_eq!(
            classify_offcpu(Some("wait_for_completion_state+0x1c")),
            OffCpuReason::Idle
        );
        // block-layer page wait, decorated.
        assert_eq!(
            classify_offcpu(Some("folio_wait_bit_common")),
            OffCpuReason::IoRead
        );
    }

    #[test]
    fn normalizer_does_not_misclassify_rust() {
        // Decoration-stripping must not turn a Rust symbol into a
        // glibc core: these have no syscall core and stay Other.
        assert_eq!(
            classify_offcpu(Some("tokio::net::tcp::write")),
            OffCpuReason::Other
        );
        assert_eq!(
            classify_offcpu(Some("my_crate::epoll_waiter")),
            OffCpuReason::Other
        );
    }
}
