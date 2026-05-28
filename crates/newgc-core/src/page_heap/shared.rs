//! `SharedHeap` — the lock-free, atomically-accessed slice of the heap.
//!
//! Sprint MM-2 of the multi-mutator plan (`docs/MULTI_MUTATOR_DESIGN.md`
//! §2.0). These are the fields a mutator must touch **without** taking
//! the heap lock: the start-bit bitmap and card table (already atomic),
//! the poison flag, and the allocation counter. Extracting them into a
//! separate `Arc`-shared struct is the prerequisite for:
//!
//!   1. the lock-free TLAB fast path (MM-3) — bump + set-start-bit +
//!      poison-check + alloc-counter without locking, and
//!   2. the soundness of the collector's `&mut PageHeap` while mutators
//!      are parked — a mutator holds `Arc<SharedHeap>`, never a bare
//!      `&PageHeap`, so the two can't alias.
//!
//! MM-2 is a **pure refactor**: `PageHeap` now reaches these fields
//! through `self.shared`, with identical single-threaded behavior. The
//! `poisoned` flag becomes `AtomicBool` and `bytes_alloc_since_gc`
//! becomes `AtomicUsize`; everything still runs under the heap mutex
//! today, so the orderings (Acquire/Release on poison, Relaxed on the
//! counter) are conservative-correct and future-proof for MM-3.

use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::Arc;

use crate::heap_common::CardTable;

use super::alloc::PageStartBits;

/// Lock-free shared heap state. Cloned (via `Arc`) into `PageHeap` and,
/// from MM-3 on, into every `Mutator`. Not generic over the layout —
/// none of these fields depend on `L`.
pub struct SharedHeap {
    /// Set once a `try_collect_*` aborts on mid-evacuation OOM. Once
    /// poisoned, allocation refuses and further `try_collect_*` calls
    /// short-circuit. Acquire load / Release store.
    pub(crate) poisoned: AtomicBool,
    /// Bytes the mutator has allocated since the last collection. Drives
    /// `should_collect`. Relaxed — it's a heuristic trigger, and exact
    /// cross-thread freshness isn't required.
    pub(crate) bytes_alloc_since_gc: AtomicUsize,
    /// Global start-bit bitmap (2 bits/cell). Already atomic; mutators
    /// set starts via `fetch_or(Relaxed)`.
    pub(crate) start_bits: PageStartBits,
    /// Soft card table over the whole reservation. Atomic interior;
    /// `mark_card_at` is a Relaxed byte store.
    pub(crate) cards: Arc<CardTable>,
}

impl SharedHeap {
    /// Build the shared state for a fresh heap.
    pub(crate) fn new(start_bits: PageStartBits, cards: Arc<CardTable>) -> Self {
        Self {
            poisoned: AtomicBool::new(false),
            bytes_alloc_since_gc: AtomicUsize::new(0),
            start_bits,
            cards,
        }
    }
}
