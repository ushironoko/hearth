//! Allocation tracking for profiling.
//!
//! A [`GlobalAlloc`] wrapper that records allocation pressure while profiling is
//! enabled, plus thread-local suppression so the profiler's own bookkeeping is
//! never counted against the code under measurement.
//!
//! Adapted from the `vize_carton` profiler (MIT).

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

thread_local! {
    static SUPPRESSION: Cell<u32> = const { Cell::new(0) };
}

pub(super) static TRACKING_ENABLED: AtomicBool = AtomicBool::new(false);
static ALLOC_CALLS: AtomicU64 = AtomicU64::new(0);
static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);
static DEALLOC_CALLS: AtomicU64 = AtomicU64::new(0);
static DEALLOC_BYTES: AtomicU64 = AtomicU64::new(0);
static REALLOC_CALLS: AtomicU64 = AtomicU64::new(0);
static REALLOC_OLD_BYTES: AtomicU64 = AtomicU64::new(0);
static REALLOC_NEW_BYTES: AtomicU64 = AtomicU64::new(0);
static ALLOC_FAILURES: AtomicU64 = AtomicU64::new(0);

/// RAII guard that suspends allocation tracking on the current thread.
#[derive(Debug)]
pub struct TrackingPause;

impl Drop for TrackingPause {
    fn drop(&mut self) {
        SUPPRESSION.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

/// Suspend allocation tracking until the returned guard is dropped.
#[inline]
pub fn pause_tracking() -> TrackingPause {
    SUPPRESSION.with(|d| d.set(d.get().saturating_add(1)));
    TrackingPause
}

#[inline]
fn is_suppressed() -> bool {
    SUPPRESSION.try_with(|d| d.get() > 0).unwrap_or(false)
}

#[inline]
fn is_enabled() -> bool {
    TRACKING_ENABLED.load(Ordering::Relaxed) && !is_suppressed()
}

/// A point-in-time copy of the process-global allocation counters.
#[derive(Debug, Clone, Copy, Default)]
pub struct AllocationSnapshot {
    pub alloc_calls: u64,
    pub alloc_bytes: u64,
    pub dealloc_calls: u64,
    pub dealloc_bytes: u64,
    pub realloc_calls: u64,
    pub realloc_old_bytes: u64,
    pub realloc_new_bytes: u64,
    pub failures: u64,
}

impl AllocationSnapshot {
    /// Allocation-like calls that requested storage.
    pub fn allocation_calls(&self) -> u64 {
        self.alloc_calls.saturating_add(self.realloc_calls)
    }

    /// Bytes requested through allocation-like calls.
    pub fn requested_bytes(&self) -> u64 {
        self.alloc_bytes.saturating_add(self.realloc_new_bytes)
    }

    /// Bytes released or replaced.
    pub fn released_bytes(&self) -> u64 {
        self.dealloc_bytes.saturating_add(self.realloc_old_bytes)
    }

    /// Approximate net heap delta.
    pub fn net_bytes(&self) -> i128 {
        i128::from(self.requested_bytes()) - i128::from(self.released_bytes())
    }

    /// Difference between two snapshots (later minus earlier), saturating.
    pub fn since(&self, earlier: &AllocationSnapshot) -> AllocationSnapshot {
        AllocationSnapshot {
            alloc_calls: self.alloc_calls.saturating_sub(earlier.alloc_calls),
            alloc_bytes: self.alloc_bytes.saturating_sub(earlier.alloc_bytes),
            dealloc_calls: self.dealloc_calls.saturating_sub(earlier.dealloc_calls),
            dealloc_bytes: self.dealloc_bytes.saturating_sub(earlier.dealloc_bytes),
            realloc_calls: self.realloc_calls.saturating_sub(earlier.realloc_calls),
            realloc_old_bytes: self
                .realloc_old_bytes
                .saturating_sub(earlier.realloc_old_bytes),
            realloc_new_bytes: self
                .realloc_new_bytes
                .saturating_sub(earlier.realloc_new_bytes),
            failures: self.failures.saturating_sub(earlier.failures),
        }
    }
}

/// Global allocator wrapper that records allocation pressure while enabled.
#[derive(Debug)]
pub struct ProfilingAllocator<A = System> {
    inner: A,
}

impl ProfilingAllocator<System> {
    pub const fn new() -> Self {
        Self { inner: System }
    }
}

impl<A> ProfilingAllocator<A> {
    pub const fn from_allocator(inner: A) -> Self {
        Self { inner }
    }
}

impl Default for ProfilingAllocator<System> {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: every method forwards the caller's layout/pointer to the inner
// allocator unchanged, then only updates lock-free counters afterwards.
unsafe impl<A: GlobalAlloc> GlobalAlloc for ProfilingAllocator<A> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { self.inner.alloc(layout) };
        if is_enabled() {
            if ptr.is_null() {
                ALLOC_FAILURES.fetch_add(1, Ordering::Relaxed);
            } else {
                ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
                ALLOC_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
            }
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { self.inner.alloc_zeroed(layout) };
        if is_enabled() {
            if ptr.is_null() {
                ALLOC_FAILURES.fetch_add(1, Ordering::Relaxed);
            } else {
                ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
                ALLOC_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
            }
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if is_enabled() {
            DEALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
            DEALLOC_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        }
        unsafe { self.inner.dealloc(ptr, layout) };
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_ptr = unsafe { self.inner.realloc(ptr, layout, new_size) };
        if is_enabled() {
            if new_ptr.is_null() {
                ALLOC_FAILURES.fetch_add(1, Ordering::Relaxed);
            } else {
                REALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
                REALLOC_OLD_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
                REALLOC_NEW_BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
            }
        }
        new_ptr
    }
}

/// Toggle allocation tracking without touching the timing profiler.
pub fn set_tracking_enabled(enabled: bool) {
    TRACKING_ENABLED.store(enabled, Ordering::Relaxed);
}

/// Reset all global allocation counters to zero.
pub fn reset_counters() {
    for c in [
        &ALLOC_CALLS,
        &ALLOC_BYTES,
        &DEALLOC_CALLS,
        &DEALLOC_BYTES,
        &REALLOC_CALLS,
        &REALLOC_OLD_BYTES,
        &REALLOC_NEW_BYTES,
        &ALLOC_FAILURES,
    ] {
        c.store(0, Ordering::Relaxed);
    }
}

/// Read the current global allocation counters.
pub fn snapshot() -> AllocationSnapshot {
    AllocationSnapshot {
        alloc_calls: ALLOC_CALLS.load(Ordering::Relaxed),
        alloc_bytes: ALLOC_BYTES.load(Ordering::Relaxed),
        dealloc_calls: DEALLOC_CALLS.load(Ordering::Relaxed),
        dealloc_bytes: DEALLOC_BYTES.load(Ordering::Relaxed),
        realloc_calls: REALLOC_CALLS.load(Ordering::Relaxed),
        realloc_old_bytes: REALLOC_OLD_BYTES.load(Ordering::Relaxed),
        realloc_new_bytes: REALLOC_NEW_BYTES.load(Ordering::Relaxed),
        failures: ALLOC_FAILURES.load(Ordering::Relaxed),
    }
}
