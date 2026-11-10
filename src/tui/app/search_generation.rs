use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// The atomic's value while the app isn't awaiting any search. Generations
/// start at 1.
const NONE_AWAITED: u64 = 0;

/// The app's search generation: responses to an earlier one are stale. The
/// search worker reads the generation the app awaits through
/// [`AwaitedSearch`], and reruns its last search over texts read only for
/// that generation.
pub(super) struct SearchGeneration {
    current: u64,
    awaited: Arc<AtomicU64>,
}

impl SearchGeneration {
    /// Generation 0, not awaiting any search, and the worker's view of the
    /// generation the app awaits.
    pub(super) fn start() -> (Self, AwaitedSearch) {
        let awaited = Arc::new(AtomicU64::new(NONE_AWAITED));
        let generation = Self {
            current: 0,
            awaited: Arc::clone(&awaited),
        };
        (generation, AwaitedSearch(awaited))
    }

    pub(super) fn current(&self) -> u64 {
        self.current
    }

    /// Moves to the next generation and awaits it, returning it.
    pub(super) fn advance(&mut self) -> u64 {
        self.current += 1;
        self.awaited.store(self.current, Ordering::Release);
        self.current
    }

    /// Keeps the current generation and stops awaiting its search: texts
    /// read no longer run it again.
    pub(super) fn stop_awaiting(&self) {
        self.awaited.store(NONE_AWAITED, Ordering::Release);
    }

    /// Advances to `generation`, a later one, as that many searches would.
    #[cfg(test)]
    pub(super) fn advance_to(&mut self, generation: u64) {
        assert!(
            generation > self.current,
            "generation {generation} is after the current {}",
            self.current
        );
        while self.current < generation {
            self.advance();
        }
    }
}

/// The search worker's read of the generation the app awaits.
pub(super) struct AwaitedSearch(Arc<AtomicU64>);

impl AwaitedSearch {
    pub(super) fn is_awaited(&self, generation: u64) -> bool {
        generation != NONE_AWAITED && self.0.load(Ordering::Acquire) == generation
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_advance_awaits_the_new_generation_alone() {
        let (mut generation, awaited) = SearchGeneration::start();
        let first = generation.advance();
        let second = generation.advance();

        assert_eq!((first, second, generation.current()), (1, 2, 2));
        assert!(
            awaited.is_awaited(2) && !awaited.is_awaited(1),
            "only the newest generation is awaited"
        );
    }

    #[test]
    fn stop_awaiting_keeps_the_generation_and_awaits_none() {
        let (mut generation, awaited) = SearchGeneration::start();
        let current = generation.advance();

        generation.stop_awaiting();

        assert_eq!(generation.current(), current, "the generation stays");
        assert!(
            !awaited.is_awaited(current),
            "its search is no longer awaited"
        );
    }

    #[test]
    fn generation_zero_is_never_awaited() {
        let (_generation, awaited) = SearchGeneration::start();

        assert!(!awaited.is_awaited(0), "no search has generation 0");
    }
}
