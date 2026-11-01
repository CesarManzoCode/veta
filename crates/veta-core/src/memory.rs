//! Global memory accounting for hash tables and materialized buffers (contract §12).
//!
//! Exceeding the limit returns [`VetaError::MemoryLimit`] (exit 4); there is no spilling.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{Result, VetaError};

/// Default `--memory-limit-mb` (contract §12).
pub const DEFAULT_LIMIT_MB: u64 = 10_240;

/// Atomic counter of reserved bytes with a limit.
#[derive(Debug)]
pub struct MemoryTracker {
    used: AtomicU64,
    peak: AtomicU64,
    limit: AtomicU64,
}

static GLOBAL: MemoryTracker = MemoryTracker::new(DEFAULT_LIMIT_MB * 1024 * 1024);

/// The process-wide tracker.
pub fn global() -> &'static MemoryTracker {
    &GLOBAL
}

impl MemoryTracker {
    pub const fn new(limit_bytes: u64) -> MemoryTracker {
        MemoryTracker {
            used: AtomicU64::new(0),
            peak: AtomicU64::new(0),
            limit: AtomicU64::new(limit_bytes),
        }
    }

    pub fn set_limit(&self, limit_bytes: u64) {
        self.limit.store(limit_bytes, Ordering::Relaxed);
    }

    pub fn set_limit_mb(&self, limit_mb: u64) {
        self.set_limit(limit_mb.saturating_mul(1024 * 1024));
    }

    pub fn limit(&self) -> u64 {
        self.limit.load(Ordering::Relaxed)
    }

    pub fn used(&self) -> u64 {
        self.used.load(Ordering::Relaxed)
    }

    /// Highest `used` seen.
    pub fn peak(&self) -> u64 {
        self.peak.load(Ordering::Relaxed)
    }

    /// Reserves `bytes`, or fails without reserving anything if the total would pass the
    /// limit.
    pub fn try_reserve(&self, bytes: u64) -> Result<()> {
        let limit = self.limit();
        let result = self
            .used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes).filter(|&total| total <= limit)
            });
        match result {
            Ok(previous) => {
                self.peak.fetch_max(previous + bytes, Ordering::Relaxed);
                Ok(())
            }
            Err(_) => Err(VetaError::MemoryLimit {
                requested: bytes,
                limit,
            }),
        }
    }

    /// Returns `bytes` to the tracker (saturating at zero).
    pub fn release(&self, bytes: u64) {
        let _ = self
            .used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                Some(used.saturating_sub(bytes))
            });
    }

    /// Reserves `bytes` and returns a guard that releases them when dropped.
    pub fn reserve(&self, bytes: u64) -> Result<Reservation<'_>> {
        self.try_reserve(bytes)?;
        Ok(Reservation {
            tracker: self,
            bytes,
        })
    }
}

/// Reserved bytes, released on drop.
#[derive(Debug)]
pub struct Reservation<'a> {
    tracker: &'a MemoryTracker,
    bytes: u64,
}

impl Reservation<'_> {
    pub fn size(&self) -> u64 {
        self.bytes
    }

    pub fn grow(&mut self, bytes: u64) -> Result<()> {
        self.tracker.try_reserve(bytes)?;
        self.bytes += bytes;
        Ok(())
    }

    pub fn shrink(&mut self, bytes: u64) {
        let bytes = bytes.min(self.bytes);
        self.tracker.release(bytes);
        self.bytes -= bytes;
    }

    /// Grows or shrinks to exactly `bytes`.
    pub fn resize(&mut self, bytes: u64) -> Result<()> {
        if bytes > self.bytes {
            self.grow(bytes - self.bytes)
        } else {
            self.shrink(self.bytes - bytes);
            Ok(())
        }
    }
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        self.tracker.release(self.bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserve_until_the_limit() {
        let tracker = MemoryTracker::new(100);
        tracker.try_reserve(60).unwrap();
        tracker.try_reserve(40).unwrap();
        let err = tracker.try_reserve(1).unwrap_err();
        assert_eq!(err.exit_code(), 4);
        assert_eq!(
            err,
            VetaError::MemoryLimit {
                requested: 1,
                limit: 100
            }
        );
        assert_eq!(tracker.used(), 100);
        tracker.release(50);
        assert_eq!(tracker.used(), 50);
        tracker.release(500);
        assert_eq!(tracker.used(), 0);
        assert_eq!(tracker.peak(), 100);
        assert!(tracker.try_reserve(u64::MAX).is_err());
    }

    #[test]
    fn reservations_release_on_drop() {
        let tracker = MemoryTracker::new(1000);
        {
            let mut r = tracker.reserve(100).unwrap();
            r.grow(400).unwrap();
            assert_eq!(tracker.used(), 500);
            assert!(r.grow(600).is_err());
            assert_eq!(r.size(), 500);
            r.shrink(200);
            assert_eq!(tracker.used(), 300);
            r.resize(900).unwrap();
            assert_eq!(tracker.used(), 900);
            r.resize(10).unwrap();
            assert_eq!(tracker.used(), 10);
        }
        assert_eq!(tracker.used(), 0);
        assert_eq!(tracker.peak(), 900);
    }

    #[test]
    fn limit_can_change() {
        let tracker = MemoryTracker::new(0);
        assert!(tracker.try_reserve(1).is_err());
        tracker.set_limit_mb(1);
        assert_eq!(tracker.limit(), 1024 * 1024);
        assert!(tracker.try_reserve(1024 * 1024).is_ok());
    }

    #[test]
    fn concurrent_reservations_never_pass_the_limit() {
        let tracker = MemoryTracker::new(1000);
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| {
                    for _ in 0..1000 {
                        if tracker.try_reserve(7).is_ok() {
                            assert!(tracker.used() <= 1000);
                            tracker.release(7);
                        }
                    }
                });
            }
        });
        assert_eq!(tracker.used(), 0);
        assert!(tracker.peak() <= 1000);
    }

    #[test]
    fn global_tracker_has_the_default_limit() {
        assert!(std::ptr::eq(global(), global()));
        assert_eq!(global().limit(), DEFAULT_LIMIT_MB * 1024 * 1024);
    }
}
