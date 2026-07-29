//! Hearth orchestration core.
//!
//! The resident [`Engine`] bundles the shared caches, the profiler, and a
//! background self-optimization loop. Tools in `hearth-tools` borrow an
//! `&Engine` per call; the daemon, CLI, and napi addon each construct exactly
//! one and keep it for their whole lifetime.
//!
//! The design follows two references: the *corsa-bind* orchestration model
//! (one handle bundling pool + session + cache, shared across tools) and the
//! *vize_carton* performance substrate (arena/compact primitives + a
//! low-overhead sharded profiler).

pub mod cache;
pub mod cancel;
pub mod engine;
pub mod invalidation;
pub mod line_index;
pub mod pathlock;
pub mod profiler;
pub mod singleflight;
pub mod watch;

pub use cancel::CancelToken;
pub use engine::{Engine, EngineConfig, Tuning};
pub use invalidation::{InvalidationDelta, InvalidationLog};
pub use line_index::LineIndex;
pub use pathlock::{PathGuard, PathLocks, mutation_key};

// Re-export the protocol types so downstream crates can depend on just core.
pub use hearth_proto as proto;
