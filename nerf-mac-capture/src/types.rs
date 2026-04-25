// Lifted from samply/src/mac/task_profiler.rs (commit
// 1920bd32c569de5650d1129eb035f43bd28ace27). Originally `UnwindSectionBytes`
// was defined inside `task_profiler.rs`; we put it here so it can be shared
// between `proc_maps` and the higher-level capture pipeline without a
// circular dependency. MIT OR Apache-2.0; see LICENSE-MIT and LICENSE-APACHE
// at the crate root.

use std::ops::Deref;

use crate::proc_maps::VmSubData;

/// A backing for unwind-section bytes (`__unwind_info`, `__eh_frame`,
/// `__debug_frame`). The bytes can either be `mach_vm_remap`-mapped from a
/// running task's address space, or allocated on the heap (e.g. after
/// decompressing `__debug_frame`).
///
/// samply also has an `Mmap` variant for on-disk binaries that don't carry
/// the unwind sections in their loaded mapping; we omit it for now and
/// add it back if framehop fails to find unwind info in practice.
#[derive(Debug)]
pub enum UnwindSectionBytes {
    Remapped(VmSubData),
    Allocated(Vec<u8>),
}

impl Deref for UnwindSectionBytes {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            UnwindSectionBytes::Remapped(vm_sub_data) => vm_sub_data.deref(),
            UnwindSectionBytes::Allocated(vec) => vec.deref(),
        }
    }
}
