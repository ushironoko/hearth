//! Per-operation metrics: counts, self/total durations, and an allocation-free
//! log2-microsecond histogram from which percentiles are estimated.

use std::time::Duration;

/// Number of histogram buckets (log2 of microseconds, capped).
const BUCKETS: usize = 48;

/// Aggregated timing metrics for one named span.
#[derive(Debug, Clone)]
pub struct Metrics {
    pub count: u64,
    pub total: Duration,
    pub self_time: Duration,
    pub child_time: Duration,
    pub min: Duration,
    pub max: Duration,
    histogram: [u64; BUCKETS],
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            count: 0,
            total: Duration::ZERO,
            self_time: Duration::ZERO,
            child_time: Duration::ZERO,
            min: Duration::MAX,
            max: Duration::ZERO,
            histogram: [0; BUCKETS],
        }
    }
}

impl Metrics {
    /// Record one sample, given its total duration and the time spent in nested
    /// child spans (so self-time can be derived).
    pub fn record(&mut self, duration: Duration, child: Duration) {
        self.count += 1;
        self.total = self.total.saturating_add(duration);
        self.child_time = self.child_time.saturating_add(child);
        self.self_time = self.self_time.saturating_add(duration.saturating_sub(child));
        self.min = self.min.min(duration);
        self.max = self.max.max(duration);
        let bucket = duration_bucket(duration);
        self.histogram[bucket] = self.histogram[bucket].saturating_add(1);
    }

    /// Merge another metrics record into this one (for cross-shard aggregation).
    pub fn merge(&mut self, other: &Metrics) {
        self.count += other.count;
        self.total = self.total.saturating_add(other.total);
        self.self_time = self.self_time.saturating_add(other.self_time);
        self.child_time = self.child_time.saturating_add(other.child_time);
        self.min = self.min.min(other.min);
        self.max = self.max.max(other.max);
        for i in 0..BUCKETS {
            self.histogram[i] = self.histogram[i].saturating_add(other.histogram[i]);
        }
    }

    /// Estimate the p-th percentile (0.0..=1.0) from the histogram.
    pub fn percentile(&self, p: f64) -> Duration {
        if self.count == 0 {
            return Duration::ZERO;
        }
        let target = ((self.count as f64) * p).ceil() as u64;
        let mut seen = 0u64;
        for (i, &n) in self.histogram.iter().enumerate() {
            seen += n;
            if seen >= target {
                return bucket_upper_bound(i);
            }
        }
        self.max
    }

    pub fn mean(&self) -> Duration {
        if self.count == 0 {
            Duration::ZERO
        } else {
            self.total / (self.count as u32).max(1)
        }
    }
}

/// Map a duration to a log2-micros bucket in `0..BUCKETS`.
#[inline]
fn duration_bucket(d: Duration) -> usize {
    let micros = d.as_micros().max(1) as u64;
    // floor(log2(micros)) clamped to the last bucket.
    let bucket = 63 - micros.leading_zeros() as usize;
    bucket.min(BUCKETS - 1)
}

/// The exclusive upper bound (as a Duration) for a bucket index.
#[inline]
fn bucket_upper_bound(bucket: usize) -> Duration {
    let micros = 1u64 << (bucket as u32).min(63);
    Duration::from_micros(micros)
}

/// A monotonic non-duration counter (bytes, calls, cache hits, …).
#[derive(Debug, Clone, Default)]
pub struct Counter {
    pub samples: u64,
    pub total: u64,
    pub min: u64,
    pub max: u64,
}

impl Counter {
    pub fn record(&mut self, value: u64) {
        if self.samples == 0 {
            self.min = value;
            self.max = value;
        } else {
            self.min = self.min.min(value);
            self.max = self.max.max(value);
        }
        self.samples += 1;
        self.total = self.total.saturating_add(value);
    }

    pub fn merge(&mut self, other: &Counter) {
        if other.samples == 0 {
            return;
        }
        if self.samples == 0 {
            *self = other.clone();
        } else {
            self.samples += other.samples;
            self.total = self.total.saturating_add(other.total);
            self.min = self.min.min(other.min);
            self.max = self.max.max(other.max);
        }
    }
}
