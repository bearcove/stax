//! v1 → v2 archive converter.
//!
//! `cargo xtask migrate-archives <path> [<path>...]` rewrites each
//! file in place from `ARCHIVE_VERSION = 1` to `ARCHIVE_VERSION = 2`.
//!
//! The format change (commit d1d9d2f) was schema-only:
//!   - bumped version u32 (1 → 2)
//!   - added `Platform` to `MachineInfo`
//!   - moved `is_shared_object` and `debuglink` out of `BinaryInfo` and
//!     into `BinaryInfo.format = BinaryFormat::Elf { ... }`
//!   - renamed `SymbolTable` → `ElfSymbolTable` (same payload)
//!   - dropped two deprecated variants (`Deprecated_BinaryMap` and
//!     `Deprecated_BinaryUnmap`); their packets are skipped during conversion
//!   - dropped a v0 → v1 DwarfReg compat hack on read; v1 archives never
//!     carry the 0xff01 register so this is a no-op for v1→v2
//!   - added `MachOSymbolTable` (only emitted by future macOS captures)
//!
//! Both schemas are spelled out inline so this tool has no other deps
//! beyond `speedy`.

use std::borrow::Cow;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};

use speedy::{Context, Endianness, Readable, Reader, Writable, Writer};

const ARCHIVE_MAGIC: u32 = 0x4652504E;

// -------------------------------- Shared types ----------------------------

#[derive(Copy, Clone, Debug, Readable, Writable)]
struct Inode {
    inode: u64,
    dev_major: u32,
    dev_minor: u32,
}

#[derive(Copy, Clone, Debug, Readable, Writable)]
enum Bitness {
    B32,
    B64,
}

#[derive(Clone, Debug)]
struct UserFrame {
    address: u64,
    initial_address: Option<u64>,
}

impl<'a, C: Context> Readable<'a, C> for UserFrame {
    fn read_from<R: Reader<'a, C>>(reader: &mut R) -> Result<Self, C::Error> {
        let address = reader.read_u64()?;
        let initial = reader.read_u64()?;
        Ok(UserFrame {
            address,
            initial_address: if initial == 0 { None } else { Some(initial) },
        })
    }
}

impl<'a, C: Context> Writable<C> for UserFrame {
    fn write_to<T: ?Sized + Writer<C>>(&self, writer: &mut T) -> Result<(), C::Error> {
        writer.write_u64(self.address)?;
        writer.write_u64(self.initial_address.unwrap_or(0))?;
        Ok(())
    }
}

#[derive(Clone, Debug, Readable, Writable)]
struct LoadHeader {
    address: u64,
    file_offset: u64,
    file_size: u64,
    memory_size: u64,
    alignment: u64,
    is_readable: bool,
    is_writable: bool,
    is_executable: bool,
}

#[derive(Copy, Clone, Debug, Readable, Writable)]
struct DwarfReg {
    register: u16,
    value: u64,
}

#[derive(Debug, Readable, Writable)]
enum ContextSwitchKind {
    In,
    OutWhileIdle,
    OutWhileRunning,
}

// -------------------------------- v1 schema -------------------------------

mod v1 {
    use super::*;

    #[allow(non_camel_case_types)]
    #[derive(Debug, Readable, Writable)]
    pub enum Packet<'a> {
        Header {
            magic: u32,
            version: u32,
        },
        MachineInfo {
            cpu_count: u32,
            bitness: Bitness,
            endianness: Endianness,
            architecture: Cow<'a, str>,
        },
        ProcessInfo {
            pid: u32,
            executable: Cow<'a, [u8]>,
            binary_id: Inode,
        },
        Sample {
            timestamp: u64,
            pid: u32,
            tid: u32,
            cpu: u32,
            kernel_backtrace: Cow<'a, [u64]>,
            user_backtrace: Cow<'a, [UserFrame]>,
        },
        BinaryInfo {
            inode: Inode,
            is_shared_object: bool,
            symbol_table_count: u16,
            path: Cow<'a, [u8]>,
            debuglink: Cow<'a, [u8]>,
            #[speedy(default_on_eof)]
            load_headers: Cow<'a, [LoadHeader]>,
        },
        StringTable {
            inode: Inode,
            offset: u64,
            data: Cow<'a, [u8]>,
            #[speedy(default_on_eof)]
            path: Cow<'a, [u8]>,
        },
        SymbolTable {
            inode: Inode,
            offset: u64,
            string_table_offset: u64,
            is_dynamic: bool,
            data: Cow<'a, [u8]>,
            #[speedy(default_on_eof)]
            path: Cow<'a, [u8]>,
        },
        FileBlob {
            path: Cow<'a, [u8]>,
            data: Cow<'a, [u8]>,
        },
        RawSample {
            timestamp: u64,
            pid: u32,
            tid: u32,
            cpu: u32,
            kernel_backtrace: Cow<'a, [u64]>,
            stack: Vec<u8>,
            regs: Cow<'a, [DwarfReg]>,
        },
        BinaryBlob {
            inode: Inode,
            path: Cow<'a, [u8]>,
            data: Cow<'a, [u8]>,
        },
        ThreadName {
            pid: u32,
            tid: u32,
            name: Cow<'a, [u8]>,
        },
        MemoryRegionMap {
            pid: u32,
            range: Range<u64>,
            is_read: bool,
            is_write: bool,
            is_executable: bool,
            is_shared: bool,
            file_offset: u64,
            inode: u64,
            major: u32,
            minor: u32,
            name: Cow<'a, [u8]>,
        },
        MemoryRegionUnmap {
            pid: u32,
            range: Range<u64>,
        },
        // Deprecated, dropped in v2; we keep the variants here so the
        // index 13 / 14 slots line up with v1 archives that emitted them.
        Deprecated_BinaryMap {
            pid: u32,
            inode: Inode,
            base_address: u64,
        },
        Deprecated_BinaryUnmap {
            pid: u32,
            inode: Inode,
            base_address: u64,
        },
        Lost {
            count: u64,
        },
        BuildId {
            inode: Inode,
            build_id: Vec<u8>,
            #[speedy(default_on_eof)]
            path: Cow<'a, [u8]>,
        },
        BinaryLoaded {
            pid: u32,
            inode: Option<Inode>,
            name: Cow<'a, [u8]>,
        },
        BinaryUnloaded {
            pid: u32,
            inode: Option<Inode>,
            name: Cow<'a, [u8]>,
        },
        ProfilingFrequency {
            frequency: u32,
        },
        ContextSwitch {
            pid: u32,
            cpu: u32,
            kind: ContextSwitchKind,
        },
    }
}

// -------------------------------- v2 schema -------------------------------

mod v2 {
    use super::*;

    #[derive(Copy, Clone, Debug, Readable, Writable)]
    pub enum Platform {
        Linux,
        MacOS,
    }

    #[derive(Debug, Readable, Writable)]
    pub enum BinaryFormat<'a> {
        Elf {
            is_shared_object: bool,
            debuglink: Cow<'a, [u8]>,
        },
        MachO,
    }

    #[derive(Clone, Debug, Readable, Writable)]
    pub struct MachOSymbolEntry {
        pub start_svma: u64,
        pub end_svma: u64,
        pub name: Vec<u8>,
    }

    #[derive(Debug, Readable, Writable)]
    pub enum Packet<'a> {
        Header {
            magic: u32,
            version: u32,
        },
        MachineInfo {
            cpu_count: u32,
            bitness: Bitness,
            endianness: Endianness,
            architecture: Cow<'a, str>,
            platform: Platform,
        },
        ProcessInfo {
            pid: u32,
            executable: Cow<'a, [u8]>,
            binary_id: Inode,
        },
        Sample {
            timestamp: u64,
            pid: u32,
            tid: u32,
            cpu: u32,
            kernel_backtrace: Cow<'a, [u64]>,
            user_backtrace: Cow<'a, [UserFrame]>,
        },
        BinaryInfo {
            inode: Inode,
            symbol_table_count: u16,
            path: Cow<'a, [u8]>,
            load_headers: Cow<'a, [LoadHeader]>,
            format: BinaryFormat<'a>,
        },
        StringTable {
            inode: Inode,
            offset: u64,
            data: Cow<'a, [u8]>,
            path: Cow<'a, [u8]>,
        },
        ElfSymbolTable {
            inode: Inode,
            offset: u64,
            string_table_offset: u64,
            is_dynamic: bool,
            data: Cow<'a, [u8]>,
            path: Cow<'a, [u8]>,
        },
        FileBlob {
            path: Cow<'a, [u8]>,
            data: Cow<'a, [u8]>,
        },
        RawSample {
            timestamp: u64,
            pid: u32,
            tid: u32,
            cpu: u32,
            kernel_backtrace: Cow<'a, [u64]>,
            stack: Vec<u8>,
            regs: Cow<'a, [DwarfReg]>,
        },
        BinaryBlob {
            inode: Inode,
            path: Cow<'a, [u8]>,
            data: Cow<'a, [u8]>,
        },
        ThreadName {
            pid: u32,
            tid: u32,
            name: Cow<'a, [u8]>,
        },
        MemoryRegionMap {
            pid: u32,
            range: Range<u64>,
            is_read: bool,
            is_write: bool,
            is_executable: bool,
            is_shared: bool,
            file_offset: u64,
            inode: u64,
            major: u32,
            minor: u32,
            name: Cow<'a, [u8]>,
        },
        MemoryRegionUnmap {
            pid: u32,
            range: Range<u64>,
        },
        Lost {
            count: u64,
        },
        BuildId {
            inode: Inode,
            build_id: Vec<u8>,
            path: Cow<'a, [u8]>,
        },
        BinaryLoaded {
            pid: u32,
            inode: Option<Inode>,
            name: Cow<'a, [u8]>,
        },
        BinaryUnloaded {
            pid: u32,
            inode: Option<Inode>,
            name: Cow<'a, [u8]>,
        },
        ProfilingFrequency {
            frequency: u32,
        },
        ContextSwitch {
            pid: u32,
            cpu: u32,
            kind: ContextSwitchKind,
        },
        MachOSymbolTable {
            inode: Inode,
            path: Cow<'a, [u8]>,
            text_svma: u64,
            entries: Vec<MachOSymbolEntry>,
        },
    }
}

// ------------------------------ conversion --------------------------------

#[derive(Default)]
struct LegacyMaps {
    /// `(inode, dev_major, dev_minor) -> base_address` from
    /// `Deprecated_BinaryMap` packets. The v1 reader used these to synthesize
    /// executable region mappings whenever `BinaryInfo.load_headers` was
    /// empty (which it always is on the older fixtures).
    base_addr_by_inode: std::collections::HashMap<(u64, u32, u32), u64>,
    /// `(inode, dev_major, dev_minor) -> [memory region]`, gathered from
    /// `MemoryRegionMap` packets so we can reconstruct load headers.
    regions_by_inode: std::collections::HashMap<(u64, u32, u32), Vec<Range<u64>>>,
}

fn to_v2(p: v1::Packet<'static>, legacy: &LegacyMaps) -> Option<v2::Packet<'static>> {
    use v1::Packet as P1;
    use v2::Packet as P2;
    Some(match p {
        P1::Header { magic, version: _ } => P2::Header { magic, version: 2 },
        P1::MachineInfo { cpu_count, bitness, endianness, architecture } => P2::MachineInfo {
            cpu_count,
            bitness,
            endianness,
            architecture,
            platform: v2::Platform::Linux,
        },
        P1::ProcessInfo { pid, executable, binary_id } => {
            P2::ProcessInfo { pid, executable, binary_id }
        }
        P1::Sample { timestamp, pid, tid, cpu, kernel_backtrace, user_backtrace } => P2::Sample {
            timestamp,
            pid,
            tid,
            cpu,
            kernel_backtrace,
            user_backtrace,
        },
        P1::BinaryInfo { inode, is_shared_object, symbol_table_count, path, debuglink, load_headers } => {
            // If the original archive carried no load_headers (the field had
            // `default_on_eof` in v1, so older captures never wrote any),
            // synthesize one per memory region using the legacy BinaryMap
            // base address. This mirrors the back-compat shim that used to
            // live in `data_reader.rs`. Without it, v2 readers can't
            // translate runtime VAs to relative addresses and online-mode
            // symbol lookup degrades to "?:libname".
            let load_headers: Cow<'static, [LoadHeader]> = if load_headers.is_empty() {
                let key = (inode.inode, inode.dev_major, inode.dev_minor);
                let base_addr = legacy.base_addr_by_inode.get(&key).copied();
                let regions = legacy.regions_by_inode.get(&key);
                match (base_addr, regions) {
                    (Some(base), Some(regions)) => {
                        let synth: Vec<LoadHeader> = regions
                            .iter()
                            .map(|r| {
                                let size = r.end - r.start;
                                LoadHeader {
                                    address: r.start.saturating_sub(base),
                                    file_offset: 0,
                                    file_size: size,
                                    memory_size: size,
                                    alignment: 1,
                                    is_readable: true,
                                    is_writable: false,
                                    is_executable: true,
                                }
                            })
                            .collect();
                        Cow::Owned(synth)
                    }
                    _ => load_headers,
                }
            } else {
                load_headers
            };
            P2::BinaryInfo {
                inode,
                symbol_table_count,
                path,
                load_headers,
                format: v2::BinaryFormat::Elf { is_shared_object, debuglink },
            }
        }
        P1::StringTable { inode, offset, data, path } => {
            P2::StringTable { inode, offset, data, path }
        }
        P1::SymbolTable { inode, offset, string_table_offset, is_dynamic, data, path } => {
            P2::ElfSymbolTable { inode, offset, string_table_offset, is_dynamic, data, path }
        }
        P1::FileBlob { path, data } => P2::FileBlob { path, data },
        P1::RawSample { timestamp, pid, tid, cpu, kernel_backtrace, stack, regs } => {
            // The v1 reader had a compat shim that remapped DwarfReg.register
            // 0xff01 -> 16 for old AMD64 captures. v2 dropped that shim, so
            // bake the remap into the migration here.
            let regs: Vec<DwarfReg> = regs
                .iter()
                .map(|r| DwarfReg {
                    register: if r.register == 0xff01 { 16 } else { r.register },
                    value: r.value,
                })
                .collect();
            P2::RawSample {
                timestamp,
                pid,
                tid,
                cpu,
                kernel_backtrace,
                stack,
                regs: Cow::Owned(regs),
            }
        }
        P1::BinaryBlob { inode, path, data } => P2::BinaryBlob { inode, path, data },
        P1::ThreadName { pid, tid, name } => P2::ThreadName { pid, tid, name },
        P1::MemoryRegionMap {
            pid, range, is_read, is_write, is_executable, is_shared,
            file_offset, inode, major, minor, name,
        } => P2::MemoryRegionMap {
            pid, range, is_read, is_write, is_executable, is_shared,
            file_offset, inode, major, minor, name,
        },
        P1::MemoryRegionUnmap { pid, range } => P2::MemoryRegionUnmap { pid, range },
        P1::Deprecated_BinaryMap { .. } | P1::Deprecated_BinaryUnmap { .. } => return None,
        P1::Lost { count } => P2::Lost { count },
        P1::BuildId { inode, build_id, path } => P2::BuildId { inode, build_id, path },
        P1::BinaryLoaded { pid, inode, name } => P2::BinaryLoaded { pid, inode, name },
        P1::BinaryUnloaded { pid, inode, name } => P2::BinaryUnloaded { pid, inode, name },
        P1::ProfilingFrequency { frequency } => P2::ProfilingFrequency { frequency },
        P1::ContextSwitch { pid, cpu, kind } => P2::ContextSwitch { pid, cpu, kind },
    })
}

// ------------------------------ framed I/O --------------------------------
//
// The on-disk layout is `[u32 length][bytes of packet]` repeated until EOF.
// We read each frame as a length-prefixed Vec<u8>, decode as a v1 packet,
// then re-encode as a v2 packet and write with a fresh length prefix.

fn read_frame<R: Read>(r: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    match r.read(&mut len_buf)? {
        0 => return Ok(None),
        n if n < 4 => {
            r.read_exact(&mut len_buf[n..])?;
        }
        _ => {}
    }
    let length = u32::from_le_bytes(len_buf) as usize;
    let mut buf = vec![0u8; length];
    r.read_exact(&mut buf)?;
    Ok(Some(buf))
}

fn write_frame<W: Write>(w: &mut W, bytes: &[u8]) -> io::Result<()> {
    w.write_all(&(bytes.len() as u32).to_le_bytes())?;
    w.write_all(bytes)?;
    Ok(())
}

pub fn migrate(input: &Path, output: &Path) -> Result<(usize, usize, usize), Box<dyn std::error::Error>> {
    // Pass 1: read all packets into memory, build the legacy maps so we can
    // back-fill BinaryInfo.load_headers in pass 2.
    let in_file = File::open(input)?;
    let mut reader = BufReader::new(in_file);

    let mut packets: Vec<v1::Packet<'static>> = Vec::new();
    let mut legacy = LegacyMaps::default();
    let mut header_seen = false;
    let mut frame_index = 0usize;

    while let Some(frame_bytes) = read_frame(&mut reader)? {
        frame_index += 1;
        let v1_packet: v1::Packet<'static> = Readable::read_from_buffer_copying_data(&frame_bytes)
            .map_err(|e| format!("v1 packet decode failed at frame {}: {}", frame_index, e))?;

        if frame_index == 1 {
            if let v1::Packet::Header { magic, version } = &v1_packet {
                if *magic != ARCHIVE_MAGIC {
                    return Err(format!("not an nperf archive (magic 0x{:08x})", magic).into());
                }
                if *version != 1 {
                    return Err(format!("expected version 1 archive, found {}", version).into());
                }
                header_seen = true;
            } else {
                return Err("first frame is not a Header packet".into());
            }
        }

        match &v1_packet {
            v1::Packet::Deprecated_BinaryMap { inode, base_address, .. } => {
                legacy.base_addr_by_inode.insert(
                    (inode.inode, inode.dev_major, inode.dev_minor),
                    *base_address,
                );
            }
            v1::Packet::MemoryRegionMap { range, inode, major, minor, is_executable, .. } => {
                if *is_executable && (*inode != 0 || *major != 0 || *minor != 0) {
                    legacy.regions_by_inode
                        .entry((*inode, *major, *minor))
                        .or_default()
                        .push(range.clone());
                }
            }
            _ => {}
        }
        packets.push(v1_packet);
    }

    if !header_seen {
        return Err("empty archive".into());
    }

    // Pass 2: convert and write.
    let out_file = File::create(output)?;
    let mut writer = BufWriter::new(out_file);

    let total = packets.len();
    let mut written = 0usize;
    let mut dropped = 0usize;

    for v1_packet in packets {
        match to_v2(v1_packet, &legacy) {
            Some(v2_packet) => {
                let bytes = v2_packet.write_to_vec()?;
                write_frame(&mut writer, &bytes)?;
                written += 1;
            }
            None => {
                dropped += 1;
            }
        }
    }

    writer.flush()?;
    Ok((total, written, dropped))
}

pub fn run(paths: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    if paths.is_empty() {
        return Err("usage: cargo xtask migrate-archives <file> [file ...]".into());
    }
    for p in paths {
        let path = PathBuf::from(p);
        let tmp = path.with_extension("nperf.v2.tmp");
        match migrate(&path, &tmp) {
            Ok((total, written, dropped)) => {
                fs::rename(&tmp, &path)?;
                println!(
                    "  {} -> v2 ({} frames in, {} written, {} dropped)",
                    path.display(),
                    total,
                    written,
                    dropped
                );
            }
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                eprintln!("  {}: {}", path.display(), e);
            }
        }
    }
    Ok(())
}
