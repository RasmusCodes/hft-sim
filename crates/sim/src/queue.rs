//! The scheduler: a priority queue ordered by `(time, insertion sequence)`.
//!
//! Events at the same time are delivered in the order they were scheduled, so simultaneous
//! events always resolve the same way.
//!
//! The heap holds only 24-byte keys `(time, seq, slot)`. Payloads (often 100 bytes or more) sit
//! in a slot table with a free list, so heap sifts move small keys instead of whole events. Slot
//! reuse never affects order, which depends on `(time, seq)` alone.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use lob::Timestamp;

#[derive(Clone, Copy, PartialEq, Eq)]
struct Key {
    time: Timestamp,
    seq: u64,
    slot: u32,
}

impl PartialOrd for Key {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Key {
    /// Reversed, so the max-heap pops the earliest `(time, seq)` first. `seq` is unique, so
    /// this agrees with the derived equality.
    fn cmp(&self, other: &Self) -> Ordering {
        other.time.cmp(&self.time).then_with(|| other.seq.cmp(&self.seq))
    }
}

pub(crate) struct EventQueue<T> {
    heap: BinaryHeap<Key>,
    slots: Vec<Option<T>>,
    free: Vec<u32>,
    next_seq: u64,
}

impl<T> EventQueue<T> {
    pub fn new() -> Self {
        EventQueue { heap: BinaryHeap::new(), slots: Vec::new(), free: Vec::new(), next_seq: 0 }
    }

    pub fn push(&mut self, time: Timestamp, item: T) {
        let slot = match self.free.pop() {
            Some(s) => {
                self.slots[s as usize] = Some(item);
                s
            }
            None => {
                self.slots.push(Some(item));
                u32::try_from(self.slots.len() - 1).expect("fewer than 2^32 pending events")
            }
        };
        self.heap.push(Key { time, seq: self.next_seq, slot });
        self.next_seq += 1;
    }

    pub fn pop(&mut self) -> Option<(Timestamp, T)> {
        let key = self.heap.pop()?;
        let item = self.slots[key.slot as usize].take().expect("scheduled slot holds an event");
        self.free.push(key.slot);
        Some((key.time, item))
    }

    pub fn peek_time(&self) -> Option<Timestamp> {
        self.heap.peek().map(|k| k.time)
    }

    pub fn len(&self) -> usize {
        self.heap.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn earliest_first_then_insertion_order() {
        let mut q = EventQueue::new();
        q.push(5, "c");
        q.push(1, "a");
        q.push(5, "d");
        q.push(1, "b");
        let order: Vec<_> = std::iter::from_fn(|| q.pop()).collect();
        assert_eq!(order, vec![(1, "a"), (1, "b"), (5, "c"), (5, "d")]);
    }

    #[test]
    fn slot_reuse_does_not_change_order() {
        let mut q = EventQueue::new();
        q.push(10, 1);
        q.push(10, 2);
        assert_eq!(q.pop(), Some((10, 1))); // frees a slot ...
        q.push(10, 3); // ... which this event reuses, but it was scheduled last
        q.push(5, 4);
        let order: Vec<_> = std::iter::from_fn(|| q.pop()).collect();
        assert_eq!(order, vec![(5, 4), (10, 2), (10, 3)]);
        assert_eq!(q.len(), 0);
    }
}
