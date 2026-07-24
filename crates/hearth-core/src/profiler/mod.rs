//! Low-overhead, always-linkable profiler for the resident engine.
//!
//! * [`ProfilingAllocator`] — a `#[global_allocator]` decorator the leaf binary
//!   composes over mimalloc (only under the `profiling` feature).
//! * [`Profiler`] — sharded timing + counters with histogram percentiles.
//! * [`report`] — fuses timing, counters, and allocation deltas into text.
//!
//! A process holds exactly one profiler, reached via [`global_profiler`]. This
//! is the one deliberate global in the crate: an allocator hook is inherently
//! process-wide, so the metrics it feeds share that scope.

mod allocation;
mod core;
mod metrics;

pub use allocation::{
    pause_tracking, reset_counters, set_tracking_enabled, snapshot as allocation_snapshot,
    AllocationSnapshot, ProfilingAllocator, TrackingPause,
};
pub use core::{ProfileGuard, Profiler};
pub use metrics::{Counter, Metrics};

use once_cell::sync::Lazy;
use std::time::Duration;

static GLOBAL: Lazy<Profiler> = Lazy::new(Profiler::new);

/// The process-wide profiler.
#[inline]
pub fn global_profiler() -> &'static Profiler {
    &GLOBAL
}

/// Open a scoped span on the global profiler. Cheap no-op when disabled.
#[inline]
pub fn span(name: &'static str) -> ProfileGuard<'static> {
    GLOBAL.span(name)
}

/// Record a value into a global counter.
#[inline]
pub fn count(name: &'static str, value: u64) {
    GLOBAL.count(name, value);
}

/// Time a block on the global profiler. Expands to just the block when the
/// profiler is disabled, so it may be sprinkled on fine-grained hot sites.
#[macro_export]
macro_rules! profile {
    ($name:expr, $body:block) => {{
        let __g = $crate::profiler::global_profiler();
        if __g.is_enabled() {
            let _guard = __g.span($name);
            $body
        } else {
            $body
        }
    }};
}

/// Render a human-readable profiler report fusing all three signal sources.
pub fn report() -> String {
    use std::fmt::Write;
    let p = global_profiler();
    let mut out = String::with_capacity(2048);

    let timing = p.timing_snapshot();
    if !timing.is_empty() {
        out.push_str("── operations (by total) ──\n");
        let _ = writeln!(
            out,
            "{:<28} {:>8} {:>10} {:>10} {:>10} {:>10}",
            "span", "count", "total", "self", "p50", "p99"
        );
        for (name, m) in &timing {
            let _ = writeln!(
                out,
                "{:<28} {:>8} {:>10} {:>10} {:>10} {:>10}",
                name,
                m.count,
                ms(m.total),
                ms(m.self_time),
                ms(m.percentile(0.50)),
                ms(m.percentile(0.99)),
            );
        }
    }

    let counters = p.counter_snapshot();
    if !counters.is_empty() {
        out.push_str("\n── counters ──\n");
        for (name, c) in &counters {
            let _ = writeln!(out, "{:<28} total={:<12} samples={}", name, c.total, c.samples);
        }
    }

    let a = allocation_snapshot();
    if a.allocation_calls() > 0 {
        out.push_str("\n── allocations ──\n");
        let _ = writeln!(
            out,
            "alloc_calls={} alloc_bytes={} dealloc_calls={} net_bytes={}",
            a.allocation_calls(),
            a.requested_bytes(),
            a.dealloc_calls,
            a.net_bytes(),
        );
    }

    if out.is_empty() {
        out.push_str("(profiler disabled or no samples)\n");
    }
    out
}

#[inline]
fn ms(d: Duration) -> String {
    format!("{:.3}ms", d.as_secs_f64() * 1000.0)
}
