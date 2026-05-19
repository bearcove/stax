//! `stax setup` (Linux only).
//!
//! Two modes, dispatched on euid at runtime:
//!
//!   * **non-root**: explain the install path. `cargo xtask install`
//!     builds and stages the user binaries; the privileged daemon is
//!     a one-time `sudo stax setup`.
//!
//!   * **root** (`sudo stax setup`): install `staxd` as a systemd
//!     service. Copies `~$SUDO_USER/.cargo/bin/staxd` to
//!     `/usr/local/bin/staxd`, drops the unit into
//!     `/etc/systemd/system/`, and `systemctl enable --now`s it.
//!     After this, `stax record …` runs without sudo because the
//!     privileged `perf_event_open` happens in `staxd`.
//!
//! The Linux counterpart of `cmd_setup_mac` — same shape, no codesign,
//! systemd instead of launchd.

use std::env;
use std::error::Error;
use std::ffi::OsStr;
use std::fs;
use std::io::{self};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;

use crate::args;

/// systemd unit installed at `/etc/systemd/system/`. Embedded verbatim
/// so a freshly-staged `stax` doesn't need the source tree at install
/// time. The canonical version on disk is
/// `staxd/systemd/eu.bearcove.staxd.service`; if you change one,
/// update both.
const STAXD_SYSTEMD_UNIT: &str = r#"[Unit]
Description=stax privileged perf fd broker (staxd)
Documentation=https://github.com/bearcove/stax
After=network.target

[Service]
Type=simple
# perf_event_open system-wide needs privilege. Running as root is the
# simplest correct answer; CAP_PERFMON (5.8+) / CAP_SYS_ADMIN +
# CAP_SYS_PTRACE are listed so a hardened deployment can instead drop
# to a dedicated unprivileged user with just these ambient caps.
User=root
AmbientCapabilities=CAP_PERFMON CAP_SYS_ADMIN CAP_SYS_PTRACE
ExecStart=/usr/local/bin/staxd --socket /run/staxd.sock
Restart=always
RestartSec=1
Environment=RUST_LOG=staxd=info,stax_linux_capture=info,stax_vox_observe=info
# Logs to the journal: journalctl -u eu.bearcove.staxd -f
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=multi-user.target
"#;

const UNIT_PATH: &str = "/etc/systemd/system/eu.bearcove.staxd.service";
const BINARY_INSTALL_PATH: &str = "/usr/local/bin/staxd";
const SYSTEMD_UNIT: &str = "eu.bearcove.staxd";

pub fn main(args: args::SetupArgs) -> Result<(), Box<dyn Error>> {
    if is_root() {
        install_daemon(&args)
    } else {
        explain_install()
    }
}

fn is_root() -> bool {
    // SAFETY: geteuid is always-safe on Unix.
    unsafe { libc::geteuid() == 0 }
}

// ---------------------------------------------------------------------------
// Non-root: explain
// ---------------------------------------------------------------------------

fn explain_install() -> Result<(), Box<dyn Error>> {
    println!("`stax setup` (no sudo) is a no-op.");
    println!();
    println!("Build + stage the binaries with:");
    println!();
    println!("    cargo xtask install");
    println!();
    println!("Then, one-time only, install the privileged daemon:");
    println!();
    println!("    sudo stax setup");
    println!();
    println!("After that `stax record …` runs without sudo: the");
    println!("privileged perf_event_open happens in the staxd service.");
    Ok(())
}

// ---------------------------------------------------------------------------
// Root: install staxd as a systemd service
// ---------------------------------------------------------------------------

fn install_daemon(args: &args::SetupArgs) -> Result<(), Box<dyn Error>> {
    let staged = locate_staged_daemon()?;
    println!(":: found staged daemon at {}", staged.display());

    if !args.yes {
        println!(
            r#"
This will install staxd as a systemd service (runs as root, does the
privileged perf_event_open and brokers the fds to unprivileged stax).

Steps:
  1. Copy {} -> {}
  2. Write {}
  3. systemctl daemon-reload
  4. systemctl enable --now {}

After install, `stax record …` works without sudo.

Press Enter to continue, or Ctrl-C to cancel."#,
            staged.display(),
            BINARY_INSTALL_PATH,
            UNIT_PATH,
            SYSTEMD_UNIT,
        );
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
    }

    println!(":: copying binary -> {BINARY_INSTALL_PATH}");
    fs::copy(&staged, BINARY_INSTALL_PATH)
        .map_err(|err| format!("copying staxd to {BINARY_INSTALL_PATH}: {err}"))?;
    fs::set_permissions(BINARY_INSTALL_PATH, fs::Permissions::from_mode(0o755))?;

    println!(":: writing systemd unit -> {UNIT_PATH}");
    fs::write(UNIT_PATH, STAXD_SYSTEMD_UNIT).map_err(|err| format!("writing {UNIT_PATH}: {err}"))?;
    fs::set_permissions(UNIT_PATH, fs::Permissions::from_mode(0o644))?;

    println!(":: systemctl daemon-reload");
    run_systemctl(&["daemon-reload"])?;

    println!(":: systemctl enable --now {SYSTEMD_UNIT}");
    run_systemctl(&["enable", "--now", SYSTEMD_UNIT])?;

    println!();
    println!(":: staxd installed and running.");
    println!(":: socket : /run/staxd.sock");
    println!(":: logs   : journalctl -u {SYSTEMD_UNIT} -f");
    println!(":: status : systemctl status {SYSTEMD_UNIT}");
    println!(":: now    : stax record --serve 127.0.0.1:8080 -- /bin/foo");
    Ok(())
}

fn run_systemctl(systemctl_args: &[&str]) -> Result<(), Box<dyn Error>> {
    let status = Command::new("systemctl").args(systemctl_args).status()?;
    if !status.success() {
        return Err(format!(
            "`systemctl {}` exited with {status}",
            systemctl_args.join(" ")
        )
        .into());
    }
    Ok(())
}

/// Find `staxd` to install. `~$SUDO_USER/.cargo/bin/staxd` (where
/// `cargo xtask install` dropped it as the normal user), then root's
/// own `~/.cargo/bin/staxd`.
fn locate_staged_daemon() -> Result<PathBuf, Box<dyn Error>> {
    let mut candidates: Vec<PathBuf> = Vec::new();

    if let Some(user_home) = sudo_user_home() {
        candidates.push(user_home.join(".cargo").join("bin").join("staxd"));
    }
    if let Some(home) = env::var_os("HOME") {
        candidates.push(PathBuf::from(home).join(".cargo").join("bin").join("staxd"));
    }

    for c in &candidates {
        if c.exists() {
            return Ok(c.clone());
        }
    }
    Err(format!(
        "couldn't find a staged `staxd` binary. Looked in:\n{}\n\
         Run `cargo xtask install` first (as your normal user, not under sudo).",
        candidates
            .iter()
            .map(|p| format!("  - {}", p.display()))
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .into())
}

/// When invoked via `sudo`, $SUDO_USER carries the original username.
/// Resolve their home via getpwnam_r (no `/home/foo` hardcoding).
fn sudo_user_home() -> Option<PathBuf> {
    let user = env::var_os("SUDO_USER")?;
    home_dir_for_user(user.as_os_str())
}

fn home_dir_for_user(name: &OsStr) -> Option<PathBuf> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c_name = CString::new(name.as_bytes()).ok()?;

    let mut buf = vec![0u8; 4096];
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result_ptr: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: getpwnam_r writes into pwd / buf and sets *result_ptr.
    let rc = unsafe {
        libc::getpwnam_r(
            c_name.as_ptr(),
            &mut pwd,
            buf.as_mut_ptr() as *mut _,
            buf.len(),
            &mut result_ptr,
        )
    };
    if rc != 0 || result_ptr.is_null() {
        return None;
    }
    // SAFETY: pw_dir is a NUL-terminated C string owned by `buf` while
    // `result_ptr` is non-null.
    let dir = unsafe { std::ffi::CStr::from_ptr(pwd.pw_dir) };
    Some(PathBuf::from(std::ffi::OsStr::from_bytes(dir.to_bytes())))
}
