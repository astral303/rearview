use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// The atomic's value while the app isn't awaiting any request. Generations
/// start at 1.
const NONE_AWAITED: u64 = 0;

/// The generation of the app's newest request to a worker thread: responses
/// to an earlier one are stale. The worker reads the generation the app
/// awaits through [`AwaitedRequest`], and stops work for any other.
pub(super) struct RequestGeneration {
    current: u64,
    awaited: Arc<AtomicU64>,
}

impl RequestGeneration {
    /// Generation 0, not awaiting any request, and the worker's view of the
    /// generation the app awaits.
    pub(super) fn start() -> (Self, AwaitedRequest) {
        let awaited = Arc::new(AtomicU64::new(NONE_AWAITED));
        let generation = Self {
            current: 0,
            awaited: Arc::clone(&awaited),
        };
        (generation, AwaitedRequest(awaited))
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

    /// Keeps the current generation and stops awaiting its response.
    pub(super) fn stop_awaiting(&self) {
        self.awaited.store(NONE_AWAITED, Ordering::Release);
    }

    /// Advances to `generation`, a later one, as that many requests would.
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

/// A worker's read of the generation the app awaits.
pub(super) struct AwaitedRequest(Arc<AtomicU64>);

impl AwaitedRequest {
    pub(super) fn is_awaited(&self, generation: u64) -> bool {
        generation != NONE_AWAITED && self.0.load(Ordering::Acquire) == generation
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_advance_awaits_the_new_generation_alone() {
        let (mut generation, awaited) = RequestGeneration::start();
        let first = generation.advance();
        let second = generation.advance();

        assert_eq!((first, second, generation.current()), (1, 2, 2));
        assert!(
            awaited.is_awaited(2) && !awaited.is_awaited(1),
            "only the newest generation is awaited"
        );
    }

    #[test]
    fn stop_awaiting_keeps_the_generation_and_doesnt_await_any() {
        let (mut generation, awaited) = RequestGeneration::start();
        let current = generation.advance();

        generation.stop_awaiting();

        assert_eq!(generation.current(), current, "the generation stays");
        assert!(
            !awaited.is_awaited(current),
            "its response is no longer awaited"
        );
    }

    #[test]
    fn generation_zero_is_never_awaited() {
        let (_generation, awaited) = RequestGeneration::start();

        assert!(!awaited.is_awaited(0), "no request has generation 0");
    }
}
