//! `nperf annotate` — disassembles hot functions with per-instruction sample
//! counts (perf-annotate style). x86_64 only for now.
//!
//! Address-space discipline. Two virtual-address spaces show up here and they
//! must not be confused:
//!
//! * [`AbsoluteAddr`] — a runtime VA. What `sample.user_backtrace[i].address`
//!   carries, what `/proc/<pid>/maps` lists, what shows up next to JIT'd
//!   code. Equal to the program counter the kernel saw.
//!
//! * [`RelativeAddr`] — a binary-internal VA. What an ELF symbol table's
//!   `st_value` holds, what `LoadHeader::address` reports, what
//!   `Frame::relative_address` returns. For a non-PIE executable this
//!   coincides with the absolute address; for a PIE/DSO it differs by the
//!   per-mapping load offset.
//!
//! Native-code bookkeeping is done entirely in `RelativeAddr` (counts, range,
//! disassembly base) so that we never have to track per-process load offsets
//! for libraries shared across mappings. JIT'd code has no relative space —
//! its addresses live wherever the JIT mmap'd them — so JIT counts stay in
//! `AbsoluteAddr`.

use std::collections::{BTreeMap, HashMap};
use std::error::Error;
use std::fmt::Write as FmtWrite;
use std::io::{self, Write};
use std::ops::Range;
use std::sync::Arc;

use nwind::{BinaryData, BinaryId, Symbols};
use yaxpeax_arch::LengthedInstruction;
use yaxpeax_x86::amd64::InstDecoder;

use crate::args::AnnotateArgs;
use crate::data_reader::{
    Binary, EventKind, read_data, repack_cli_args,
};

#[derive(Copy, Clone, Hash, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct AbsoluteAddr( u64 );

#[derive(Copy, Clone, Hash, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct RelativeAddr( u64 );

impl RelativeAddr {
    fn raw( self ) -> u64 { self.0 }
}

impl AbsoluteAddr {
    fn raw( self ) -> u64 { self.0 }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
enum FuncSourceTag {
    Native( BinaryId ),
    Jit
}

type FuncKey = (FuncSourceTag, String);

enum FuncRecord {
    Native {
        binary_id: BinaryId,
        range: Range< RelativeAddr >,
        counts: BTreeMap< RelativeAddr, u64 >,
        total: u64
    },
    Jit {
        range: Range< AbsoluteAddr >,
        counts: BTreeMap< AbsoluteAddr, u64 >,
        total: u64
    }
}

impl FuncRecord {
    fn total( &self ) -> u64 {
        match self {
            FuncRecord::Native { total, .. } | FuncRecord::Jit { total, .. } => *total
        }
    }
}

/// Per-binary cache. Holds whatever `BinaryData` we managed to obtain (for
/// fetching code bytes) and a sorted list of `(relative_range, demangled_name)`
/// pairs (for mapping a sampled relative address to its enclosing function).
///
/// Sources tried, in order:
/// * `binary.data()` — the binary if the recorder embedded it as a `BinaryBlob`.
/// * `BinaryData::load_from_fs(binary.path())` — the binary on disk. Useful
///   for system libraries (libc.so.6, libm.so.6 …) that the recorder doesn't
///   embed but that are usually still present on the host.
/// * `binary.debug_data()` — the auto-loaded debug companion (e.g. the
///   `.build-id`-keyed `.debug` file). Only used as a symbol-range source —
///   debug files don't have the actual code bytes.
struct BinaryArchive {
    code: Option< Arc< BinaryData > >,
    symbols: Vec< (Range< RelativeAddr >, String) >
}

impl BinaryArchive {
    fn lookup( &self, addr: RelativeAddr ) -> Option< (Range< RelativeAddr >, &str) > {
        let idx = self.symbols.partition_point( |(range, _)| range.start <= addr );
        if idx == 0 {
            return None;
        }
        let (range, name) = &self.symbols[ idx - 1 ];
        if range.start <= addr && addr < range.end {
            Some( (range.clone(), name.as_str()) )
        } else {
            None
        }
    }
}

struct ArchiveCache {
    by_binary: HashMap< BinaryId, BinaryArchive >
}

impl ArchiveCache {
    fn new() -> Self {
        ArchiveCache { by_binary: HashMap::new() }
    }

    fn get_or_load( &mut self, binary_id: &BinaryId, binary: &Binary ) -> &BinaryArchive {
        if !self.by_binary.contains_key( binary_id ) {
            let archive = build_archive( binary );
            self.by_binary.insert( binary_id.clone(), archive );
        }
        self.by_binary.get( binary_id ).unwrap()
    }
}

fn build_archive( binary: &Binary ) -> BinaryArchive {
    let code: Option< Arc< BinaryData > > = binary.data().cloned()
        .or_else( || {
            let path = binary.path();
            // Skip pseudo-paths like "[vdso]", "[heap]"; load_from_fs would
            // just fail on those anyway, but the failure produces noise.
            if path.starts_with( '[' ) {
                None
            } else {
                match BinaryData::load_from_fs( path ) {
                    Ok( data ) => Some( Arc::new( data ) ),
                    Err( err ) => {
                        debug!( "annotate: could not open '{}' from disk: {}", path, err );
                        None
                    }
                }
            }
        });

    // For symbol ranges, prefer the binary that actually has a populated
    // .symtab. That's usually the debug file when one exists (system libs
    // are stripped); otherwise the code binary we just resolved.
    let symbol_source = binary.debug_data().cloned().or_else( || code.clone() );

    let mut symbols: Vec< (Range< RelativeAddr >, String) > = Vec::new();
    if let Some( src ) = symbol_source {
        Symbols::each_from_binary_data( &src, |range, name| {
            let demangled = rustc_demangle::demangle( name ).to_string();
            symbols.push((
                Range { start: RelativeAddr( range.start ), end: RelativeAddr( range.end ) },
                demangled
            ));
        });
    }
    symbols.sort_by_key( |(range, _)| range.start );

    BinaryArchive { code, symbols }
}

fn format_hex_bytes( bytes: &[u8] ) -> String {
    let mut out = String::with_capacity( bytes.len() * 3 );
    for (i, byte) in bytes.iter().enumerate() {
        if i > 0 {
            out.push( ' ' );
        }
        let _ = write!( &mut out, "{:02x}", byte );
    }
    out
}

/// Locate the slice of file bytes corresponding to a binary-relative range,
/// using the executable PT_LOAD segment that contains it.
fn fetch_code_bytes< 'a >( data: &'a BinaryData, range: &Range< RelativeAddr > ) -> Option< &'a [u8] > {
    let start = range.start.raw();
    let end = range.end.raw();
    let len = (end - start) as usize;
    for header in data.load_headers() {
        if !header.is_executable {
            continue;
        }
        let segment_end = header.address + header.memory_size;
        if header.address <= start && end <= segment_end {
            let in_segment = start - header.address;
            if in_segment + (len as u64) > header.file_size {
                return None;
            }
            let file_off = (header.file_offset + in_segment) as usize;
            let bytes = data.as_bytes();
            if file_off.checked_add( len )? > bytes.len() {
                return None;
            }
            return Some( &bytes[ file_off..file_off + len ] );
        }
    }
    None
}

fn disassemble_amd64< W: Write, A: Copy + Eq + Ord + Into<u64> >(
    decoder: &InstDecoder,
    bytes: &[u8],
    base: A,
    counts: &BTreeMap< A, u64 >,
    out: &mut W
) -> io::Result< () >
where
    A: From<u64>,
{
    let base_u64: u64 = base.into();
    let mut offset: usize = 0;
    while offset < bytes.len() {
        let cursor: A = (base_u64 + offset as u64).into();
        let cursor_u64 = base_u64 + offset as u64;
        match decoder.decode_slice( &bytes[ offset.. ] ) {
            Ok( instr ) => {
                let len = instr.len().to_const() as usize;
                let end = (offset + len).min( bytes.len() );
                let count = counts.get( &cursor ).copied().unwrap_or( 0 );
                let mark = if count > 0 { ">" } else { " " };
                let hex = format_hex_bytes( &bytes[ offset..end ] );
                writeln!( out, " {} {:>8}  0x{:012x}  {:<30}  {}",
                          mark, count, cursor_u64, hex, instr )?;
                offset = end;
            }
            Err( err ) => {
                let count = counts.get( &cursor ).copied().unwrap_or( 0 );
                let mark = if count > 0 { ">" } else { " " };
                writeln!( out, " {} {:>8}  0x{:012x}  {:<30}  <decode error: {}>",
                          mark, count, cursor_u64, format!( "{:02x}", bytes[ offset ] ), err )?;
                offset += 1;
            }
        }
    }
    Ok(())
}

impl From<u64> for AbsoluteAddr { fn from(v: u64) -> Self { AbsoluteAddr(v) } }
impl From<AbsoluteAddr> for u64 { fn from(v: AbsoluteAddr) -> Self { v.0 } }
impl From<u64> for RelativeAddr { fn from(v: u64) -> Self { RelativeAddr(v) } }
impl From<RelativeAddr> for u64 { fn from(v: RelativeAddr) -> Self { v.0 } }

pub fn main( args: AnnotateArgs ) -> Result< (), Box< dyn Error > > {
    // Parse the jitdump up front (if any), capturing the actual code bytes
    // alongside the per-record VA. State::jitdump_names already remembers the
    // VA->name mapping, but throws the bytes away — we need them here.
    let jit_code: HashMap< AbsoluteAddr, Vec< u8 > > = if let Some( path ) = args.collation_args.jitdump.as_ref() {
        let dump = crate::jitdump::JitDump::load( std::path::Path::new( path ) )
            .map_err( |err| format!( "failed to open jitdump {:?}: {}", path, err ) )?;
        let mut map = HashMap::new();
        for record in dump.records {
            if let crate::jitdump::Record::CodeLoad { virtual_address, code, .. } = record {
                map.insert( AbsoluteAddr( virtual_address ), code.into_owned() );
            }
        }
        map
    } else {
        HashMap::new()
    };

    let (_, read_data_args) = repack_cli_args( &args.collation_args );

    let mut archives = ArchiveCache::new();
    let mut funcs: HashMap< FuncKey, FuncRecord > = HashMap::new();

    let state = read_data( read_data_args, |event| {
        let sample = match event.kind {
            EventKind::Sample( s ) => s,
            _ => return
        };
        let leaf = match sample.user_backtrace.first() {
            Some( f ) => f,
            None => return
        };
        let leaf_va = AbsoluteAddr( leaf.address );

        // Native? Use address_space to get the symbol name and relative
        // address — this matches collate's resolution path (and works even
        // when `binary.data` is None because the symbols were loaded via
        // SymbolTable packets rather than a full BinaryBlob).
        if let Some( region ) = sample.process.memory_regions().get_value( leaf_va.raw() ) {
            let binary_id: BinaryId = region.into();
            let binary = event.state.get_binary( &binary_id );

            let mut resolved: Option< (String, RelativeAddr) > = None;
            sample.process.address_space().decode_symbol_while( leaf_va.raw(), &mut |frame| {
                if frame.is_inline {
                    return true;
                }
                let name = frame.demangled_name.take()
                    .or_else( || frame.name.take() )
                    .map( |n| n.into_owned() );
                if let Some( name ) = name {
                    resolved = Some( (name, RelativeAddr( frame.relative_address )) );
                }
                false
            });

            let (name, rel_addr) = match resolved {
                Some( v ) => v,
                None => return
            };

            let archive = archives.get_or_load( &binary_id, binary );
            let range = archive.lookup( rel_addr )
                .map( |(r, _)| r )
                .unwrap_or( Range { start: rel_addr, end: RelativeAddr( rel_addr.raw() + 1 ) } );

            let key: FuncKey = (FuncSourceTag::Native( binary_id.clone() ), name);
            let entry = funcs.entry( key ).or_insert_with( || FuncRecord::Native {
                binary_id,
                range,
                counts: BTreeMap::new(),
                total: 0
            });
            if let FuncRecord::Native { counts, total, .. } = entry {
                *counts.entry( rel_addr ).or_insert( 0 ) += 1;
                *total += 1;
            }
            return;
        }

        // JIT? Look up by absolute VA in the jitdump_names range map.
        if let Some( idx ) = event.state.jitdump_names().get_index( leaf_va.raw() ) {
            let (range, name) = event.state.jitdump_names().get_by_index( idx ).unwrap();
            let key: FuncKey = (FuncSourceTag::Jit, name.clone());
            let abs_range = Range {
                start: AbsoluteAddr( range.start ),
                end: AbsoluteAddr( range.end )
            };
            let entry = funcs.entry( key ).or_insert_with( || FuncRecord::Jit {
                range: abs_range,
                counts: BTreeMap::new(),
                total: 0
            });
            if let FuncRecord::Jit { counts, total, .. } = entry {
                *counts.entry( leaf_va ).or_insert( 0 ) += 1;
                *total += 1;
            }
        }
    })?;

    let arch = state.architecture();
    if arch != "amd64" {
        return Err( format!(
            "annotate: only x86_64 (amd64) is supported in this version (got '{}')",
            arch
        ).into() );
    }

    if funcs.is_empty() {
        eprintln!( "annotate: no samples landed in known functions" );
        return Ok(());
    }

    let mut chosen: Vec< (FuncKey, FuncRecord) > = if args.function.is_empty() {
        let mut v: Vec< _ > = funcs.into_iter().collect();
        v.sort_by( |a, b| b.1.total().cmp( &a.1.total() ) );
        v.truncate( args.top.max( 1 ) );
        v
    } else {
        funcs.into_iter()
            .filter( |(k, _)| args.function.iter().any( |needle| k.1.contains( needle ) ) )
            .collect()
    };
    chosen.sort_by( |a, b| b.1.total().cmp( &a.1.total() ) );

    if chosen.is_empty() {
        eprintln!( "annotate: no functions matched the --function filter" );
        return Ok(());
    }

    let stdout = io::stdout();
    let mut out = stdout.lock();
    let decoder = InstDecoder::default();

    for ((_tag, name), record) in chosen {
        match record {
            FuncRecord::Native { binary_id, range, counts, total } => {
                let binary = state.get_binary( &binary_id );
                let label = binary.basename();
                if range.end.raw() <= range.start.raw() + 1 {
                    writeln!( out, "==== {} [{}]  total={}  (no symbol-table range; cannot disassemble) ====\n",
                              name, label, total )?;
                    continue;
                }
                let archive = archives.get_or_load( &binary_id, binary );
                let bytes = match archive.code.as_ref().and_then( |data| fetch_code_bytes( data, &range ) ) {
                    Some( b ) => b,
                    None => {
                        writeln!( out, "==== {} [{}]  rel 0x{:x}..0x{:x}  total={}  (no code bytes available) ====\n",
                                  name, label, range.start.raw(), range.end.raw(), total )?;
                        continue;
                    }
                };
                writeln!( out, "==== {} [{}]  rel 0x{:x}..0x{:x}  total={} samples ====",
                          name, label, range.start.raw(), range.end.raw(), total )?;
                writeln!( out, "      count   address       bytes                           asm" )?;
                disassemble_amd64( &decoder, bytes, range.start, &counts, &mut out )?;
            }
            FuncRecord::Jit { range, counts, total } => {
                let bytes = match jit_code.get( &range.start ) {
                    Some( b ) => b.as_slice(),
                    None => {
                        writeln!( out, "==== {} [JIT]  range 0x{:x}..0x{:x}  total={}  (no jitdump code bytes; pass --jitdump?) ====\n",
                                  name, range.start.raw(), range.end.raw(), total )?;
                        continue;
                    }
                };
                writeln!( out, "==== {} [JIT]  range 0x{:x}..0x{:x}  total={} samples ====",
                          name, range.start.raw(), range.end.raw(), total )?;
                writeln!( out, "      count   address       bytes                           asm" )?;
                disassemble_amd64( &decoder, bytes, range.start, &counts, &mut out )?;
            }
        }
        writeln!( out )?;
    }

    Ok(())
}
