//! Trait + event types that downstream crates implement and consume.
//!
//! This crate used to contain a samply-derived suspend-and-walk
//! sampling recorder; that recorder has been removed, and only the
//! `SampleSink` trait (plus `MachOSymbol`, `ThreadNameCache`) remains.
//! The daemon backend (`staxd-client`) is the sole sampling path.

// Pure trait + event types — no OS APIs. Despite the crate name it
// compiles and is consumed on every target (the Linux capture backend
// drives the same `SampleSink`). Renaming the crate to `stax-capture`
// is a deferred cleanup (it would ripple imports across the workspace).

pub mod proc_maps;
pub mod recorder;
pub mod sample_sink;

pub use sample_sink::{
    BinaryLoadedEvent, BinaryUnloadedEvent, CpuIntervalEvent, CpuIntervalKind, JitdumpEvent,
    MachOByteSource, SampleEvent, SampleSink, ThreadNameEvent, WakeupEvent,
};
