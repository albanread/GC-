//! Multi-mutator front end — sprint MM-1.
//!
//! Introduces the *handle* API shape without changing performance or
//! the collector. A [`GcCoordinator`] owns the heap behind an
//! `Arc<Mutex<PageHeap>>` and hands out [`Mutator`] handles; any number
//! of threads can each hold a handle and allocate. Allocation is
//! **serialized by the heap mutex** in MM-1 — there are no per-thread
//! TLABs (MM-3) and no safepoints (MM-4) yet. The mutex also makes
//! allocation and collection mutually exclusive, so the collector still
//! sees a consistent heap.
//!
//! **Soundness caveat (until MM-5).** Collection roots still come from
//! the single closure passed to [`GcCoordinator::collect_minor`] et al.
//! With more than one live mutator, that closure must enumerate *every*
//! mutator's roots, or a mutator's live objects can be reclaimed.
//! Per-mutator root enumeration lands in MM-5; until then, multi-mutator
//! GC is only safe when the caller supplies all roots (or none). See
//! `docs/MULTI_MUTATOR_DESIGN.md` §8.

use std::marker::PhantomData;
use std::ptr::NonNull;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, RwLock};

use crate::traits::HeapLayout;
use crate::word::Word;

use super::alloc::{set_cons_start_bit_at, set_start_bit_at};
use super::cycle::{CollectResult, FullCollectResult};
use super::evac::{GcError, PageEvacuator};
use super::page_desc::{Generation, PageKind};
use super::pin::PinHandle;
use super::shared::SharedHeap;
use super::space::{PageHeap, PAGE_SIZE_CELLS};

/// Initial TLAB refill size in cells (4 KB). Doubles each refill up to
/// `MAX_TLAB_CELLS`.
const INITIAL_TLAB_CELLS: usize = 512;
/// Max TLAB size in cells (one 64 KB page).
const MAX_TLAB_CELLS: usize = PAGE_SIZE_CELLS;

/// `(gen_idx, kind_idx)` into the per-mutator `tlabs` array. Mirrors
/// `PageHeap::region_index` (kept local since that one is private).
#[inline]
fn region_index(generation: Generation, kind: PageKind) -> (usize, usize) {
    let gi = match generation {
        Generation::G0 => 0,
        Generation::G1 => 1,
        Generation::Tenured => 2,
        Generation::Free => unreachable!("Free has no alloc region"),
    };
    let ki = match kind {
        PageKind::Cons => 0,
        PageKind::Boxed => 1,
        _ => unreachable!("only Cons/Boxed have TLABs"),
    };
    (gi, ki)
}

/// A thread-local allocation buffer: a slab carved from the heap that
/// the owning mutator bumps **lock-free**. One per `(gen, kind)`.
/// `Copy` so the `[[Tlab; 2]; 3]` array initializes cheaply.
#[derive(Copy, Clone)]
struct Tlab {
    /// First cell of the slab (null = empty, triggers refill).
    start: *mut u64,
    /// Next free cell. Bumped by the fast path.
    cursor: *mut u64,
    /// One past the last cell of the slab.
    end: *mut u64,
    /// Page this slab lives on (for `words_used` reconciliation).
    page_idx: usize,
    /// Cells reserved at refill (for reconciling the unused tail).
    reserved_cells: u32,
    /// Size to request at the next refill (dynamic 4 KB → 64 KB).
    next_refill_cells: u32,
}

impl Tlab {
    const fn empty() -> Self {
        Self {
            start: std::ptr::null_mut(),
            cursor: std::ptr::null_mut(),
            end: std::ptr::null_mut(),
            page_idx: 0,
            reserved_cells: 0,
            next_refill_cells: INITIAL_TLAB_CELLS as u32,
        }
    }

    #[inline]
    fn room_cells(&self) -> usize {
        if self.start.is_null() {
            0
        } else {
            (self.end as usize - self.cursor as usize) / 8
        }
    }
}

/// Stable identifier for a registered mutator (index into the
/// coordinator's registry). Used by later sprints (MM-4) to look up a
/// mutator's safepoint state; in MM-1 it only drives slot lifecycle.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct MutatorId(usize);

impl MutatorId {
    /// The registry slot index.
    pub fn index(self) -> usize {
        self.0
    }
}

/// Per-mutator metadata in the coordinator's registry. Minimal in MM-1
/// (existence only). MM-3 adds TLAB cursors; MM-4 adds `last_epoch` /
/// `is_active` / `state` for the safepoint protocol.
#[derive(Default)]
struct MutatorInner {
    // Intentionally empty in MM-1.
}

/// Shared mutator registry. A free slot is reused before the vector
/// grows, so `MutatorId`s stay small and dense.
struct Registry {
    slots: RwLock<Vec<Option<Arc<MutatorInner>>>>,
}

impl Registry {
    fn new() -> Self {
        Self {
            slots: RwLock::new(Vec::new()),
        }
    }

    fn register(&self) -> (MutatorId, Arc<MutatorInner>) {
        let inner = Arc::new(MutatorInner::default());
        let mut slots = self.slots.write().unwrap();
        // Reuse the first free slot, else push.
        let id = match slots.iter().position(|s| s.is_none()) {
            Some(i) => {
                slots[i] = Some(Arc::clone(&inner));
                i
            }
            None => {
                slots.push(Some(Arc::clone(&inner)));
                slots.len() - 1
            }
        };
        (MutatorId(id), inner)
    }

    fn deregister(&self, id: MutatorId) {
        let mut slots = self.slots.write().unwrap();
        if let Some(slot) = slots.get_mut(id.0) {
            *slot = None;
        }
    }

    fn live_count(&self) -> usize {
        self.slots.read().unwrap().iter().filter(|s| s.is_some()).count()
    }
}

/// Owns the heap and hands out mutator handles. `Clone` is cheap (it
/// clones the inner `Arc`s) so each thread can hold its own coordinator
/// handle and register a mutator locally. `Send + Sync`.
pub struct GcCoordinator<L: HeapLayout> {
    heap: Arc<Mutex<PageHeap<L>>>,
    registry: Arc<Registry>,
}

impl<L: HeapLayout> Clone for GcCoordinator<L> {
    fn clone(&self) -> Self {
        Self {
            heap: Arc::clone(&self.heap),
            registry: Arc::clone(&self.registry),
        }
    }
}

impl<L: HeapLayout> GcCoordinator<L> {
    /// Build a coordinator over a heap with `young_bytes` + `old_bytes`
    /// (mirrors [`PageHeap::new`]).
    pub fn new(young_bytes: usize, old_bytes: usize) -> Self {
        Self::from_heap(PageHeap::<L>::new(young_bytes, old_bytes))
    }

    /// Build a coordinator over a heap reserving `reserved_bytes`
    /// (mirrors [`PageHeap::with_reservation`]).
    pub fn with_reservation(reserved_bytes: usize) -> Self {
        Self::from_heap(PageHeap::<L>::with_reservation(reserved_bytes))
    }

    fn from_heap(heap: PageHeap<L>) -> Self {
        Self {
            heap: Arc::new(Mutex::new(heap)),
            registry: Arc::new(Registry::new()),
        }
    }

    /// Register a mutator on the current thread. The returned
    /// [`Mutator`] is `!Send` — keep it on this thread. Any number of
    /// threads may register concurrently.
    pub fn register_mutator(&self) -> Mutator<L> {
        let (id, inner) = self.registry.register();
        // Cache the lock-free handles for the bump fast path (one heap
        // lock, at registration only).
        let (shared, base_addr) = {
            let h = self.heap.lock().unwrap();
            (h.shared_handle(), h.base_ptr() as usize)
        };
        Mutator {
            heap: Arc::clone(&self.heap),
            shared,
            base_addr,
            registry: Arc::clone(&self.registry),
            id,
            tlabs: [[Tlab::empty(); 2]; 3],
            tlab_refills: 0,
            _inner: inner,
            _not_send: PhantomData,
        }
    }

    /// Number of currently-registered (live) mutators.
    pub fn mutator_count(&self) -> usize {
        self.registry.live_count()
    }

    /// Run a closure with exclusive `&mut PageHeap`. Locks the heap
    /// mutex for the duration — allocation by any mutator is excluded.
    /// Escape hatch for diagnostics/tests; collection should go through
    /// the `collect_*` wrappers.
    pub fn with_heap<R>(&self, f: impl FnOnce(&mut PageHeap<L>) -> R) -> R {
        f(&mut self.heap.lock().unwrap())
    }

    // -- Collection entries (MM-1: lock + delegate) ----------------------
    //
    // In MM-4 the trigger moves onto `Mutator` (the driver self-parks);
    // for MM-1, with no safepoints, collection is driven here and the
    // caller supplies roots via the closure exactly as the single-mutator
    // `PageHeap::collect_*` does. The heap mutex serializes collection
    // against allocation.

    /// Minor collection. The closure must visit every live root (across
    /// all mutators) — see the module soundness caveat.
    pub fn collect_minor<F>(&self, visit_roots: F) -> CollectResult
    where
        F: FnMut(&mut PageEvacuator<'_, L>),
    {
        self.heap.lock().unwrap().collect_minor(visit_roots)
    }

    /// Major collection (G1→Tenured, then G0→G0).
    pub fn collect_major<F>(&self, visit_roots: F) -> CollectResult
    where
        F: FnMut(&mut PageEvacuator<'_, L>),
    {
        self.heap.lock().unwrap().collect_major(visit_roots)
    }

    /// Full collection (force-promote + compact Tenured).
    pub fn collect_full<F>(&self, visit_roots: F) -> FullCollectResult
    where
        F: FnMut(&mut PageEvacuator<'_, L>),
    {
        self.heap.lock().unwrap().collect_full(visit_roots)
    }

    /// Trigger-policy-driven collection (minor or major per heap state).
    pub fn collect_auto<F>(&self, visit_roots: F) -> CollectResult
    where
        F: FnMut(&mut PageEvacuator<'_, L>),
    {
        self.heap.lock().unwrap().collect_auto(visit_roots)
    }

    /// Recoverable minor collection — `Err` on mid-evac OOM (and the
    /// heap is poisoned thereafter; see [`PageHeap::is_poisoned`]).
    pub fn try_collect_minor<F>(&self, visit_roots: F) -> Result<CollectResult, GcError>
    where
        F: FnMut(&mut PageEvacuator<'_, L>),
    {
        self.heap.lock().unwrap().try_collect_minor(visit_roots)
    }

    /// Recoverable major collection.
    pub fn try_collect_major<F>(&self, visit_roots: F) -> Result<CollectResult, GcError>
    where
        F: FnMut(&mut PageEvacuator<'_, L>),
    {
        self.heap.lock().unwrap().try_collect_major(visit_roots)
    }

    /// Recoverable auto collection.
    pub fn try_collect_auto<F>(&self, visit_roots: F) -> Result<CollectResult, GcError>
    where
        F: FnMut(&mut PageEvacuator<'_, L>),
    {
        self.heap.lock().unwrap().try_collect_auto(visit_roots)
    }

    /// True if a previous `try_collect_*` poisoned the heap.
    pub fn is_poisoned(&self) -> bool {
        self.heap.lock().unwrap().is_poisoned()
    }
}

/// Per-thread allocation handle. `!Send + !Sync` — bound to the thread
/// that registered it. In MM-1 every operation locks the shared heap
/// mutex; MM-3 adds a lock-free TLAB fast path.
pub struct Mutator<L: HeapLayout> {
    heap: Arc<Mutex<PageHeap<L>>>,
    /// Lock-free shared state (start bits, poison flag, alloc counter).
    /// The bump fast path touches only this — no heap lock (MM-3).
    shared: Arc<SharedHeap>,
    /// Reservation base address, cached for global-cell-index math.
    base_addr: usize,
    registry: Arc<Registry>,
    id: MutatorId,
    /// Per-`(gen, kind)` thread-local allocation buffers.
    tlabs: [[Tlab; 2]; 3],
    /// Count of TLAB refills (each takes the heap lock once). Diagnostic
    /// — lets tests verify the bump fast path amortizes the lock.
    tlab_refills: u64,
    _inner: Arc<MutatorInner>,
    _not_send: PhantomData<*mut ()>,
}

impl<L: HeapLayout> Mutator<L> {
    /// This mutator's stable id.
    pub fn id(&self) -> MutatorId {
        self.id
    }

    /// Allocate a cons cell (2 cells) in `generation`. Lock-free bump
    /// in the common case; locks the heap only to refill an exhausted
    /// TLAB. Returns `None` on OOM or if the heap is poisoned.
    #[inline]
    pub fn try_alloc_cons_in(&mut self, generation: Generation) -> Option<NonNull<u64>> {
        self.bump(generation, PageKind::Cons, 2, /*is_cons=*/ true)
    }

    /// Allocate an `n_cells` boxed object (header + payload) in
    /// `generation`. Lock-free bump; refill on exhaustion.
    #[inline]
    pub fn try_alloc_boxed_in(
        &mut self,
        generation: Generation,
        n_cells: usize,
    ) -> Option<NonNull<u64>> {
        if n_cells == 0 || n_cells > PAGE_SIZE_CELLS {
            return None;
        }
        self.bump(generation, PageKind::Boxed, n_cells, /*is_cons=*/ false)
    }

    /// Allocate a large (≥ one page) object in `generation`. Large
    /// objects bypass TLABs and go through the central path under the
    /// heap lock.
    pub fn try_alloc_large(
        &mut self,
        n_cells: usize,
        generation: Generation,
    ) -> Option<NonNull<u64>> {
        self.heap.lock().unwrap().try_alloc_large(n_cells, generation)
    }

    /// The lock-free bump fast path. On a hit it advances the TLAB
    /// cursor, sets the object's start bit (atomic `fetch_or`), bumps
    /// the alloc counter (atomic), and returns — **no heap lock**. On a
    /// miss it refills (one heap lock) and retries.
    #[inline]
    fn bump(
        &mut self,
        generation: Generation,
        kind: PageKind,
        n_cells: usize,
        is_cons: bool,
    ) -> Option<NonNull<u64>> {
        // Poison check is lock-free (Acquire on the shared flag).
        if self.shared.poisoned.load(Ordering::Acquire) {
            return None;
        }
        let (gi, ki) = region_index(generation, kind);
        loop {
            // Fast path: room in the current TLAB.
            if self.tlabs[gi][ki].room_cells() >= n_cells {
                let ptr = self.tlabs[gi][ki].cursor;
                self.tlabs[gi][ki].cursor = unsafe { ptr.add(n_cells) };
                let cell_idx = (ptr as usize - self.base_addr) / 8;
                if is_cons {
                    set_cons_start_bit_at(&self.shared.start_bits, cell_idx);
                } else {
                    set_start_bit_at(&self.shared.start_bits, cell_idx);
                }
                self.shared
                    .bytes_alloc_since_gc
                    .fetch_add(n_cells * 8, Ordering::Relaxed);
                return Some(unsafe { NonNull::new_unchecked(ptr) });
            }
            // Slow path: refill and retry. If refill fails (OOM / cap /
            // poison), give up.
            if !self.refill(generation, kind, gi, ki, n_cells) {
                return None;
            }
        }
    }

    /// Refill the `(gi, ki)` TLAB with a fresh slab carved under the
    /// heap lock. Grows the request 4 KB → 64 KB across successive
    /// refills. Returns false on OOM / young-cap / poison.
    #[cold]
    fn refill(
        &mut self,
        generation: Generation,
        kind: PageKind,
        gi: usize,
        ki: usize,
        min_cells: usize,
    ) -> bool {
        let want = (self.tlabs[gi][ki].next_refill_cells as usize).max(min_cells);
        let slab = {
            let mut heap = self.heap.lock().unwrap();
            heap.reserve_tlab(generation, kind, min_cells, want)
        };
        self.tlab_refills += 1;
        match slab {
            Some((ptr, page_idx, cells)) => {
                let next = ((self.tlabs[gi][ki].next_refill_cells as usize) * 2)
                    .min(MAX_TLAB_CELLS) as u32;
                let start = ptr.as_ptr();
                self.tlabs[gi][ki] = Tlab {
                    start,
                    cursor: start,
                    end: unsafe { start.add(cells) },
                    page_idx,
                    reserved_cells: cells as u32,
                    next_refill_cells: next,
                };
                true
            }
            None => false,
        }
    }

    /// Reconcile every TLAB's unused tail back into its page's
    /// `words_used` and clear the TLABs (next alloc refills fresh).
    /// **Must be called before a collection** while this mutator holds
    /// live TLABs — the cursor would otherwise dangle if GC moved the
    /// TLAB's page. MM-4's safepoint protocol calls this automatically;
    /// in MM-3 a single-mutator client calls it explicitly before
    /// `GcCoordinator::collect_*`.
    pub fn flush_tlabs(&mut self) {
        let mut heap = self.heap.lock().unwrap();
        for gi in 0..3 {
            for ki in 0..2 {
                let t = self.tlabs[gi][ki];
                if t.start.is_null() {
                    continue;
                }
                let used = (t.cursor as usize - t.start as usize) / 8;
                let unused = (t.reserved_cells as usize).saturating_sub(used);
                if unused > 0 {
                    let d = heap.desc_mut(t.page_idx);
                    d.words_used = d.words_used.saturating_sub(unused as u16);
                }
                self.tlabs[gi][ki] = Tlab::empty();
            }
        }
    }

    /// Number of TLAB refills this mutator has performed (each took the
    /// heap lock once). Diagnostic / test hook.
    pub fn tlab_refill_count(&self) -> u64 {
        self.tlab_refills
    }

    /// Card barrier — mark the card covering `slot_addr`.
    pub fn mark_card_at(&self, slot_addr: *const u8) {
        self.heap.lock().unwrap().mark_card_at(slot_addr);
    }

    /// Explicit FFI pin (MM-0). Keeps `w`'s target fixed until `unpin`.
    pub fn pin(&mut self, w: Word) -> PinHandle {
        self.heap.lock().unwrap().pin(w)
    }

    /// Release an explicit pin.
    pub fn unpin(&mut self, handle: PinHandle) {
        self.heap.lock().unwrap().unpin(handle);
    }
}

impl<L: HeapLayout> Drop for Mutator<L> {
    fn drop(&mut self) {
        // Deregister the slot. MM-1 touches no heap state on drop; the
        // STW-aware drop (deregister + notify) lands with the safepoint
        // protocol in MM-4.
        self.registry.deregister(self.id);
    }
}
