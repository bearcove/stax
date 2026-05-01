//! Source-line resolution + on-disk source caching for live annotate.
//!
//! Given the SVMA of an instruction inside a `CodeImage`, returns the
//! best-effort `(file, line)` from `symbolic` debug info plus a
//! highlighted snippet of the source line if we can find it on disk.

use std::collections::HashMap;
use std::sync::Arc;

use symbolic::debuginfo::Object;

use crate::binaries::CodeImage;
use crate::highlight::TokenHighlighter;
use stax_live_proto::Token;

pub struct SourceResolver {
    /// Cached line tables keyed by binary path. Failed builds are
    /// remembered as `None` so we do not keep reparsing them.
    line_tables: HashMap<String, Option<LineTable>>,
    /// Source-file contents (lines) keyed by absolute path. `None` is
    /// "tried and failed" (don't keep stat'ing).
    sources: HashMap<String, Option<Arc<Vec<String>>>>,
    /// Highlighter reused across resolves; arborium grammars are
    /// expensive to instantiate per-call.
    hl: TokenHighlighter,
}

#[derive(Clone, Debug)]
struct LineRecord {
    start: u64,
    end: Option<u64>,
    file: String,
    line: u32,
}

struct LineTable {
    lines: Vec<LineRecord>,
}

impl LineTable {
    fn build(bytes: Arc<Vec<u8>>) -> Option<Self> {
        let object = Object::parse(bytes.as_slice()).ok()?;
        let session = object.debug_session().ok()?;
        let mut lines = Vec::new();

        for function in session.functions() {
            let function = function.ok()?;
            let compilation_dir = String::from_utf8_lossy(function.compilation_dir).into_owned();
            for line in function.lines {
                let line_no = match u32::try_from(line.line).ok() {
                    Some(0) | None => continue,
                    Some(line_no) => line_no,
                };
                let file = join_compilation_dir(&compilation_dir, &line.file.path_str());
                let end = line
                    .size
                    .and_then(|size| line.address.checked_add(size))
                    .filter(|end| *end > line.address);
                lines.push(LineRecord {
                    start: line.address,
                    end,
                    file,
                    line: line_no,
                });
            }
        }

        if lines.is_empty() {
            return None;
        }

        lines.sort_by_key(|line| line.start);
        Some(Self { lines })
    }

    fn find_location(&self, address: u64) -> Option<(String, u32)> {
        let idx = match self.lines.binary_search_by_key(&address, |line| line.start) {
            Ok(idx) => idx,
            Err(0) => return None,
            Err(idx) => idx - 1,
        };
        let line = self.lines.get(idx)?;
        let end = line
            .end
            .or_else(|| self.lines.get(idx + 1).map(|next| next.start));
        if let Some(end) = end
            && address >= end
        {
            return None;
        }
        Some((line.file.clone(), line.line))
    }
}

fn join_compilation_dir(compilation_dir: &str, file: &str) -> String {
    let path = std::path::Path::new(file);
    if path.is_absolute() {
        return file.to_owned();
    }
    if compilation_dir.is_empty() {
        return file.to_owned();
    }
    std::path::Path::new(compilation_dir)
        .join(path)
        .to_string_lossy()
        .into_owned()
}

impl SourceResolver {
    pub fn new() -> Self {
        Self {
            line_tables: HashMap::new(),
            sources: HashMap::new(),
            hl: TokenHighlighter::new_for_source(),
        }
    }

    /// Look up the (file, line) for `svma` inside `image`. Returns
    /// `None` if the binary has no usable debug line info.
    pub fn locate(
        &mut self,
        binary_path: &str,
        image: &Arc<CodeImage>,
        svma: u64,
    ) -> Option<(String, u32)> {
        if !self.line_tables.contains_key(binary_path) {
            let table = LineTable::build(image.bytes.clone());
            if table.is_none() {
                tracing::debug!("source: no symbolic line info for {}", binary_path);
            }
            self.line_tables.insert(binary_path.to_owned(), table);
        }
        self.line_tables.get(binary_path)?.as_ref()?.find_location(svma)
    }

    /// Highlighted snippet for `(file, line_1based)`. Returns an empty
    /// vector when the file isn't loadable from disk.
    pub fn snippet(&mut self, file: &str, line: u32) -> Vec<Token> {
        let lines = self.source_lines(file);
        let raw = match lines
            .as_ref()
            .and_then(|v| v.get(line.saturating_sub(1) as usize))
        {
            Some(s) => s.trim().to_owned(),
            None => return Vec::new(),
        };
        let lang = arborium::detect_language(file).unwrap_or("rust");
        self.hl.highlight_in(lang, &raw)
    }

    fn source_lines(&mut self, file: &str) -> Option<Arc<Vec<String>>> {
        if let Some(entry) = self.sources.get(file) {
            return entry.clone();
        }
        let loaded = read_source(file).map(|s| Arc::new(s.lines().map(str::to_owned).collect()));
        self.sources.insert(file.to_owned(), loaded.clone());
        loaded
    }
}

/// Read a source file, with rust-src remapping. The rustc compiler stamps
/// std/core/alloc paths into DWARF as `/rustc/<commit>/library/...` —
/// those don't exist on the user's box, but the rust-src component does
/// (under `<sysroot>/lib/rustlib/src/rust/library/...`), so we try that
/// translation as a fallback.
fn read_source(file: &str) -> Option<String> {
    if let Ok(s) = std::fs::read_to_string(file) {
        return Some(s);
    }
    if let Some(rs_path) = rust_src_translate(file) {
        if let Ok(s) = std::fs::read_to_string(&rs_path) {
            tracing::debug!(
                "source: {} translated to rust-src path {}",
                file,
                rs_path.display()
            );
            return Some(s);
        }
    }
    None
}

fn rust_src_translate(file: &str) -> Option<std::path::PathBuf> {
    let rest = file.strip_prefix("/rustc/")?;
    let (_commit, rel) = rest.split_once('/')?;
    let sysroot = rust_sysroot()?;
    Some(sysroot.join("lib/rustlib/src/rust").join(rel))
}

fn rust_sysroot() -> Option<&'static std::path::Path> {
    use std::sync::OnceLock;
    static SYSROOT: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();
    SYSROOT
        .get_or_init(|| {
            std::process::Command::new("rustc")
                .arg("--print")
                .arg("sysroot")
                .output()
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| std::path::PathBuf::from(s.trim()))
        })
        .as_deref()
}

impl Default for SourceResolver {
    fn default() -> Self {
        Self::new()
    }
}
