//! Compact tracing storage, input capture/replay, and static metadata scanning.
#![feature(ptr_metadata)]
pub mod data;
pub mod subscriber;

// Make scanner types and artifact readers available to this library's consumers.
pub use tracing_scan_elf;
