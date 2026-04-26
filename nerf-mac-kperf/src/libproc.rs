//! Out-of-process introspection via libproc. Region enumeration and
//! per-thread name/id, all keyed by PID -- no Mach task port required.
//!
//! Lets us drop the `task_for_pid` + `mach_vm_read` path used by
//! `nerf-mac-capture::proc_maps` so the kperf child-launch flow keeps
//! working when the parent (root) has dropped the child to a non-root
//! uid: AMFI/task_for_pid policy denies the cross-uid task port even
//! to root, but `proc_pidinfo` is gated only on read-permission and
//! goes through with no fanfare.

use std::ffi::c_void;

use libc::{c_int, ESRCH};

// libc 0.2.186 is missing the PROC_PID* constants and the
// `proc_regionwithpathinfo` struct we need. Declare them here.

const PROC_PIDLISTTHREADS: c_int = 6;
const PROC_PIDREGIONPATHINFO: c_int = 8;

const MAXPATHLEN: usize = 1024;

/// Mirror of `<sys/proc_info.h>` `struct proc_regioninfo`. Layout
/// matches the kernel's verbatim under `#[repr(C)]`.
#[repr(C)]
#[derive(Clone, Copy)]
struct ProcRegionInfo {
    pri_protection: u32,
    pri_max_protection: u32,
    pri_inheritance: u32,
    pri_flags: u32,
    pri_offset: u64,
    pri_behavior: u32,
    pri_user_wired_count: u32,
    pri_user_tag: u32,
    pri_pages_resident: u32,
    pri_pages_shared_now_private: u32,
    pri_pages_swapped_out: u32,
    pri_pages_dirtied: u32,
    pri_ref_count: u32,
    pri_shadow_depth: u32,
    pri_share_mode: u32,
    pri_private_pages_resident: u32,
    pri_shared_pages_resident: u32,
    pri_obj_id: u32,
    pri_depth: u32,
    pri_address: u64,
    pri_size: u64,
}

/// Mirror of `<sys/proc_info.h>` `struct vnode_info`. We don't read any
/// of `vnode_stat`, so it's an opaque 144-byte pad (same size + align
/// as the C struct).
#[repr(C)]
#[derive(Clone, Copy)]
struct VnodeInfo {
    vi_stat_pad: [u64; 18], // 144 bytes; vnode_stat is 8-byte aligned
    vi_type: i32,
    vi_pad: i32,
    vi_fsid: [i32; 2],
}

/// Mirror of `<sys/proc_info.h>` `struct vnode_info_path`.
#[repr(C)]
#[derive(Clone, Copy)]
struct VnodeInfoPath {
    vip_vi: VnodeInfo,
    vip_path: [u8; MAXPATHLEN],
}

/// Mirror of `<sys/proc_info.h>` `struct proc_regionwithpathinfo`.
#[repr(C)]
#[derive(Clone, Copy)]
struct ProcRegionWithPathInfo {
    prp_prinfo: ProcRegionInfo,
    prp_vip: VnodeInfoPath,
}

const VM_PROT_READ: u32 = 0x1;
#[allow(dead_code)]
const VM_PROT_WRITE: u32 = 0x2;
const VM_PROT_EXECUTE: u32 = 0x4;

extern "C" {
    fn proc_pidinfo(
        pid: c_int,
        flavor: c_int,
        arg: u64,
        buffer: *mut c_void,
        buffersize: c_int,
    ) -> c_int;
}

/// One entry from a libproc region walk.
#[derive(Clone, Debug)]
pub struct Region {
    pub address: u64,
    pub size: u64,
    pub is_executable: bool,
    pub is_readable: bool,
    /// Filesystem path of the backing vnode, or empty for anonymous
    /// regions (typical for JIT'd code). Truncated at the first NUL.
    pub path: String,
}

/// Walk the target process's address space via
/// `proc_pidinfo(PROC_PIDREGIONPATHINFO)`. Returns one entry per
/// distinct VM region, in ascending address order.
///
/// Errors out with `ESRCH` if the process is gone; otherwise yields
/// whatever the kernel hands us.
pub fn enumerate_regions(pid: u32) -> std::io::Result<Vec<Region>> {
    let mut out = Vec::new();
    let mut addr: u64 = 0;
    loop {
        let mut info: ProcRegionWithPathInfo = unsafe { std::mem::zeroed() };
        let n = unsafe {
            proc_pidinfo(
                pid as c_int,
                PROC_PIDREGIONPATHINFO,
                addr,
                &mut info as *mut _ as *mut c_void,
                std::mem::size_of::<ProcRegionWithPathInfo>() as c_int,
            )
        };
        if n <= 0 {
            let err = std::io::Error::last_os_error();
            // ESRCH after at least one successful iteration just means
            // we walked off the end of the address space; treat it as
            // a clean stop.
            if err.raw_os_error() == Some(ESRCH) && !out.is_empty() {
                break;
            }
            if n == 0 {
                break;
            }
            return Err(err);
        }
        let pri = &info.prp_prinfo;
        let path_bytes = info.prp_vip.vip_path;
        let nul = path_bytes.iter().position(|&b| b == 0).unwrap_or(path_bytes.len());
        let path = String::from_utf8_lossy(&path_bytes[..nul]).into_owned();
        out.push(Region {
            address: pri.pri_address,
            size: pri.pri_size,
            is_executable: pri.pri_protection & VM_PROT_EXECUTE != 0,
            is_readable: pri.pri_protection & VM_PROT_READ != 0,
            path,
        });
        let next = pri.pri_address.saturating_add(pri.pri_size);
        if next <= addr {
            // Defensive: kernel didn't advance; bail rather than loop.
            break;
        }
        addr = next;
    }
    Ok(out)
}

/// Mirror of `<sys/proc_info.h>` `struct proc_threadinfo`. libc's
/// `proc_threadinfo` exists but we redeclare with the fields we need
/// to keep the bindings localised.
#[repr(C)]
#[derive(Clone, Copy)]
struct ProcThreadInfoC {
    pth_user_time: u64,
    pth_system_time: u64,
    pth_cpu_usage: i32,
    pth_policy: i32,
    pth_run_state: i32,
    pth_flags: i32,
    pth_sleep_time: i32,
    pth_curpri: i32,
    pth_priority: i32,
    pth_maxpriority: i32,
    pth_name: [u8; 64], // MAXTHREADNAMESIZE
}

const PROC_PIDTHREADINFO: c_int = 5;

/// List the system-wide thread ids belonging to `pid`. The TIDs come
/// out of the kernel as 64-bit values; downstream code in nerf
/// truncates to u32 to match the existing archive packet shape.
pub fn list_thread_ids(pid: u32) -> std::io::Result<Vec<u64>> {
    // Start with a generous buffer; resize if the kernel says it needs
    // more (proc_pidinfo returns the byte count it wants to write).
    let mut cap = 64usize;
    loop {
        let mut buf: Vec<u64> = vec![0; cap];
        let n = unsafe {
            proc_pidinfo(
                pid as c_int,
                PROC_PIDLISTTHREADS,
                0,
                buf.as_mut_ptr() as *mut c_void,
                (buf.len() * std::mem::size_of::<u64>()) as c_int,
            )
        };
        if n < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let bytes = n as usize;
        let count = bytes / std::mem::size_of::<u64>();
        // If the kernel filled the buffer exactly, it might have had
        // more to give -- grow and retry.
        if count == cap {
            cap *= 2;
            continue;
        }
        buf.truncate(count);
        return Ok(buf);
    }
}

/// Look up the name of a single thread via its system-wide tid.
pub fn thread_name(tid: u64) -> std::io::Result<Option<String>> {
    // `proc_pidinfo(PROC_PIDTHREADINFO)` takes the tid via `arg` and
    // ignores `pid` for the lookup; passing 0 works.
    let mut info: ProcThreadInfoC = unsafe { std::mem::zeroed() };
    let n = unsafe {
        proc_pidinfo(
            0,
            PROC_PIDTHREADINFO,
            tid,
            &mut info as *mut _ as *mut c_void,
            std::mem::size_of::<ProcThreadInfoC>() as c_int,
        )
    };
    if n <= 0 {
        return Err(std::io::Error::last_os_error());
    }
    let nul = info.pth_name.iter().position(|&b| b == 0).unwrap_or(info.pth_name.len());
    if nul == 0 {
        Ok(None)
    } else {
        Ok(Some(String::from_utf8_lossy(&info.pth_name[..nul]).into_owned()))
    }
}
