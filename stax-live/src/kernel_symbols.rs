use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use object::endian::Endianness;
use object::macho;
use object::read::macho::{
    LoadCommandVariant, MachHeader, Nlist, Section as MachSection, Segment as MachSegment,
};
use parking_lot::Mutex;

const BOOT_KERNEL_COLLECTION: &str =
    "/private/var/db/KernelExtensionManagement/KernelCollections/BootKernelCollection.kc";

#[derive(Default)]
pub(crate) struct KernelSymbolResolver {
    state: Mutex<KernelSymbolState>,
}

#[derive(Default)]
enum KernelSymbolState {
    #[default]
    Uninitialized,
    Ready(Arc<KernelSymbols>),
    Failed,
}

pub(crate) struct KernelResolvedSymbol {
    pub module: String,
    pub function_name: String,
    pub language: stax_demangle::Language,
}

#[derive(Debug)]
struct KernelSymbols {
    ranges: Vec<KernelImageRange>,
    functions_by_module: HashMap<String, Vec<KernelFunctionEntry>>,
    global_functions: Vec<KernelFunctionEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct KernelImageRange {
    module: String,
    start: u64,
    end: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct KernelFunctionEntry {
    module: String,
    address: u64,
    name: Option<String>,
}

impl KernelSymbolResolver {
    pub(crate) fn lookup(&self, address: u64) -> Option<KernelResolvedSymbol> {
        if !is_kernel_address(address) {
            return None;
        }

        let symbols = {
            let mut state = self.state.lock();
            match &*state {
                KernelSymbolState::Ready(symbols) => Some(Arc::clone(symbols)),
                KernelSymbolState::Failed => None,
                KernelSymbolState::Uninitialized => match KernelSymbols::load_system() {
                    Ok(symbols) => {
                        let symbols = Arc::new(symbols);
                        *state = KernelSymbolState::Ready(Arc::clone(&symbols));
                        Some(symbols)
                    }
                    Err(error) => {
                        tracing::debug!(%error, "failed to load kernel symbols");
                        *state = KernelSymbolState::Failed;
                        None
                    }
                },
            }
        }?;

        symbols.lookup(address)
    }
}

pub(crate) fn is_kernel_address(address: u64) -> bool {
    address >= 0xffff_0000_0000_0000
}

impl KernelSymbols {
    fn load_system() -> Result<Self, String> {
        let kernel_collection = ensure_boot_kernel_collection()?;
        let bytes = fs::read(&kernel_collection)
            .map_err(|e| format!("reading {}: {e}", kernel_collection.display()))?;
        let (ranges, symbols) = parse_kernel_collection(&bytes)?;
        Ok(Self::from_parts(ranges, symbols))
    }

    fn from_parts(ranges: Vec<KernelImageRange>, mut functions: Vec<KernelFunctionEntry>) -> Self {
        functions.sort_by_key(|function| function.address);

        let mut functions_by_module: HashMap<String, Vec<KernelFunctionEntry>> = HashMap::new();
        for function in &functions {
            functions_by_module
                .entry(function.module.clone())
                .or_default()
                .push(function.clone());
        }
        for entries in functions_by_module.values_mut() {
            entries.sort_by_key(|function| function.address);
        }

        let mut ranges = ranges;
        ranges.sort_by_key(|range| range.start);

        Self {
            ranges,
            functions_by_module,
            global_functions: functions,
        }
    }

    fn lookup(&self, address: u64) -> Option<KernelResolvedSymbol> {
        let module = self.module_for_address(address);
        let (entry, normalized_address) = match module {
            Some(module) => match self.functions_by_module.get(module) {
                Some(entries) => nearest_function_for_pc(entries, address)?,
                None => nearest_function_for_pc(&self.global_functions, address)?,
            },
            None => nearest_function_for_pc(&self.global_functions, address)?,
        };

        let offset = normalized_address.saturating_sub(entry.address);
        let (name, language) = match entry.name.as_deref() {
            Some(name) => {
                let demangled = stax_demangle::demangle_str(name);
                (demangled.name, demangled.language)
            }
            None => (
                format!("sub_{:016x}", entry.address),
                stax_demangle::Language::Unknown,
            ),
        };
        let function_name = format_symbol_with_offset(name, offset);

        Some(KernelResolvedSymbol {
            module: entry.module.clone(),
            function_name,
            language,
        })
    }

    fn module_for_address(&self, address: u64) -> Option<&str> {
        let index = self.ranges.partition_point(|range| range.start <= address);
        if index == 0 {
            return None;
        }
        let range = &self.ranges[index - 1];
        (address < range.end).then_some(range.module.as_str())
    }
}

type KernelMachHeader = macho::MachHeader64<Endianness>;
type KernelSymtabCommand = macho::SymtabCommand<Endianness>;
type KernelLinkeditCommand = macho::LinkeditDataCommand<Endianness>;

fn parse_kernel_collection(
    data: &[u8],
) -> Result<(Vec<KernelImageRange>, Vec<KernelFunctionEntry>), String> {
    let header = KernelMachHeader::parse(data, 0).map_err(format_object_error)?;
    let endian = header.endian().map_err(format_object_error)?;
    let mut commands = header
        .load_commands(endian, data, 0)
        .map_err(format_object_error)?;

    let mut ranges = Vec::new();
    let mut functions = Vec::new();

    while let Some(command) = commands.next().map_err(format_object_error)? {
        let LoadCommandVariant::FilesetEntry(entry) =
            command.variant().map_err(format_object_error)?
        else {
            continue;
        };
        let module = command
            .string(endian, entry.entry_id)
            .map_err(format_object_error)?;
        let module =
            std::str::from_utf8(module).map_err(|e| format!("invalid fileset entry id: {e}"))?;
        let fileoff = entry.fileoff.get(endian);
        parse_fileset_entry(data, fileoff, module, &mut ranges, &mut functions)?;
    }

    Ok((ranges, functions))
}

fn parse_fileset_entry(
    data: &[u8],
    header_offset: u64,
    module: &str,
    ranges: &mut Vec<KernelImageRange>,
    functions: &mut Vec<KernelFunctionEntry>,
) -> Result<(), String> {
    let header = KernelMachHeader::parse(data, header_offset).map_err(format_object_error)?;
    let endian = header.endian().map_err(format_object_error)?;
    let mut commands = header
        .load_commands(endian, data, header_offset)
        .map_err(format_object_error)?;
    let mut text_ranges = Vec::new();
    let mut text_section_indexes = Vec::new();
    let mut text_segment_addr = None;
    let mut section_index = 1u8;
    let mut symtab = None;
    let mut function_starts = None;

    while let Some(command) = commands.next().map_err(format_object_error)? {
        match command.variant().map_err(format_object_error)? {
            LoadCommandVariant::Segment64(segment, section_data) => {
                let start = segment.vmaddr.get(endian);
                let size = segment.vmsize.get(endian);
                let Some(end) = start.checked_add(size) else {
                    continue;
                };
                if segment.name() == b"__TEXT" {
                    text_segment_addr = Some(start);
                }
                if segment.name() == b"__TEXT_EXEC" {
                    let range = KernelImageRange {
                        module: module.to_owned(),
                        start,
                        end,
                    };
                    text_ranges.push((range.start, range.end));
                    ranges.push(range);
                }
                for section in segment
                    .sections(endian, section_data)
                    .map_err(format_object_error)?
                {
                    if section.segment_name() == b"__TEXT_EXEC" && section.name() == b"__text" {
                        text_section_indexes.push(section_index);
                    }
                    section_index = section_index.saturating_add(1);
                }
            }
            LoadCommandVariant::Symtab(command) => {
                symtab = Some(*command);
            }
            LoadCommandVariant::LinkeditData(command)
                if command.cmd.get(endian) == macho::LC_FUNCTION_STARTS =>
            {
                function_starts = Some(*command);
            }
            _ => {}
        }
    }

    let mut named_symbols = HashMap::new();
    if let Some(symtab) = symtab {
        named_symbols =
            parse_text_symbols(data, endian, &symtab, &text_section_indexes, &text_ranges)?;
    }
    let mut module_functions = match function_starts {
        Some(function_starts) => parse_function_starts(
            data,
            header_offset,
            endian,
            module,
            text_segment_addr.or_else(|| text_ranges.first().map(|&(start, _)| start)),
            &function_starts,
            &named_symbols,
        )
        .unwrap_or_else(|error| {
            tracing::debug!(%module, %error, "failed to parse kernel LC_FUNCTION_STARTS; falling back to text symbols");
            functions_from_symbols(module, &named_symbols)
        }),
        None => named_symbols
            .into_iter()
            .map(|(address, name)| KernelFunctionEntry {
                module: module.to_owned(),
                address,
                name: Some(name),
            })
            .collect(),
    };
    functions.append(&mut module_functions);

    Ok(())
}

fn parse_text_symbols(
    data: &[u8],
    endian: Endianness,
    symtab: &KernelSymtabCommand,
    text_section_indexes: &[u8],
    text_ranges: &[(u64, u64)],
) -> Result<HashMap<u64, String>, String> {
    let symbol_table = symtab
        .symbols::<KernelMachHeader, _>(endian, data)
        .map_err(format_object_error)?;
    let strings = symbol_table.strings();
    let mut symbols = HashMap::new();
    for symbol in symbol_table.iter() {
        if !symbol.is_definition() {
            continue;
        }
        let address = symbol.n_value(endian);
        if !text_section_indexes.contains(&symbol.n_sect())
            && !text_ranges
                .iter()
                .any(|&(start, end)| start <= address && address < end)
        {
            continue;
        }
        let name = symbol.name(endian, strings).map_err(format_object_error)?;
        if name.is_empty() {
            continue;
        }
        let Ok(name) = std::str::from_utf8(name) else {
            continue;
        };
        symbols.insert(address, name.to_owned());
    }
    Ok(symbols)
}

fn parse_function_starts(
    data: &[u8],
    header_offset: u64,
    endian: Endianness,
    module: &str,
    text_segment_addr: Option<u64>,
    function_starts: &KernelLinkeditCommand,
    named_symbols: &HashMap<u64, String>,
) -> Result<Vec<KernelFunctionEntry>, String> {
    let text_segment_addr = text_segment_addr
        .ok_or_else(|| format!("{module}: LC_FUNCTION_STARTS without a text segment"))?;
    let starts = collect_function_starts(data, endian, text_segment_addr, function_starts)
        .or_else(|absolute_error| {
            let entry_data = data
                .get(header_offset as usize..)
                .ok_or_else(|| format!("invalid fileset header offset {header_offset}"))?;
            collect_function_starts(entry_data, endian, text_segment_addr, function_starts)
                .map_err(|relative_error| format!("{absolute_error}; relative: {relative_error}"))
        })?;
    if starts.is_empty() {
        return Ok(functions_from_symbols(module, named_symbols));
    }
    Ok(starts
        .into_iter()
        .map(|address| KernelFunctionEntry {
            module: module.to_owned(),
            address,
            name: named_symbols.get(&address).cloned(),
        })
        .collect())
}

fn collect_function_starts(
    data: &[u8],
    endian: Endianness,
    text_segment_addr: u64,
    function_starts: &KernelLinkeditCommand,
) -> Result<Vec<u64>, String> {
    let starts = function_starts
        .function_starts(endian, data, text_segment_addr)
        .map_err(format_object_error)?;
    let mut functions = Vec::new();
    for start in starts {
        functions.push(start.map_err(format_object_error)?);
    }
    Ok(functions)
}

fn functions_from_symbols(
    module: &str,
    named_symbols: &HashMap<u64, String>,
) -> Vec<KernelFunctionEntry> {
    named_symbols
        .iter()
        .map(|(&address, name)| KernelFunctionEntry {
            module: module.to_owned(),
            address,
            name: Some(name.clone()),
        })
        .collect()
}

fn format_object_error(error: object::read::Error) -> String {
    error.to_string()
}

fn ensure_boot_kernel_collection() -> Result<PathBuf, String> {
    let path = Path::new(BOOT_KERNEL_COLLECTION);
    if path.is_file() {
        return Ok(path.to_owned());
    }

    let output = Command::new("kmutil")
        .arg("emit-macho")
        .output()
        .map_err(|e| format!("running kmutil emit-macho: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "kmutil emit-macho failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    if path.is_file() {
        Ok(path.to_owned())
    } else {
        Err(format!(
            "kmutil emit-macho succeeded but {BOOT_KERNEL_COLLECTION} does not exist"
        ))
    }
}

fn nearest_function_for_pc(
    functions: &[KernelFunctionEntry],
    address: u64,
) -> Option<(&KernelFunctionEntry, u64)> {
    let raw = nearest_function(functions, address)?;
    if raw.address == address
        && raw.name.is_none()
        && let Some(adjusted_address) = address.checked_sub(4)
        && let Some(adjusted) = nearest_function(functions, adjusted_address)
        && adjusted.address != raw.address
    {
        return Some((adjusted, adjusted_address));
    }
    Some((raw, address))
}

fn nearest_function(
    functions: &[KernelFunctionEntry],
    address: u64,
) -> Option<&KernelFunctionEntry> {
    let index = functions.partition_point(|function| function.address <= address);
    if index == 0 {
        return None;
    }
    functions.get(index - 1)
}

fn format_symbol_with_offset(name: String, offset: u64) -> String {
    if offset == 0 {
        name
    } else {
        format!("{name}+0x{offset:x}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_symbol_in_fileset_module() {
        let ranges = vec![KernelImageRange {
            module: "com.apple.kec.pthread".to_owned(),
            start: 0xfffffe000b66c420,
            end: 0xfffffe000b672020,
        }];
        let symbols = vec![
            KernelFunctionEntry {
                module: "com.apple.kec.pthread".to_owned(),
                address: 0xfffffe000b6709f0,
                name: Some("_psynch_cvupdate".to_owned()),
            },
            KernelFunctionEntry {
                module: "com.apple.kec.pthread".to_owned(),
                address: 0xfffffe000b670a3c,
                name: Some("_psynch_cvcontinue".to_owned()),
            },
            KernelFunctionEntry {
                module: "com.apple.kec.pthread".to_owned(),
                address: 0xfffffe000b670a80,
                name: Some("_psynch_cvbroad".to_owned()),
            },
            KernelFunctionEntry {
                module: "com.apple.kernel".to_owned(),
                address: 0xfffffe000b660000,
                name: Some("_wrong_fileset".to_owned()),
            },
        ];
        let symbols = KernelSymbols::from_parts(ranges, symbols);

        let exact = symbols.lookup(0xfffffe000b670a3c).unwrap();
        assert_eq!(exact.module, "com.apple.kec.pthread");
        assert_eq!(exact.function_name, "psynch_cvcontinue");

        let offset = symbols.lookup(0xfffffe000b670a40).unwrap();
        assert_eq!(offset.function_name, "psynch_cvcontinue+0x4");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn parses_emitted_boot_kernel_collection_when_available() {
        let path = Path::new(BOOT_KERNEL_COLLECTION);
        if !path.is_file() {
            return;
        }

        let data = fs::read(path).unwrap();
        let (ranges, symbols) = parse_kernel_collection(&data).unwrap();
        assert!(!ranges.is_empty());
        assert!(!symbols.is_empty());
        assert!(
            ranges
                .iter()
                .any(|range| range.module == "com.apple.kernel")
        );

        let symbols = KernelSymbols::from_parts(ranges, symbols);
        if let Some(resolved) = symbols.lookup(0xfffffe000b670a3c) {
            assert_eq!(resolved.module, "com.apple.kec.pthread");
            assert_eq!(resolved.function_name, "psynch_cvcontinue");
        }
    }
}
