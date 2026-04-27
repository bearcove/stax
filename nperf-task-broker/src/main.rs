//! `nperf-task-broker` — same-uid Mach IPC broker for task port handout.
//!
//! Why this exists: AMFI denies `task_for_pid` from a process that
//! crossed a privilege boundary against the target — root → privilege-
//! dropped child fails, even though both are owned by the same logical
//! user. With the `com.apple.security.cs.debugger` entitlement (granted
//! via ad-hoc codesigning, see `./entitlements.plist`), this binary
//! can call `task_for_pid` against any same-uid + same-team-id pid.
//!
//! Designed to run under launchd as a per-user LaunchAgent. Service
//! name `eu.bearcove.nperf.task-broker`. Clients (`nperf-live`,
//! `nperf record`, ...) call `bootstrap_look_up`, send a request
//! Mach message with the target pid, and receive a reply containing
//! a Mach send right on the task port (or an error code).
//!
//! v0 protocol:
//!   request:  msgh_id = 1, body { i32 pid }
//!   reply:    msgh_id = 1 | reply, body {
//!     mach_msg_port_descriptor (task port, MOVE_SEND, NULL on error),
//!     kern_return_t error,
//!   }
//!
//! Subsequent versions can add operations (memory read/write at
//! arbitrary addresses, thread state queries) by introducing new
//! `msgh_id` values without changing the existing message format.

#![cfg(target_os = "macos")]

use std::ffi::CString;
use std::mem::{self, MaybeUninit};

use mach2::bootstrap::{BOOTSTRAP_SUCCESS, bootstrap_check_in, bootstrap_port};
use mach2::kern_return::{KERN_SUCCESS, kern_return_t};
use mach2::message::{
    MACH_MSGH_BITS, MACH_MSGH_BITS_COMPLEX, MACH_MSG_SUCCESS, MACH_MSG_TIMEOUT_NONE,
    MACH_MSG_TYPE_MOVE_SEND, MACH_RCV_MSG, MACH_SEND_MSG, mach_msg, mach_msg_body_t,
    mach_msg_header_t, mach_msg_port_descriptor_t, mach_msg_size_t,
};
use mach2::port::{MACH_PORT_NULL, mach_port_t};
use mach2::traps::{mach_task_self, task_for_pid};
use tracing::{info, warn};

const SERVICE_NAME: &str = "eu.bearcove.nperf.task-broker";

/// Disposition the client gives us in the request's `msgh_local_port`
/// is `MAKE_SEND_ONCE`, which the kernel converts to `MOVE_SEND_ONCE`
/// on the receiving side. We mirror that for the reply.
const MACH_MSG_TYPE_MOVE_SEND_ONCE: u32 = 18;

/// Message id the client sets on a `task_for_pid` request.
const MSG_ID_TASK_FOR_PID: i32 = 1;
/// Reply messages OR the request id with this bit, mirroring MIG.
const MSG_ID_REPLY_BIT: i32 = 0x100;

/// Wire format of a request. Layout matches what the client constructs;
/// no padding manipulation needed because everything is naturally
/// aligned at LP64.
#[repr(C)]
#[derive(Default)]
struct RequestMsg {
    header: mach_msg_header_t,
    pid: i32,
    /// Padding so `mach_msg_size_t` matches `size_of::<RequestMsg>()`
    /// without reflection on alignment quirks.
    _pad: u32,
}

#[repr(C)]
struct ReplyMsg {
    header: mach_msg_header_t,
    body: mach_msg_body_t,
    task_port: mach_msg_port_descriptor_t,
    error: kern_return_t,
    _pad: u32,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "nperf_task_broker=info".into()),
        )
        .init();

    let server_port = check_in_with_launchd()?;
    info!("nperf-task-broker registered as {SERVICE_NAME}");

    serve_loop(server_port)
}

/// Receive the service's receive port from launchd. The plist's
/// `MachServices` dict creates the receive right on launchd's behalf
/// and hands it over to us when we check in. If the plist isn't
/// installed (running by hand for testing), this fails with
/// `BOOTSTRAP_UNKNOWN_SERVICE` — there's no graceful fallback.
fn check_in_with_launchd() -> Result<mach_port_t, Box<dyn std::error::Error>> {
    let service_cstr = CString::new(SERVICE_NAME)?;
    let mut server_port: mach_port_t = MACH_PORT_NULL;
    let kr = unsafe {
        bootstrap_check_in(
            bootstrap_port,
            service_cstr.as_ptr(),
            &mut server_port,
        )
    };
    if kr != BOOTSTRAP_SUCCESS as i32 {
        return Err(format!(
            "bootstrap_check_in({SERVICE_NAME}) failed: {kr:#x} \
             (LaunchAgent plist installed?)"
        )
        .into());
    }
    Ok(server_port)
}

fn serve_loop(server_port: mach_port_t) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        let mut req: MaybeUninit<RequestMsg> = MaybeUninit::uninit();
        let kr = unsafe {
            mach_msg(
                req.as_mut_ptr() as *mut mach_msg_header_t,
                MACH_RCV_MSG,
                0,
                mem::size_of::<RequestMsg>() as mach_msg_size_t,
                server_port,
                MACH_MSG_TIMEOUT_NONE,
                MACH_PORT_NULL,
            )
        };
        if kr != MACH_MSG_SUCCESS {
            warn!("mach_msg recv failed: {kr:#x}; continuing");
            continue;
        }
        let req = unsafe { req.assume_init() };

        if req.header.msgh_id != MSG_ID_TASK_FOR_PID {
            warn!("unknown msgh_id {:#x}; dropping", req.header.msgh_id);
            // Drop the reply port too by not replying — kernel will
            // reclaim the send-once right when the message is destroyed.
            continue;
        }

        handle_task_for_pid(&req);
    }
}

fn handle_task_for_pid(req: &RequestMsg) {
    let target_pid = req.pid;
    let reply_port = req.header.msgh_remote_port;
    info!(target_pid, reply_port = format!("{reply_port:#x}"), "task_for_pid request");

    let mut task: mach_port_t = MACH_PORT_NULL;
    let tfp_kr = unsafe { task_for_pid(mach_task_self(), target_pid, &mut task) };
    if tfp_kr != KERN_SUCCESS {
        warn!(target_pid, kr = format!("{tfp_kr:#x}"), "task_for_pid failed");
    } else {
        info!(target_pid, task = format!("{task:#x}"), "task_for_pid ok");
    }

    let mut reply = ReplyMsg {
        header: mach_msg_header_t {
            // MOVE_SEND_ONCE on the remote port (consumes the
            // send-once right the client gave us); local port is
            // null because we're not expecting a follow-up.
            msgh_bits: MACH_MSGH_BITS(MACH_MSG_TYPE_MOVE_SEND_ONCE, 0)
                | MACH_MSGH_BITS_COMPLEX,
            msgh_size: mem::size_of::<ReplyMsg>() as mach_msg_size_t,
            msgh_remote_port: reply_port,
            msgh_local_port: MACH_PORT_NULL,
            msgh_voucher_port: 0,
            msgh_id: MSG_ID_TASK_FOR_PID | MSG_ID_REPLY_BIT,
        },
        body: mach_msg_body_t { msgh_descriptor_count: 1 },
        task_port: mach_msg_port_descriptor_t::new(
            if tfp_kr == KERN_SUCCESS { task } else { MACH_PORT_NULL },
            MACH_MSG_TYPE_MOVE_SEND,
        ),
        error: tfp_kr,
        _pad: 0,
    };

    let send_kr = unsafe {
        mach_msg(
            &mut reply.header as *mut _,
            MACH_SEND_MSG,
            mem::size_of::<ReplyMsg>() as mach_msg_size_t,
            0,
            MACH_PORT_NULL,
            MACH_MSG_TIMEOUT_NONE,
            MACH_PORT_NULL,
        )
    };
    if send_kr != MACH_MSG_SUCCESS {
        warn!("mach_msg send failed: {send_kr:#x}");
    }
}
