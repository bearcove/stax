use nwind::UserFrame;

pub struct SampleEvent< 'a > {
    pub timestamp: u64,
    pub pid: u32,
    pub tid: u32,
    pub cpu: u32,
    pub kernel_backtrace: &'a [u64],
    pub user_backtrace: &'a [UserFrame],
}

/// One symbol from a binary's symbol table (Mach-O `nlist_64` or ELF
/// symtab/dynsym). Addresses are SVMAs (binary-relative; same space as
/// `BinaryLoadedEvent::text_svma`).
pub struct LiveSymbol< 'a > {
    pub start_svma: u64,
    pub end_svma: u64,
    /// Raw, possibly mangled, possibly non-UTF-8 symbol bytes.
    pub name: &'a [u8],
}

pub struct BinaryLoadedEvent< 'a > {
    /// Filesystem path the dynamic loader resolved this image to (or
    /// the dyld cache install-name on macOS for system dylibs).
    pub path: &'a str,
    /// Runtime base address (AVMA) where the image's text segment was
    /// mapped.
    pub base_avma: u64,
    /// Size of the text segment.
    pub vmsize: u64,
    /// SVMA of the text segment in the on-disk binary, i.e. the address
    /// the linker laid out symbols against. `slide = base_avma - text_svma`.
    pub text_svma: u64,
    /// Architecture identifier matching `archive::Packet::MachineInfo`
    /// (e.g. "aarch64", "amd64"). Used to pick the disassembler.
    pub arch: Option< &'a str >,
    pub symbols: &'a [LiveSymbol< 'a >],
}

pub struct BinaryUnloadedEvent< 'a > {
    pub path: &'a str,
    pub base_avma: u64,
}

pub trait LiveSink: Send + Sync {
    fn on_sample( &self, event: &SampleEvent );

    /// A new image was mapped into the target process.
    #[allow(unused_variables)]
    fn on_binary_loaded( &self, event: &BinaryLoadedEvent ) {}

    /// A previously-loaded image was unmapped.
    #[allow(unused_variables)]
    fn on_binary_unloaded( &self, event: &BinaryUnloadedEvent ) {}
}
