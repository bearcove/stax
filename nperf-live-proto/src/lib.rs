//! Schema for the nperf live RPC service.
//!
//! This crate is intentionally tiny: it holds only the `#[vox::service]`
//! trait + the wire types. Both `nperf-live` (the runtime that implements
//! and serves the trait) and `xtask` (which generates TypeScript bindings
//! from the trait) depend on this crate. Keeping the schema in its own
//! crate lets `xtask` skip the heavy runtime deps (tokio, transports, etc.)
//! that `nperf-live` pulls in.

use facet::Facet;

#[derive(Clone, Debug, Facet)]
pub struct TopEntry {
    pub address: u64,
    pub self_count: u64,
    pub total_count: u64,
}

#[derive(Clone, Debug, Facet)]
pub struct TopUpdate {
    pub total_samples: u64,
    pub entries: Vec<TopEntry>,
}

/// One disassembled instruction with its current sample count.
#[derive(Clone, Debug, Facet)]
pub struct AnnotatedLine {
    pub address: u64,
    /// HTML-highlighted assembly text. Uses the class-name format of
    /// `arborium` (`<span class="a-k">mov</span>` etc.). Render with
    /// `dangerouslySetInnerHTML` and style the classes via CSS.
    pub html: String,
    pub self_count: u64,
}

#[derive(Clone, Debug, Facet)]
pub struct AnnotatedView {
    /// Best-effort symbol name (or hex string fallback).
    pub function_name: String,
    /// Address the disassembly starts at. Used by the client to mark which
    /// line corresponds to the original query address.
    pub base_address: u64,
    pub queried_address: u64,
    pub lines: Vec<AnnotatedLine>,
}

#[vox::service]
pub trait Profiler {
    /// Snapshot of the top-N functions by self time.
    async fn top(&self, limit: u32) -> Vec<TopEntry>;

    /// Stream periodic top-N updates to the client.
    async fn subscribe_top(&self, limit: u32, output: vox::Tx<TopUpdate>);

    /// Total number of samples observed since the server started.
    async fn total_samples(&self) -> u64;

    /// Stream annotated disassembly for the function containing `address`.
    /// Sample counts update live; the disassembly itself only changes if
    /// the binary is unloaded/reloaded.
    async fn subscribe_annotated(&self, address: u64, output: vox::Tx<AnnotatedView>);
}

/// All service descriptors exposed by nperf-live; the codegen iterates over
/// this list.
pub fn all_services() -> Vec<&'static vox::session::ServiceDescriptor> {
    vec![profiler_service_descriptor()]
}
