//! When a batch of independent items runs on the rayon pool.

/// The number of items a batch needs before it runs on the rayon pool.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ParallelThreshold {
    min_items: usize,
}

impl ParallelThreshold {
    /// 256 items. A smaller batch runs on the calling thread instead of
    /// waiting for a pool that another session's parse may be keeping busy.
    pub const DEFAULT: Self = Self { min_items: 256 };

    /// Every batch runs on the rayon pool, empty ones included.
    #[cfg(test)]
    pub const ALWAYS: Self = Self { min_items: 0 };

    /// No batch runs on the rayon pool.
    #[cfg(test)]
    pub const NEVER: Self = Self {
        min_items: usize::MAX,
    };

    pub fn runs_in_parallel(self, items: usize) -> bool {
        items >= self.min_items
    }
}
