//! Atomic primitives that swap to loom's checked versions under
//! `--cfg loom`, so the concurrency-sensitive types are model-checked
//! with the exact code that ships.

#[cfg(loom)]
pub(crate) use loom::sync::atomic::{AtomicU32, AtomicU64, Ordering};

#[cfg(not(loom))]
pub(crate) use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
