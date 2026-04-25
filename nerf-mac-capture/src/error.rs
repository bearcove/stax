// Vendored from samply (https://github.com/mstange/samply) at commit
// 1920bd32c569de5650d1129eb035f43bd28ace27. MIT OR Apache-2.0; see
// LICENSE-MIT and LICENSE-APACHE at the crate root.

use thiserror::Error;

use super::kernel_error::KernelError;

#[derive(Debug, Clone, Error)]
pub enum SamplingError {
    #[error("Fatal error encountered during sampling: {0}, {1}")]
    Fatal(&'static str, KernelError),

    #[error("Ignorable error encountered during sampling: {0}, {1}")]
    Ignorable(&'static str, KernelError),

    #[error("The target thread has probably been terminated. {0}, {1}")]
    ThreadTerminated(&'static str, KernelError),

    #[error("The target process has probably been terminated. {0}, {1}")]
    ProcessTerminated(&'static str, KernelError),

    #[error("Could not obtain root task.")]
    CouldNotObtainRootTask,
}
