// Adapted from samply/src/mac/task_profiler.rs::add_lib_to_unwinder_and_ensure_debug_id
// (commit 1920bd32c569de5650d1129eb035f43bd28ace27). MIT OR Apache-2.0; see
// LICENSE-MIT and LICENSE-APACHE at the crate root.
//
// Differences from samply:
//   - The `text_segment_data`-driven `compute_debug_id_from_text_section`
//     fallback is dropped. We rely on the LC_UUID load command for the
//     module identity. If a binary is missing LC_UUID the framehop module
//     still works for unwinding; only post-hoc symbol matching might miss.
//   - The on-disk `__debug_frame` fallback (`get_debug_frame`) is dropped.
//     Mach-O binaries on macOS ship `__unwind_info` in the loaded mapping
//     in practice; if we hit cases that need `__debug_frame` we'll add
//     the mmap-from-disk path back.

use framehop::{ExplicitModuleSectionInfo, MayAllocateDuringUnwind, Module, Unwinder, UnwinderNative};
use mach2::port::mach_port_t;

use crate::proc_maps::{DyldInfo, ModuleSvmaInfo, VmSubData};
use crate::types::UnwindSectionBytes;

pub fn add_lib_to_unwinder(
    unwinder: &mut UnwinderNative<UnwindSectionBytes, MayAllocateDuringUnwind>,
    task: mach_port_t,
    lib: &DyldInfo,
) {
    let ModuleSvmaInfo {
        base_svma,
        text_svma,
        stubs_svma,
        stub_helper_svma,
        got_svma,
        eh_frame_svma,
        eh_frame_hdr_svma,
        text_segment_svma,
    } = lib.module_info.clone();

    let base_avma = lib.base_avma;
    let unwind_info = lib
        .unwind_sections
        .unwind_info_section
        .and_then(|(svma, size)| {
            VmSubData::map_from_task(task, svma - base_svma + base_avma, size).ok()
        })
        .map(UnwindSectionBytes::Remapped);
    let eh_frame = lib
        .unwind_sections
        .eh_frame_section
        .and_then(|(svma, size)| {
            VmSubData::map_from_task(task, svma - base_svma + base_avma, size).ok()
        })
        .map(UnwindSectionBytes::Remapped);
    let text_segment = lib.unwind_sections.text_segment.and_then(|(svma, size)| {
        let avma = svma - base_svma + base_avma;
        VmSubData::map_from_task(task, avma, size).ok()
    }).map(UnwindSectionBytes::Remapped);

    let module = Module::new(
        lib.file.clone(),
        lib.base_avma..(lib.base_avma + lib.vmsize),
        lib.base_avma,
        ExplicitModuleSectionInfo {
            base_svma,
            text_svma,
            text: None,
            stubs_svma,
            stub_helper_svma,
            got_svma,
            unwind_info,
            eh_frame_svma,
            eh_frame,
            eh_frame_hdr_svma,
            eh_frame_hdr: None,
            debug_frame: None,
            text_segment_svma,
            text_segment,
        },
    );
    unwinder.add_module(module);
}

pub fn remove_lib_from_unwinder(
    unwinder: &mut UnwinderNative<UnwindSectionBytes, MayAllocateDuringUnwind>,
    base_avma: u64,
) {
    unwinder.remove_module(base_avma);
}
