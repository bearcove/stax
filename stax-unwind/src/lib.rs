use std::borrow::Cow;
use std::convert::TryInto;
use std::ops::Range;
use std::sync::Arc;

use framehop::{
    CacheNative, ExplicitModuleSectionInfo, MayAllocateDuringUnwind, Module, UnwindRegsNative,
    Unwinder, UnwinderNative,
};
use object::{Object, ObjectSection, ObjectSegment};

type SectionBytes = Arc<[u8]>;
type NativeModule = Module<SectionBytes>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapturedImageMapping {
    pub start: u64,
    pub end: u64,
    pub file_offset: u64,
    pub is_read: bool,
    pub is_write: bool,
    pub is_executable: bool,
    pub path: String,
}

impl CapturedImageMapping {
    pub fn executable_text(
        path: impl Into<String>,
        start: u64,
        size: u64,
        file_offset: u64,
    ) -> Self {
        Self {
            start,
            end: start.saturating_add(size),
            file_offset,
            is_read: true,
            is_write: false,
            is_executable: true,
            path: path.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapturedLoadFailure {
    pub path: String,
    pub error: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CapturedReload {
    pub mapped_regions: usize,
    pub loaded_binaries: usize,
    pub load_failures: Vec<CapturedLoadFailure>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnwindFailure {
    MissingInstructionPointer,
    NullInstructionPointer,
    NoBinary,
    NoUnwindInfo,
    MissingCfa,
    MissingCfaRegister,
    CfaExpressionFailed,
    RegisterMemoryReadFailed,
    RegisterExpressionFailed,
    UnsupportedRegisterRule,
    MissingReturnAddress,
    FramePointerUnavailable,
    FramePointerOutsideStack,
    FramePointerReadFailed,
    ArmUnwindInfoMissing,
    ArmUnwindFailed,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UnwindMode {
    #[default]
    Default,
    DwarfOnly,
    CompactOnly,
    CompactWithDwarfRefs,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapturedUnwindError {
    NoMappings,
    NoMappedRegions,
    MissingStackPointer,
    MissingInstructionPointer,
    EmptyStack,
    OnlyLeafFrame { reason: Option<UnwindFailure> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CapturedThreadState {
    pub pc: u64,
    pub lr: u64,
    pub fp: u64,
    pub sp: u64,
}

impl CapturedThreadState {
    pub fn new(pc: u64, lr: u64, fp: u64, sp: u64) -> Self {
        Self {
            pc: strip_code_pointer(pc),
            lr: strip_code_pointer(lr),
            fp: strip_data_pointer(fp),
            sp: strip_data_pointer(sp),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CapturedStack<'a> {
    pub base: u64,
    pub bytes: &'a [u8],
}

impl<'a> CapturedStack<'a> {
    pub fn new(base: u64, bytes: &'a [u8]) -> Self {
        Self {
            base: strip_data_pointer(base),
            bytes,
        }
    }

    fn read_u64(&self, address: u64) -> Option<u64> {
        let offset = address.checked_sub(self.base)? as usize;
        let end = offset.checked_add(8)?;
        let bytes = self.bytes.get(offset..end)?;
        Some(u64::from_le_bytes(bytes.try_into().ok()?))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserFrame {
    pub address: u64,
    pub initial_address: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapturedBridgePolicy {
    Never,
    AnyOnlyLeaf,
    OnlyNoBinaryOrNoUnwindInfo,
}

impl CapturedBridgePolicy {
    fn should_bridge(self, error: &CapturedUnwindError) -> bool {
        match self {
            Self::Never => false,
            Self::AnyOnlyLeaf => matches!(error, CapturedUnwindError::OnlyLeafFrame { .. }),
            Self::OnlyNoBinaryOrNoUnwindInfo => matches!(
                error,
                CapturedUnwindError::OnlyLeafFrame {
                    reason: Some(UnwindFailure::NoBinary | UnwindFailure::NoUnwindInfo)
                }
            ),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CapturedUnwindOptions {
    pub mode: UnwindMode,
    pub bridge: CapturedBridgePolicy,
    pub max_frames: usize,
}

impl CapturedUnwindOptions {
    pub const DEFAULT_MAX_FRAMES: usize = 64;

    pub fn metadata(mode: UnwindMode) -> Self {
        Self {
            mode,
            bridge: CapturedBridgePolicy::AnyOnlyLeaf,
            max_frames: Self::DEFAULT_MAX_FRAMES,
        }
    }

    pub fn dwarf_with_fp_bridge() -> Self {
        Self {
            mode: UnwindMode::Default,
            bridge: CapturedBridgePolicy::OnlyNoBinaryOrNoUnwindInfo,
            max_frames: Self::DEFAULT_MAX_FRAMES,
        }
    }
}

impl Default for CapturedUnwindOptions {
    fn default() -> Self {
        Self {
            mode: UnwindMode::Default,
            bridge: CapturedBridgePolicy::Never,
            max_frames: Self::DEFAULT_MAX_FRAMES,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapturedUnwindOutcome {
    pub callers: Vec<u64>,
    pub bridge_attempted: bool,
    pub bridge_steps: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapturedUnwindFailure {
    pub error: CapturedUnwindError,
    pub bridge_attempted: bool,
    pub bridge_steps: usize,
}

#[derive(Clone, Copy, Debug)]
struct ModuleSupport {
    start: u64,
    end: u64,
    has_unwind_info: bool,
}

struct FramehopLane {
    unwinder: UnwinderNative<SectionBytes, MayAllocateDuringUnwind>,
    cache: CacheNative<MayAllocateDuringUnwind>,
    modules: Vec<ModuleSupport>,
}

impl Default for FramehopLane {
    fn default() -> Self {
        Self {
            unwinder: UnwinderNative::new(),
            cache: CacheNative::new(),
            modules: Vec::new(),
        }
    }
}

impl FramehopLane {
    fn new() -> Self {
        Self::default()
    }

    fn add_module(&mut self, module: NativeModule, support: ModuleSupport) {
        self.unwinder.add_module(module);
        self.modules.push(support);
    }

    fn hint_for(&self, address: u64) -> Option<UnwindFailure> {
        let module = self
            .modules
            .iter()
            .find(|module| module.start <= address && address < module.end)?;
        if module.has_unwind_info {
            None
        } else {
            Some(UnwindFailure::NoUnwindInfo)
        }
    }
}

pub struct CapturedStackUnwinder {
    compact_lane: FramehopLane,
    rich_lane: FramehopLane,
    mappings: Vec<CapturedImageMapping>,
    dirty: bool,
    last_reload: CapturedReload,
}

impl CapturedStackUnwinder {
    pub fn new() -> Self {
        Self {
            compact_lane: FramehopLane::new(),
            rich_lane: FramehopLane::new(),
            mappings: Vec::new(),
            dirty: false,
            last_reload: CapturedReload::default(),
        }
    }

    pub fn set_mappings(&mut self, mappings: impl IntoIterator<Item = CapturedImageMapping>) {
        self.mappings = mappings.into_iter().collect();
        self.dirty = true;
    }

    pub fn add_mapping(&mut self, mapping: CapturedImageMapping) {
        self.mappings
            .retain(|existing| existing.start != mapping.start || existing.path != mapping.path);
        self.mappings.push(mapping);
        self.dirty = true;
    }

    pub fn remove_mapping_by_start(&mut self, start: u64) {
        let old_len = self.mappings.len();
        self.mappings.retain(|mapping| mapping.start != start);
        if self.mappings.len() != old_len {
            self.dirty = true;
        }
    }

    pub fn last_reload(&self) -> &CapturedReload {
        &self.last_reload
    }

    pub fn reload_if_dirty(&mut self) -> &CapturedReload {
        if !self.dirty {
            return &self.last_reload;
        }

        let mut compact_lane = FramehopLane::new();
        let mut rich_lane = FramehopLane::new();
        let mut mapped_regions = 0usize;
        let mut loaded_binaries = 0usize;
        let mut load_failures = Vec::new();

        for mapping in self
            .mappings
            .iter()
            .filter(|mapping| mapping.start < mapping.end && !mapping.path.is_empty())
        {
            mapped_regions += 1;

            let bytes = match std::fs::read(&mapping.path) {
                Ok(bytes) => {
                    loaded_binaries += 1;
                    Arc::new(bytes)
                }
                Err(error) => {
                    load_failures.push(CapturedLoadFailure {
                        path: mapping.path.clone(),
                        error: error.to_string(),
                    });
                    continue;
                }
            };

            match build_modules(mapping, bytes) {
                Ok(loaded) => {
                    compact_lane.add_module(loaded.compact_module, loaded.compact_support);
                    rich_lane.add_module(loaded.rich_module, loaded.rich_support);
                }
                Err(error) => load_failures.push(CapturedLoadFailure {
                    path: mapping.path.clone(),
                    error,
                }),
            }
        }

        self.compact_lane = compact_lane;
        self.rich_lane = rich_lane;
        self.last_reload = CapturedReload {
            mapped_regions,
            loaded_binaries,
            load_failures,
        };
        self.dirty = false;
        &self.last_reload
    }

    pub fn unwind_callers(
        &mut self,
        state: CapturedThreadState,
        stack: CapturedStack<'_>,
        scratch: &mut Vec<UserFrame>,
        options: CapturedUnwindOptions,
    ) -> Result<CapturedUnwindOutcome, CapturedUnwindFailure> {
        match self.unwind_callers_once(state, stack, scratch, options.mode, options.max_frames) {
            Ok(callers) => Ok(CapturedUnwindOutcome {
                callers,
                bridge_attempted: false,
                bridge_steps: 0,
            }),
            Err(error) if !options.bridge.should_bridge(&error) => Err(CapturedUnwindFailure {
                error,
                bridge_attempted: false,
                bridge_steps: 0,
            }),
            Err(error) => {
                let mut last_error = error;
                let mut bridge_steps = 0usize;
                let mut bridge_prefix = Vec::with_capacity(options.max_frames);
                let mut fp = strip_data_pointer(state.fp);
                let mut sp = strip_data_pointer(state.sp);

                while bridge_steps < options.max_frames {
                    let Some(next_state) = fp_bridge_step(stack, fp, sp) else {
                        return Err(CapturedUnwindFailure {
                            error: last_error,
                            bridge_attempted: true,
                            bridge_steps,
                        });
                    };

                    bridge_steps += 1;
                    bridge_prefix.push(next_state.pc);

                    match self.unwind_callers_once(
                        next_state,
                        stack,
                        scratch,
                        options.mode,
                        options.max_frames,
                    ) {
                        Ok(mut callers) => {
                            bridge_prefix.append(&mut callers);
                            return Ok(CapturedUnwindOutcome {
                                callers: bridge_prefix,
                                bridge_attempted: true,
                                bridge_steps,
                            });
                        }
                        Err(error) if options.bridge.should_bridge(&error) => {
                            last_error = error;
                            fp = next_state.fp;
                            sp = next_state.sp;
                        }
                        Err(error) => {
                            return Err(CapturedUnwindFailure {
                                error,
                                bridge_attempted: true,
                                bridge_steps,
                            });
                        }
                    }
                }

                Err(CapturedUnwindFailure {
                    error: last_error,
                    bridge_attempted: true,
                    bridge_steps,
                })
            }
        }
    }

    fn unwind_callers_once(
        &mut self,
        state: CapturedThreadState,
        stack: CapturedStack<'_>,
        scratch: &mut Vec<UserFrame>,
        mode: UnwindMode,
        max_frames: usize,
    ) -> Result<Vec<u64>, CapturedUnwindError> {
        if self.mappings.is_empty() {
            scratch.clear();
            return Err(CapturedUnwindError::NoMappings);
        }
        if stack.bytes.is_empty() {
            scratch.clear();
            return Err(CapturedUnwindError::EmptyStack);
        }
        if state.sp == 0 {
            scratch.clear();
            return Err(CapturedUnwindError::MissingStackPointer);
        }
        if state.pc == 0 {
            scratch.clear();
            return Err(CapturedUnwindError::MissingInstructionPointer);
        }

        let reload = self.reload_if_dirty();
        if reload.mapped_regions == 0 {
            scratch.clear();
            return Err(CapturedUnwindError::NoMappedRegions);
        }

        let lane = match mode {
            UnwindMode::CompactOnly => &mut self.compact_lane,
            UnwindMode::CompactWithDwarfRefs | UnwindMode::Default | UnwindMode::DwarfOnly => {
                &mut self.rich_lane
            }
        };

        let lane_hint = lane.hint_for(state.pc).or_else(|| {
            if self
                .mappings
                .iter()
                .any(|mapping| mapping.start <= state.pc && state.pc < mapping.end)
            {
                None
            } else {
                Some(UnwindFailure::NoBinary)
            }
        });

        scratch.clear();
        let mut read_stack = |address| stack.read_u64(strip_data_pointer(address)).ok_or(());
        let regs = native_unwind_regs(state);
        let mut iter = lane
            .unwinder
            .iter_frames(state.pc, regs, &mut lane.cache, &mut read_stack);
        let mut terminal_reason = None;

        loop {
            match iter.next() {
                Ok(Some(frame)) => scratch.push(UserFrame {
                    address: strip_code_pointer(frame.address()),
                    initial_address: None,
                }),
                Ok(None) => break,
                Err(error) => {
                    terminal_reason = Some(map_framehop_error(error, lane_hint));
                    break;
                }
            }
        }

        if scratch.len() <= 1 {
            return Err(CapturedUnwindError::OnlyLeafFrame {
                reason: terminal_reason.or(lane_hint),
            });
        }

        let mut callers = Vec::with_capacity(scratch.len().saturating_sub(1).min(max_frames));
        for frame in scratch.iter().skip(1).take(max_frames) {
            let pc = strip_code_pointer(frame.address);
            if pc != 0 {
                callers.push(pc);
            }
        }
        if callers.is_empty() {
            return Err(CapturedUnwindError::OnlyLeafFrame { reason: None });
        }
        Ok(callers)
    }
}

impl Default for CapturedStackUnwinder {
    fn default() -> Self {
        Self::new()
    }
}

struct LoadedModule {
    compact_module: NativeModule,
    rich_module: NativeModule,
    compact_support: ModuleSupport,
    rich_support: ModuleSupport,
}

fn build_modules(mapping: &CapturedImageMapping, bytes: Arc<Vec<u8>>) -> Result<LoadedModule, String> {
    let file = object::File::parse(bytes.as_slice()).map_err(|error| error.to_string())?;
    let text_svma = section_range(&file, ".text");
    let text = section_data(&file, ".text");
    let stubs_svma = section_range(&file, ".stubs");
    let stub_helper_svma = section_range(&file, ".stub_helper");
    let got_svma = section_range(&file, ".got");
    let unwind_info = section_data(&file, "__unwind_info");
    let eh_frame_svma = section_range(&file, ".eh_frame");
    let eh_frame = section_data(&file, ".eh_frame");
    let eh_frame_hdr_svma = section_range(&file, ".eh_frame_hdr");
    let eh_frame_hdr = section_data(&file, ".eh_frame_hdr");
    let debug_frame = section_data(&file, ".debug_frame");
    let text_segment_svma = segment_range(&file, "__TEXT");
    let text_segment = segment_data(&file, "__TEXT");

    let base_svma = base_svma_for(&file, text_segment_svma.as_ref(), text_svma.as_ref());
    let text_start_svma = text_svma
        .as_ref()
        .map(|range| range.start)
        .unwrap_or(base_svma);
    let base_avma = mapping
        .start
        .saturating_sub(text_start_svma.saturating_sub(base_svma));
    let avma_range = mapping.start..mapping.end;

    let compact_has_unwind = unwind_info.is_some();
    let rich_has_unwind =
        compact_has_unwind || eh_frame.is_some() || eh_frame_hdr.is_some() || debug_frame.is_some();

    let compact_module = Module::new(
        mapping.path.clone(),
        avma_range.clone(),
        base_avma,
        ExplicitModuleSectionInfo {
            base_svma,
            text_svma: text_svma.clone(),
            text: text.clone(),
            stubs_svma: stubs_svma.clone(),
            stub_helper_svma: stub_helper_svma.clone(),
            got_svma: got_svma.clone(),
            unwind_info: unwind_info.clone(),
            text_segment_svma: text_segment_svma.clone(),
            text_segment: text_segment.clone(),
            ..Default::default()
        },
    );
    let rich_module = Module::new(
        mapping.path.clone(),
        avma_range.clone(),
        base_avma,
        ExplicitModuleSectionInfo {
            base_svma,
            text_svma,
            text,
            stubs_svma,
            stub_helper_svma,
            got_svma,
            unwind_info,
            eh_frame_svma,
            eh_frame,
            eh_frame_hdr_svma,
            eh_frame_hdr,
            debug_frame,
            text_segment_svma,
            text_segment,
            ..Default::default()
        },
    );

    Ok(LoadedModule {
        compact_module,
        rich_module,
        compact_support: ModuleSupport {
            start: avma_range.start,
            end: avma_range.end,
            has_unwind_info: compact_has_unwind,
        },
        rich_support: ModuleSupport {
            start: avma_range.start,
            end: avma_range.end,
            has_unwind_info: rich_has_unwind,
        },
    })
}

fn section_range(file: &object::File<'_>, name: &str) -> Option<Range<u64>> {
    let section = file.section_by_name(name)?;
    let start = section.address();
    let end = start.checked_add(section.size())?;
    Some(start..end)
}

fn section_data(file: &object::File<'_>, name: &str) -> Option<SectionBytes> {
    let section = file.section_by_name(name)?;
    let data = section.uncompressed_data().ok()?;
    Some(cow_into_arc(data))
}

fn segment_range(file: &object::File<'_>, name: &str) -> Option<Range<u64>> {
    let segment = file
        .segments()
        .find(|segment| segment.name().ok().flatten() == Some(name))?;
    let start = segment.address();
    let end = start.checked_add(segment.size())?;
    Some(start..end)
}

fn segment_data(file: &object::File<'_>, name: &str) -> Option<SectionBytes> {
    let segment = file
        .segments()
        .find(|segment| segment.name().ok().flatten() == Some(name))?;
    let data = segment.data().ok()?;
    Some(Arc::<[u8]>::from(data))
}

fn cow_into_arc(data: Cow<'_, [u8]>) -> SectionBytes {
    match data {
        Cow::Borrowed(bytes) => Arc::<[u8]>::from(bytes),
        Cow::Owned(bytes) => Arc::<[u8]>::from(bytes),
    }
}

fn base_svma_for(
    file: &object::File<'_>,
    text_segment_svma: Option<&Range<u64>>,
    text_svma: Option<&Range<u64>>,
) -> u64 {
    match file.format() {
        object::BinaryFormat::MachO => text_segment_svma
            .map(|range| range.start)
            .or_else(|| text_svma.map(|range| range.start))
            .unwrap_or(0),
        object::BinaryFormat::Pe => file.relative_address_base(),
        _ => 0,
    }
}

fn map_framehop_error(error: framehop::Error, hint: Option<UnwindFailure>) -> UnwindFailure {
    if let Some(hint) = hint {
        return hint;
    }
    match error {
        framehop::Error::CouldNotReadStack(_) => UnwindFailure::RegisterMemoryReadFailed,
        framehop::Error::FramepointerUnwindingMovedBackwards => {
            UnwindFailure::FramePointerOutsideStack
        }
        framehop::Error::DidNotAdvance => UnwindFailure::MissingReturnAddress,
        framehop::Error::IntegerOverflow => UnwindFailure::UnsupportedRegisterRule,
        framehop::Error::ReturnAddressIsNull => UnwindFailure::MissingReturnAddress,
    }
}

#[cfg(target_arch = "aarch64")]
fn native_unwind_regs(state: CapturedThreadState) -> UnwindRegsNative {
    UnwindRegsNative::new(state.lr, state.sp, state.fp)
}

#[cfg(target_arch = "x86_64")]
fn native_unwind_regs(state: CapturedThreadState) -> UnwindRegsNative {
    UnwindRegsNative::new(state.pc, state.sp, state.fp)
}

pub fn captured_frame_pointer_walk(
    state: CapturedThreadState,
    stack: CapturedStack<'_>,
    max_frames: usize,
) -> Vec<u64> {
    let mut walked = Vec::with_capacity(max_frames);
    let mut fp = strip_data_pointer(state.fp);
    for _ in 0..max_frames {
        let Some(next_fp) = stack.read_u64(fp).map(strip_data_pointer) else {
            break;
        };
        let Some(saved_lr) = stack.read_u64(fp.saturating_add(8)).map(strip_code_pointer) else {
            break;
        };
        if saved_lr != 0 {
            walked.push(saved_lr);
        }
        if next_fp <= fp {
            break;
        }
        fp = next_fp;
    }
    walked
}

fn fp_bridge_step(stack: CapturedStack<'_>, fp: u64, sp: u64) -> Option<CapturedThreadState> {
    let fp = strip_data_pointer(fp);
    if fp == 0 || fp < strip_data_pointer(sp) {
        return None;
    }

    let next_fp = strip_data_pointer(stack.read_u64(fp)?);
    let pc = strip_code_pointer(stack.read_u64(fp.checked_add(8)?)?);
    if next_fp == 0 || next_fp <= fp || pc == 0 {
        return None;
    }

    let caller_sp = fp.checked_add(16)?;
    let lr = stack
        .read_u64(next_fp.checked_add(8)?)
        .map(strip_code_pointer)
        .unwrap_or(0);

    Some(CapturedThreadState {
        pc,
        lr,
        fp: next_fp,
        sp: caller_sp,
    })
}

#[cfg(all(target_arch = "aarch64", target_vendor = "apple"))]
pub fn strip_code_pointer(mut ptr: u64) -> u64 {
    unsafe {
        std::arch::asm!(
            "xpaci {ptr}",
            ptr = inout(reg) ptr,
            options(nomem, nostack, preserves_flags)
        );
    }
    ptr
}

#[cfg(not(all(target_arch = "aarch64", target_vendor = "apple")))]
pub fn strip_code_pointer(ptr: u64) -> u64 {
    ptr
}

#[cfg(all(target_arch = "aarch64", target_vendor = "apple"))]
pub fn strip_data_pointer(mut ptr: u64) -> u64 {
    unsafe {
        std::arch::asm!(
            "xpacd {ptr}",
            ptr = inout(reg) ptr,
            options(nomem, nostack, preserves_flags)
        );
    }
    ptr
}

#[cfg(not(all(target_arch = "aarch64", target_vendor = "apple")))]
pub fn strip_data_pointer(ptr: u64) -> u64 {
    ptr
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captured_fp_walk_reads_saved_lr_chain() {
        let mut stack = vec![0u8; 64];
        write_u64(&mut stack, 0, 0x1010);
        write_u64(&mut stack, 8, 0xaaaa);
        write_u64(&mut stack, 16, 0);
        write_u64(&mut stack, 24, 0xbbbb);

        let state = CapturedThreadState::new(0, 0, 0x1000, 0x1000);
        let stack = CapturedStack::new(0x1000, &stack);

        assert_eq!(
            captured_frame_pointer_walk(state, stack, 64),
            vec![0xaaaa, 0xbbbb]
        );
    }

    #[test]
    fn captured_fp_walk_preserves_recursive_return_addresses() {
        let mut stack = vec![0u8; 64];
        write_u64(&mut stack, 0, 0x1010);
        write_u64(&mut stack, 8, 0xaaaa);
        write_u64(&mut stack, 16, 0x1020);
        write_u64(&mut stack, 24, 0xaaaa);
        write_u64(&mut stack, 32, 0);
        write_u64(&mut stack, 40, 0xbbbb);

        let state = CapturedThreadState::new(0, 0, 0x1000, 0x1000);
        let stack = CapturedStack::new(0x1000, &stack);

        assert_eq!(
            captured_frame_pointer_walk(state, stack, 64),
            vec![0xaaaa, 0xaaaa, 0xbbbb]
        );
    }

    fn write_u64(stack: &mut [u8], offset: usize, value: u64) {
        stack[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }
}
