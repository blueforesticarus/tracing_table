//! Reusable numeric, alignment, and allocation primitives for data storage.
pub mod float_u16;
pub mod smallbox;
pub mod unalign;

// Shared sparse ownership keeps tracing metadata out of allocation primitives.
pub mod exact_set;
