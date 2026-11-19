//! Order storage for the matching engine.
//!
//! * Orders live in a `Vec<Node>` arena addressed by `u32` slots and recycled through a
//!   free list: no per-order heap allocation in steady state and no raw pointers, so
//!   the structure is memory-safe without `unsafe`.
//! * Each price level holds an intrusive doubly linked FIFO (head/tail slots); cancel by
//!   id is O(1) after the hash lookup plus a level lookup.
//! * Levels per side are a `Vec` sorted so that the best price is the *last* element:
//!   matching consumes the back in O(1), and inserting near the touch, where most
//!   orders arrive, moves few elements.
//! * `OrderId -> slot` is a hash map with a fixed multiplicative hash (no random seed),
//!   so nothing about a run depends on per-process randomness.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

use crate::types::{OrderId, OrderSnapshot, OwnerId, Price, Qty, Side, StpMode};

/// Null slot.
pub(crate) const NIL: u32 = u32::MAX;

/// Fibonacci-multiplicative hasher for integer keys. Deterministic: unlike `std`'s default
/// hasher it has no per-process random key, so nothing about a run depends on it. Not
/// collision-resistant against adversarial keys, which simulator ids never are.
#[derive(Default, Clone, Copy, Debug)]
pub struct IdHasher(u64);

impl Hasher for IdHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }

    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0.rotate_left(8) ^ u64::from(b)).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        }
    }

    #[inline]
    fn write_u64(&mut self, n: u64) {
        self.0 = (self.0 ^ n).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    }
}

/// Hash map keyed by order id with the deterministic hasher.
pub(crate) type IdMap<V> = HashMap<OrderId, V, BuildHasherDefault<IdHasher>>;

/// One live order. 64 bytes: one cache line.
#[derive(Clone, Debug)]
pub(crate) struct Node {
    pub id: OrderId,
    /// Limit price (unused for market orders).
    pub price: Price,
    pub leaves: Qty,
    pub order_qty: Qty,
    pub cum_qty: Qty,
    /// Time priority: strictly increasing along every queue.
    pub prio: u64,
    pub owner: OwnerId,
    pub prev: u32,
    pub next: u32,
    pub side: Side,
    pub is_market: bool,
    pub post_only: bool,
    pub stp: StpMode,
}

/// One price level: aggregate quantity and a FIFO of slots.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Level {
    pub price: Price,
    pub qty: Qty,
    pub count: u32,
    pub head: u32,
    pub tail: u32,
}

/// FIFO of market orders resting during an auction call phase.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Queue {
    pub qty: Qty,
    pub count: u32,
    pub head: u32,
    pub tail: u32,
}

impl Queue {
    const EMPTY: Queue = Queue { qty: 0, count: 0, head: NIL, tail: NIL };
}

pub(crate) struct Book {
    pub nodes: Vec<Node>,
    free: Vec<u32>,
    pub index: IdMap<u32>,
    /// `levels[Buy]` ascending and `levels[Sell]` descending by price, so the best level
    /// of either side is `last()`.
    pub levels: [Vec<Level>; 2],
    /// Market orders resting in an auction call phase, per side, in time priority.
    pub market: [Queue; 2],
    next_prio: u64,
}

impl Book {
    pub fn with_capacity(orders: usize) -> Self {
        Book {
            nodes: Vec::with_capacity(orders),
            free: Vec::new(),
            index: IdMap::with_capacity_and_hasher(orders, Default::default()),
            levels: [Vec::with_capacity(256), Vec::with_capacity(256)],
            market: [Queue::EMPTY; 2],
            next_prio: 1,
        }
    }

    #[inline]
    pub fn slot(&self, id: OrderId) -> Option<u32> {
        self.index.get(&id).copied()
    }

    #[inline]
    pub fn live(&self) -> usize {
        self.index.len()
    }

    #[inline]
    pub fn free_len(&self) -> usize {
        self.free.len()
    }

    /// Store a node (unlinked) and index it by id.
    pub fn alloc(&mut self, node: Node) -> u32 {
        let id = node.id;
        let slot = match self.free.pop() {
            Some(s) => {
                self.nodes[s as usize] = node;
                s
            }
            None => {
                let s = u32::try_from(self.nodes.len()).expect("order arena exceeds u32 slots");
                assert!(s != NIL, "order arena exhausted");
                self.nodes.push(node);
                s
            }
        };
        let previous = self.index.insert(id, slot);
        debug_assert!(previous.is_none(), "duplicate order id {id}");
        slot
    }

    /// Forget an unlinked node and recycle its slot.
    pub fn release(&mut self, slot: u32) {
        let id = self.nodes[slot as usize].id;
        let removed = self.index.remove(&id);
        debug_assert_eq!(removed, Some(slot));
        self.free.push(slot);
    }

    #[inline]
    fn find_level(levels: &[Level], side: Side, price: Price) -> Result<usize, usize> {
        if let Some(last) = levels.last() {
            if last.price == price {
                return Ok(levels.len() - 1);
            }
        }
        match side {
            Side::Buy => levels.binary_search_by(|l| l.price.cmp(&price)),
            Side::Sell => levels.binary_search_by(|l| price.cmp(&l.price)),
        }
    }

    /// Append an allocated, unlinked node at the back of its queue with a fresh time
    /// priority (creating the price level if needed).
    pub fn link(&mut self, slot: u32) {
        let prio = self.next_prio;
        self.next_prio += 1;
        let node = &mut self.nodes[slot as usize];
        node.prio = prio;
        node.next = NIL;
        let (side, price, leaves, is_market) = (node.side, node.price, node.leaves, node.is_market);
        let (head, tail, qty, count) = if is_market {
            let q = &mut self.market[side.idx()];
            (&mut q.head, &mut q.tail, &mut q.qty, &mut q.count)
        } else {
            let levels = &mut self.levels[side.idx()];
            let i = match Self::find_level(levels, side, price) {
                Ok(i) => i,
                Err(i) => {
                    levels.insert(i, Level { price, qty: 0, count: 0, head: NIL, tail: NIL });
                    i
                }
            };
            let l = &mut levels[i];
            (&mut l.head, &mut l.tail, &mut l.qty, &mut l.count)
        };
        let old_tail = *tail;
        self.nodes[slot as usize].prev = old_tail;
        if old_tail == NIL {
            *head = slot;
        } else {
            self.nodes[old_tail as usize].next = slot;
        }
        *tail = slot;
        *qty += leaves;
        *count += 1;
    }

    /// Detach a linked node from its queue (removing an emptied level). The node stays
    /// allocated and indexed.
    pub fn unlink(&mut self, slot: u32) {
        let n = &self.nodes[slot as usize];
        let (side, price, leaves, is_market, prev, next) =
            (n.side, n.price, n.leaves, n.is_market, n.prev, n.next);
        if prev != NIL {
            self.nodes[prev as usize].next = next;
        }
        if next != NIL {
            self.nodes[next as usize].prev = prev;
        }
        if is_market {
            let q = &mut self.market[side.idx()];
            if q.head == slot {
                q.head = next;
            }
            if q.tail == slot {
                q.tail = prev;
            }
            q.qty -= leaves;
            q.count -= 1;
        } else {
            let levels = &mut self.levels[side.idx()];
            let i = Self::find_level(levels, side, price).expect("linked order has a level");
            let l = &mut levels[i];
            if l.head == slot {
                l.head = next;
            }
            if l.tail == slot {
                l.tail = prev;
            }
            l.qty -= leaves;
            l.count -= 1;
            if l.count == 0 {
                levels.remove(i);
            }
        }
        let n = &mut self.nodes[slot as usize];
        n.prev = NIL;
        n.next = NIL;
    }

    /// Reduce a linked node's open quantity in place (priority unchanged).
    /// Returns the new open quantity.
    pub fn reduce(&mut self, slot: u32, by: Qty) -> Qty {
        let n = &mut self.nodes[slot as usize];
        n.leaves -= by;
        let (side, price, is_market, leaves) = (n.side, n.price, n.is_market, n.leaves);
        if is_market {
            self.market[side.idx()].qty -= by;
        } else {
            let levels = &mut self.levels[side.idx()];
            let i = Self::find_level(levels, side, price).expect("linked order has a level");
            levels[i].qty -= by;
        }
        leaves
    }

    /// Detach the head order of the best level on `side`; its open quantity must already
    /// be zero (it was fully executed). Returns its slot, still allocated.
    pub fn pop_best_head(&mut self, side: Side) -> u32 {
        let levels = &mut self.levels[side.idx()];
        let l = levels.last_mut().expect("best level exists");
        let head = l.head;
        debug_assert_eq!(self.nodes[head as usize].leaves, 0);
        let next = self.nodes[head as usize].next;
        l.head = next;
        if next == NIL {
            l.tail = NIL;
        } else {
            self.nodes[next as usize].prev = NIL;
        }
        l.count -= 1;
        if l.count == 0 {
            levels.pop();
        }
        self.nodes[head as usize].next = NIL;
        head
    }

    pub fn snap(&self, slot: u32) -> OrderSnapshot {
        let n = &self.nodes[slot as usize];
        OrderSnapshot {
            order_id: n.id,
            owner: n.owner,
            side: n.side,
            price: if n.is_market { None } else { Some(n.price) },
            leaves: n.leaves,
            order_qty: n.order_qty,
            cum_qty: n.cum_qty,
        }
    }
}
