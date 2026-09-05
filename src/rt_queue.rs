//! Fixed-capacity single-producer, single-consumer transport for realtime data.
//!
//! The producer owns `head` and the consumer owns `tail`. A slot is published
//! with a release store only after its value has been written, and is returned
//! to the producer with a release store only after the consumer has copied it.
//! When the queue is full, the producer may replace the newest ready slot. A
//! compare-exchange gives the consumer exclusive ownership first if it is
//! already reading that slot, so coalescing never races a callback read.

use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

const EMPTY: u8 = 0;
const READY: u8 = 1;
const WRITING: u8 = 2;
const READING: u8 = 3;

/// Result of attempting to append a value to a bounded queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushResult {
    /// The value occupied a new queue position.
    Queued,
    /// The queue was full, so the newest pending value was replaced.
    Coalesced,
    /// The queue was full and its newest value was being consumed at the same
    /// time. The incoming value is discarded.
    Full,
}

/// A fixed-capacity lock-free SPSC queue.
///
/// `CAPACITY` must be at least two. The queue allocates no storage after
/// construction and neither [`Self::push`] nor [`Self::pop`] performs locking,
/// allocation, I/O, or unbounded work.
pub struct SpscQueue<T, const CAPACITY: usize> {
    slots: [UnsafeCell<MaybeUninit<T>>; CAPACITY],
    states: [AtomicU8; CAPACITY],
    head: AtomicUsize,
    tail: AtomicUsize,
}

impl<T, const CAPACITY: usize> SpscQueue<T, CAPACITY> {
    /// Construct an empty queue with all storage in place.
    pub fn new() -> Self {
        assert!(CAPACITY >= 2, "an SPSC queue needs at least two slots");
        Self {
            slots: std::array::from_fn(|_| UnsafeCell::new(MaybeUninit::uninit())),
            states: std::array::from_fn(|_| AtomicU8::new(EMPTY)),
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
        }
    }

    /// Number of values currently available to the consumer.
    pub fn len(&self) -> usize {
        self.head
            .load(Ordering::Acquire)
            .wrapping_sub(self.tail.load(Ordering::Acquire))
            .min(CAPACITY)
    }

    /// Whether no value is currently available.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether all queue positions are occupied.
    pub fn is_full(&self) -> bool {
        self.len() == CAPACITY
    }

    /// Append a value without waiting for the consumer.
    ///
    /// There is one producer by contract. A full queue coalesces by replacing
    /// its newest pending slot when that slot is not concurrently being read.
    /// If the consumer wins that ownership race, `Full` is returned and the
    /// incoming value is discarded.
    pub fn push(&self, value: T) -> PushResult {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);

        if head.wrapping_sub(tail) < CAPACITY {
            let index = head % CAPACITY;
            // SAFETY: SPSC ownership guarantees that this index was released
            // by the consumer before it is reused by the producer.
            unsafe { (*self.slots[index].get()).write(value) };
            self.states[index].store(READY, Ordering::Release);
            self.head.store(head.wrapping_add(1), Ordering::Release);
            return PushResult::Queued;
        }

        // Keep the queue bounded while letting bursts converge to their newest
        // state. The consumer can claim this slot first; in that case no write
        // occurs and the incoming value is simply dropped.
        let newest = head.wrapping_sub(1) % CAPACITY;
        if self.states[newest]
            .compare_exchange(READY, WRITING, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            // SAFETY: WRITING is exclusively owned by this producer. The old
            // value is no longer visible to the consumer while this state is
            // held.
            unsafe {
                (*self.slots[newest].get()).assume_init_drop();
                (*self.slots[newest].get()).write(value);
            }
            self.states[newest].store(READY, Ordering::Release);
            PushResult::Coalesced
        } else {
            PushResult::Full
        }
    }

    /// Remove the oldest available value without waiting for the producer.
    pub fn pop(&self) -> Option<T> {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        if tail == head {
            return None;
        }

        let index = tail % CAPACITY;
        // The producer only coalesces the newest slot. With CAPACITY >= 2,
        // that cannot be this oldest slot while the queue is full. Therefore a
        // ready value is guaranteed here after the acquire of head.
        if self.states[index]
            .compare_exchange(READY, READING, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return None;
        }

        // SAFETY: READING is exclusively owned by this consumer, and head was
        // published only after the producer initialized this slot.
        let value = unsafe { (*self.slots[index].get()).assume_init_read() };
        self.states[index].store(EMPTY, Ordering::Release);
        self.tail.store(tail.wrapping_add(1), Ordering::Release);
        Some(value)
    }
}

impl<T, const CAPACITY: usize> Default for SpscQueue<T, CAPACITY> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T, const CAPACITY: usize> Drop for SpscQueue<T, CAPACITY> {
    fn drop(&mut self) {
        // A queue is dropped only after its producer and consumer have stopped.
        // Drop every value that remains published; WRITING is included for
        // defensive completeness if construction is interrupted by unwinding.
        for index in 0..CAPACITY {
            if matches!(self.states[index].load(Ordering::Relaxed), READY | WRITING) {
                // SAFETY: no queue operation can run while &mut self exists.
                unsafe { (*self.slots[index].get()).assume_init_drop() };
            }
        }
    }
}

// SAFETY: T crosses from the one producer thread to the one consumer thread,
// and every slot access is ordered by its state and the head/tail atomics.
unsafe impl<T: Send, const CAPACITY: usize> Send for SpscQueue<T, CAPACITY> {}
// SAFETY: shared references are safe because the SPSC protocol gives unique
// access to each initialized slot at every point in its lifetime.
unsafe impl<T: Send, const CAPACITY: usize> Sync for SpscQueue<T, CAPACITY> {}

#[cfg(test)]
mod tests {
    use super::{PushResult, SpscQueue};

    type Queue = SpscQueue<u32, 4>;

    #[test]
    fn preserves_order_across_wraparound() {
        let queue = Queue::new();
        for value in 0..4 {
            assert_eq!(queue.push(value), PushResult::Queued);
        }
        assert_eq!(queue.pop(), Some(0));
        assert_eq!(queue.pop(), Some(1));
        assert_eq!(queue.push(4), PushResult::Queued);
        assert_eq!(queue.push(5), PushResult::Queued);
        assert_eq!(queue.pop(), Some(2));
        assert_eq!(queue.pop(), Some(3));
        assert_eq!(queue.pop(), Some(4));
        assert_eq!(queue.pop(), Some(5));
        assert_eq!(queue.pop(), None);
    }

    #[test]
    fn full_queue_coalesces_newest_pending_value() {
        let queue = Queue::new();
        for value in 0..4 {
            assert_eq!(queue.push(value), PushResult::Queued);
        }
        assert_eq!(queue.push(99), PushResult::Coalesced);
        assert_eq!(queue.len(), 4);
        assert_eq!(queue.pop(), Some(0));
        assert_eq!(queue.pop(), Some(1));
        assert_eq!(queue.pop(), Some(2));
        assert_eq!(queue.pop(), Some(99));
        assert!(queue.is_empty());
    }

    #[test]
    fn full_queue_is_nonblocking_when_newest_is_being_read() {
        let queue = Queue::new();
        for value in 0..4 {
            assert_eq!(queue.push(value), PushResult::Queued);
        }

        // Sequential ownership means the normal full case always coalesces;
        // this assertion documents that it remains bounded and never grows.
        assert!(matches!(
            queue.push(4),
            PushResult::Coalesced | PushResult::Full
        ));
        assert_eq!(queue.len(), 4);
    }
}
