//! `cargo xtask <subcommand>` — dev-only entrypoints.
//!
//! Available subcommands:
//!   - `install`        Build nperf in release mode, copy to ~/.cargo/bin/,
//!                      and (on macOS) ad-hoc codesign with the
//!                      `com.apple.security.cs.debugger` entitlement.
//!   - `build-daemon`   Build nperfd in release mode and print the
//!                      one-time `sudo cp` / `launchctl load` instructions
//!                      for the LaunchDaemon plist.
//!   - `build-broker`   Build nperf-task-broker in release mode, ad-hoc
//!                      codesign with cs.debugger, and print the
//!                      `cp` / `launchctl load` instructions for the
//!                      per-user LaunchAgent plist.

use std::env;
use std::error::Error;
use std::fs;
#[cfg(target_os = "macos")]
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

mod codegen;
mod migrate_archive;

const BIN_NAME: &str = "nperf";
const DAEMON_BIN: &str = "nperfd";
const BROKER_BIN: &str = "nperf-task-broker";

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = env::args().collect();
    let task = args.get(1).map(String::as_str).unwrap_or("");
    match task {
        "install" => install()?,
        "build-daemon" => build_daemon()?,
        "build-broker" => build_broker()?,
        "migrate-archives" => migrate_archive::run(&args[2..])?,
        "codegen" => codegen::run()?,
        "" | "help" | "--help" | "-h" => {
            print_usage();
        }
        other => {
            eprintln!("xtask: unknown subcommand {:?}", other);
            print_usage();
            std::process::exit(1);
        }
    }
    Ok(())
}

fn print_usage() {
    eprintln!("Usage: cargo xtask <subcommand>");
    eprintln!();
    eprintln!("Subcommands:");
    eprintln!(
        "  install              Build {bin} (release), copy to ~/.cargo/bin/{bin}, codesign on macOS",
        bin = BIN_NAME
    );
    eprintln!(
        "  build-daemon         Build {bin} (release) and print install instructions",
        bin = DAEMON_BIN
    );
    eprintln!(
        "  build-broker         Build {bin} (release), codesign with cs.debugger, print install instructions",
        bin = BROKER_BIN
    );
    eprintln!(
        "  migrate-archives     Rewrite v1 .nperf archives in place to v2 (one-shot fixture migration)"
    );
    eprintln!(
        "  codegen              Generate TypeScript bindings for nperf-live into frontend/src/generated/"
    );
}

fn install() -> Result<(), Box<dyn Error>> {
    let workspace_root = workspace_root();
    let cargo_bin = cargo_bin_dir()?;
    fs::create_dir_all(&cargo_bin)?;

    // Build all three: the user-facing CLI plus the trinity binaries.
    // Both the CLI and the broker need cs.debugger; the daemon doesn't
    // (it runs under launchd as root, no entitlement needed).
    for bin in [BIN_NAME, DAEMON_BIN, BROKER_BIN] {
        println!(":: Building {bin} (release)...");
        cargo_build_release(&workspace_root, bin)?;

        let src = workspace_root.join("target").join("release").join(bin);
        if !src.exists() {
            return Err(format!("expected built binary at {} but it wasn't there", src.display()).into());
        }
        let dst = cargo_bin.join(bin);
        println!(":: Copying {} -> {}", src.display(), dst.display());
        fs::copy(&src, &dst)?;

        #[cfg(target_os = "macos")]
        if bin == BIN_NAME {
            codesign_macos(&dst)?;
        } else if bin == BROKER_BIN {
            let entitlements = workspace_root.join(BROKER_BIN).join("entitlements.plist");
            codesign_with_entitlements(&dst, &entitlements)?;
        }
        // DAEMON_BIN: no codesign — runs as root under launchd, no
        // entitlement needed; gets installed to /usr/local/bin by
        // `sudo nperf setup`.
    }

    println!();
    println!(":: Installed three binaries to {}.", cargo_bin.display());
    println!(":: To enable the trinity (sudo-less profiling, framehop,");
    println!(":: future arg-capture), run:");
    println!();
    println!("     sudo nperf setup");
    println!();
    println!(":: This installs nperfd as a LaunchDaemon under /usr/local/bin/");
    println!(":: and /Library/LaunchDaemons/. After that, the same nperf CLI");
    println!(":: works without sudo via `--mac-backend daemon`.");
    Ok(())
}

#[cfg(target_os = "macos")]
fn codesign_macos(binary: &Path) -> Result<(), Box<dyn Error>> {
    // - cs.debugger: lets nperf attach to other processes
    // - get-task-allow: lets debuggers attach to nperf itself
    // - cs.allow-jit: required for any JIT mmap (kept for completeness)
    // - cs.allow-unsigned-executable-memory: cranelift-jit (used by vox)
    //   uses plain mprotect-PROT_EXEC rather than MAP_JIT, which the
    //   kernel rejects under hardened runtime + allow-jit alone. The
    //   broader entitlement accepts non-MAP_JIT executable pages.
    const ENTITLEMENTS_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>com.apple.security.cs.debugger</key>
	<true/>
	<key>com.apple.security.get-task-allow</key>
	<true/>
	<key>com.apple.security.cs.allow-jit</key>
	<true/>
	<key>com.apple.security.cs.allow-unsigned-executable-memory</key>
	<true/>
</dict>
</plist>
"#;

    let mut entitlements_path = env::temp_dir();
    entitlements_path.push(format!("nperf-xtask-entitlements-{}.xml", std::process::id()));
    fs::write(&entitlements_path, ENTITLEMENTS_XML)?;

    println!(
        ":: Codesigning {} with com.apple.security.cs.debugger entitlement...",
        binary.display()
    );
    let status = Command::new("codesign")
        .arg("--force")
        .arg("--options")
        .arg("runtime")
        .arg("--sign")
        .arg("-")
        .arg("--entitlements")
        .arg(&entitlements_path)
        .arg(binary)
        .status()?;

    let _ = fs::remove_file(&entitlements_path);

    if !status.success() {
        return Err(format!("codesign exited with {status}").into());
    }
    Ok(())
}

fn build_daemon() -> Result<(), Box<dyn Error>> {
    let workspace_root = workspace_root();

    println!(":: Building {DAEMON_BIN} (release)...");
    cargo_build_release(&workspace_root, DAEMON_BIN)?;

    let binary = workspace_root.join("target").join("release").join(DAEMON_BIN);
    let plist = workspace_root
        .join(DAEMON_BIN)
        .join("launchd")
        .join("eu.bearcove.nperfd.plist");
    println!();
    println!(":: Built {}", binary.display());
    println!();
    println!(":: To install (one-time, requires sudo):");
    println!("     sudo cp {} /usr/local/bin/", binary.display());
    println!(
        "     sudo cp {} /Library/LaunchDaemons/",
        plist.display()
    );
    println!(
        "     sudo launchctl load /Library/LaunchDaemons/eu.bearcove.nperfd.plist"
    );
    println!();
    println!(":: After install, the daemon listens on /var/run/nperfd.sock.");
    println!(":: Logs at /var/log/nperfd.log.");
    Ok(())
}

fn build_broker() -> Result<(), Box<dyn Error>> {
    let workspace_root = workspace_root();

    println!(":: Building {BROKER_BIN} (release)...");
    cargo_build_release(&workspace_root, BROKER_BIN)?;

    let binary = workspace_root.join("target").join("release").join(BROKER_BIN);
    let entitlements = workspace_root.join(BROKER_BIN).join("entitlements.plist");

    #[cfg(target_os = "macos")]
    codesign_with_entitlements(&binary, &entitlements)?;

    let plist = workspace_root
        .join(BROKER_BIN)
        .join("launchd")
        .join("eu.bearcove.nperf.task-broker.plist");
    println!();
    println!(":: Built + codesigned {}", binary.display());
    println!();
    println!(":: To install (per-user, no sudo needed for the LaunchAgent):");
    println!("     sudo cp {} /usr/local/bin/", binary.display());
    println!("     mkdir -p ~/Library/LaunchAgents");
    println!("     cp {} ~/Library/LaunchAgents/", plist.display());
    println!("     launchctl load ~/Library/LaunchAgents/eu.bearcove.nperf.task-broker.plist");
    println!();
    println!(":: After install, the broker registers as eu.bearcove.nperf.task-broker.");
    println!(":: Logs at /tmp/nperf-task-broker.log.");
    Ok(())
}

fn cargo_build_release(workspace_root: &Path, package: &str) -> Result<(), Box<dyn Error>> {
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(&cargo)
        .args(["build", "--release", "-p", package])
        .current_dir(workspace_root)
        .status()?;
    if !status.success() {
        return Err(format!("cargo build -p {package} failed: {status}").into());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn codesign_with_entitlements(binary: &Path, entitlements: &Path) -> Result<(), Box<dyn Error>> {
    println!(
        ":: Codesigning {} with entitlements from {}...",
        binary.display(),
        entitlements.display()
    );
    let status = Command::new("codesign")
        .arg("--force")
        .arg("--options")
        .arg("runtime")
        .arg("--sign")
        .arg("-")
        .arg("--entitlements")
        .arg(entitlements)
        .arg(binary)
        .status()?;
    if !status.success() {
        return Err(format!("codesign exited with {status}").into());
    }
    Ok(())
}

fn cargo_bin_dir() -> Result<PathBuf, Box<dyn Error>> {
    if let Some(cargo_home) = env::var_os("CARGO_HOME") {
        return Ok(PathBuf::from(cargo_home).join("bin"));
    }
    let home = env::var_os("HOME").ok_or("HOME is not set")?;
    Ok(PathBuf::from(home).join(".cargo").join("bin"))
}

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is the xtask crate's directory; its parent is the
    // workspace root.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask crate has a parent directory")
        .to_path_buf()
}
