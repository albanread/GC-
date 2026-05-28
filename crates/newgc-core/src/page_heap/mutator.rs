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
use std::sync::{Arc, Mutex, RwLock};

use crate::traits::HeapLayout;
use crate::word::Word;

use super::cycle::{CollectResult, FullCollectResult};
use super::evac::{GcError, PageEvacuator};
use super::page_desc::Generation;
use super::pin::PinHandle;
use super::space::PageHeap;

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
        Mutator {
            heap: Arc::clone(&self.heap),
            registry: Arc::clone(&self.registry),
            id,
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
    registry: Arc<Registry>,
    id: MutatorId,
    _inner: Arc<MutatorInner>,
    _not_send: PhantomData<*mut ()>,
}

impl<L: HeapLayout> Mutator<L> {
    /// This mutator's stable id.
    pub fn id(&self) -> MutatorId {
        self.id
    }

    /// Allocate a cons cell in `generation`. Returns `None` on OOM or if
    /// the heap is poisoned.
    pub fn try_alloc_cons_in(&mut self, generation: Generation) -> Option<NonNull<u64>> {
        self.heap.lock().unwrap().try_alloc_cons_in(generation)
    }

    /// Allocate an `n_cells` boxed object (header + payload) in `generation`.
    pub fn try_alloc_boxed_in(
        &mut self,
        generation: Generation,
        n_cells: usize,
    ) -> Option<NonNull<u64>> {
        self.heap.lock().unwrap().try_alloc_boxed_in(generation, n_cells)
    }

    /// Allocate a large (≥ one page) object in `generation`.
    pub fn try_alloc_large(
        &mut self,
        n_cells: usize,
        generation: Generation,
    ) -> Option<NonNull<u64>> {
        self.heap.lock().unwrap().try_alloc_large(n_cells, generation)
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
