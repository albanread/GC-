# Multi-Mutator Design for NewGC's PageHeap

**Status:** Draft v2 — incorporates red-team critique from 2026-05-27.
No code yet. Supersedes the "Roadmap to true multi-thread support"
section of `THREADING.md` (whose 5-step ladder this doc refines into a
phased, landable plan).

**Revision history:**
- v2 (2026-05-27): Added explicit happens-before audit for TLAB
  retirement → coordinator reads (§4.4). Added `coord_mutex` to
  serialize coordinator entries. Added unpark sequence diagram (§4.4).
  Tightened `poisoned` migration language: explicit
  `AtomicBool::load(Acquire)` at mutator-side check sites (§2.5, §7).
  Recommendation: TLAB retirement happens under `alloc_mutex` for
  belt-and-braces happens-before (§4.4).
- v1 (2026-05-27): Initial draft.

**Scope:** Make `PageHeap<L>` safe and efficient for *N concurrent
mutator threads* allocating into one shared heap, with a stop-the-world
collector that parks all mutators at a safepoint, runs, and resumes
them. The collector remains single-threaded internally.

---

## 1. Goals and non-goals

### Goals

1. **Concurrent allocation fast path.** Two mutator threads allocating
   cons cells / boxed objects into G0 must not contend on a lock in the
   common case. Each thread bumps within a private TLAB.
2. **Cooperative parking.** When the collector needs to run, every
   mutator reaches a safepoint and blocks. Mutators that are inside the
   GC's parked region cannot perform any heap action — allocation,
   card-marking on a moving target, root mutation — that would race the
   collector.
3. **Per-mutator root enumeration.** The collector enumerates every
   parked mutator's roots, not a single global root set.
4. **Preserve the poison contract.** After a `try_collect_*` returns
   `Err(GcError::MidEvacOom | HeapPoisoned)`, subsequent allocation and
   GC calls from *any* mutator behave identically to the single-mutator
   case (they refuse). The poison flag becomes visible to every mutator.
5. **Preserve `young_page_cap`.** The page-cap accounting that gates G0
   growth must still work, including the `recycle_live_counts_active_for(G0)`
   bypass for GC-internal evacuation.
6. **Keep all of `tests/threading.rs` passing** (with at most a tiny
   diff to swap `Mutex<PageHeap>::lock().try_alloc_*` patterns for the
   new `Mutator` API where the test specifically asserts the *current*
   shape).

### Non-goals (explicitly excluded from this design)

- **Concurrent GC.** The collector remains STW. Mutators do not allocate
  or read pointers while the collector runs.
- **Parallel mark/evac.** The collector is single-threaded internally.
  A future doc could parallelize phase 1 copying or the mark BFS; this
  one does not.
- **Lock-free GC.** No CAS-based concurrent BFS, no work-stealing
  evacuator queues.
- **Pre-emptive parking.** Mutators only park at *cooperative* safepoint
  polls. No signals, no `SuspendThread`, no debug-trap insertion. A
  mutator running an unbounded loop without a poll will block GC forever
  — this is a deliberate trade-off documented in §4.
- **Cross-platform TLS optimization.** Mutators are explicit `Mutator<L>`
  handles passed by the client; we do not maintain a `thread_local!`
  back-pointer to the current mutator. Clients that want
  thread-local-by-default can wrap.

---

## 2. API surface

### 2.1 The `Mutator<L>` handle

A `Mutator<L>` is the per-thread allocation handle. It owns the
mutator's TLABs and its safepoint-pending flag. It holds a shared
reference to the `PageHeap` and is `!Send + !Sync` — a mutator is bound
to the thread that registered it.

```rust
pub struct Mutator<L: HeapLayout> {
    /// Strong reference into the shared heap. Mutators outlive the
    /// `register_mutator` call but the heap must outlive every mutator.
    heap: Arc<PageHeap<L>>,
    /// Stable identifier (index into PageHeap::mutators). Used by the
    /// coordinator to look up this mutator's metadata.
    id: MutatorId,
    /// Cached start-bits Arc (avoids one indirection per cons alloc).
    start_bits: PageStartBits,
    /// Per-(gen, kind) TLABs. See §3 — 6 entries total, indexed by
    /// `region_index(gen, kind)`.
    tlabs: [[Tlab; 2]; 3],
    /// Roots provider — see §5. Client supplies one of:
    ///   - A closure stashed at registration that walks this thread's
    ///     stack into a Vec<Word>.
    ///   - An explicit Vec<Word> updated by the client before each
    ///     safepoint poll (sets it via `mutator.publish_roots`).
    roots: RootsSource<L>,
    /// !Send + !Sync marker.
    _not_send: PhantomData<*mut ()>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct MutatorId(u32);

/// Internal TLAB cache for one (gen, kind) pair.
struct Tlab {
    /// First cell of the TLAB. Null while empty.
    start: *mut u64,
    /// One past the last cell (exclusive). bump < end means there is
    /// room for at least one cell.
    end: *mut u64,
    /// Current bump pointer.
    bump: *mut u64,
    /// Page index this TLAB sits on, for words_used accounting at
    /// refill / retirement.
    page_idx: usize,
    /// Cells reserved at refill (so we can compute "cells consumed"
    /// at retirement and reconcile words_used).
    reserved_cells: u32,
    /// Cells already accounted toward this page's words_used. Bumped
    /// to current usage at retirement, so the page descriptor is
    /// accurate when GC reads it. Inside the TLAB the bump pointer is
    /// the source of truth.
    accounted_cells: u32,
}
```

#### Drop behavior

When a `Mutator<L>` is dropped:

1. Each TLAB is **retired**: its unused tail is logically "skipped"
   (cells beyond the bump pointer have no start bits set, so GC walkers
   skip them — same trick the existing `try_alloc_g0_cons_slab` uses).
2. The TLAB's page's `words_used` is reconciled to the actual cells
   consumed by this mutator (delta = `(bump - start)/8 -
   accounted_cells`). This delta is folded into the shared
   `PageDesc::words_used`.
3. The mutator deregisters from `PageHeap::mutators` (slot becomes
   `None`).

Dropping a mutator while the collector holds the "world is stopped"
lock would deadlock. The `Drop` impl asserts (debug only) that the
collector is not currently running.

### 2.2 Registration

**Recommended shape:**

```rust
impl<L: HeapLayout> PageHeap<L> {
    /// Register a new mutator on the current thread. The returned
    /// `Mutator<L>` is `!Send + !Sync` — keep it on the thread that
    /// called this method.
    ///
    /// Takes `&Arc<Self>` so multiple mutators can be registered
    /// against one heap. Internally takes the `mutators` mutex.
    pub fn register_mutator(self: &Arc<Self>, roots: RootsSource<L>)
        -> Mutator<L>;
}
```

**Why `&Arc<Self>` not `&mut self`:** The whole point of multi-mutator
is to let several threads hold a handle simultaneously. `&mut self`
would force serialization through `Mutex<PageHeap>`, defeating the
exercise. We accept that this means `PageHeap` carries some
`Mutex`/`Atomic` interior mutability that did not exist before (see
§7 audit).

### 2.3 The GC entry shape — **recommendation: option (a), `&mut self`**

This is the most consequential API decision in the doc. Two shapes
were considered:

**(a) GC requires `&mut self`.** The collector takes `&mut PageHeap`,
which means *the user must drop every live `Mutator<L>` (or, more
practically, every borrow they hold) before calling `collect_*`*.
Because `Mutator<L>` holds an `Arc<PageHeap>`, the user must instead
arrange that all mutators have reached a *safepoint and released the
PageHeap mutator lock they hold*. The collector then **takes a
heap-wide write lock** (`RwLock<PageHeap>` or a hand-rolled equivalent)
to get exclusive access.

**(b) GC takes `&self`.** Mutators don't have to be released; the
collector parks them via safepoint and then runs against `&self`,
using internal `&mut` on the relevant sub-fields through interior
mutability.

**Recommendation: (a), with a wrapper.** We keep `collect_minor`,
`collect_major`, `collect_full`, `try_collect_*` taking `&mut self`
on `PageHeap` — *no change to their signatures* — and add a new top-
level coordinator type:

```rust
pub struct GcCoordinator<L: HeapLayout> {
    heap: Arc<PageHeap<L>>,
    /// Safepoint state; see §4.
    safepoint: Arc<Safepoint>,
}

impl<L: HeapLayout> GcCoordinator<L> {
    pub fn new(heap: Arc<PageHeap<L>>) -> Self { ... }

    /// Park every registered mutator, drop the world-stop barrier on
    /// them, take exclusive access to the heap, run the closure with
    /// `&mut PageHeap`, then resume mutators.
    pub fn with_world_stopped<R, F: FnOnce(&mut PageHeap<L>) -> R>(
        &self,
        f: F,
    ) -> R;

    /// Convenience wrappers — same names as today.
    pub fn collect_minor<F>(&self, visit_roots: F) -> CollectResult
    where F: FnMut(&mut PageEvacuator<'_, L>);

    pub fn try_collect_minor<F>(&self, visit_roots: F)
        -> Result<CollectResult, GcError>
    where F: FnMut(&mut PageEvacuator<'_, L>);

    // ... collect_major, collect_full, collect_auto, and try_ variants ...
}
```

**Why (a) wins:**

1. The collector mutates *enormous* amounts of state — page descriptors,
   start bits, alloc regions, mark bits, recycle counts, pin sets, card
   tables, the poison flag. Making every one of those atomic just to
   pretend the collector is `&self` is bookkeeping cost we never pay back.
   STW means the collector *is* the exclusive owner; the type system
   should say so.
2. The existing `try_collect_*` poison contract is built on `&mut self`:
   the heap sets `poisoned = true` and that store is visible because no
   other thread can be looking at it. Under option (b) the poison store
   would need to be `AtomicBool`, fences would need to be added at the
   collector exit and at every mutator's allocation entry — extra
   complexity for no win.
3. The `&mut PageHeap` closures `collect_minor` accepts today
   (`F: FnMut(&mut PageEvacuator)`) keep working unchanged.
4. The world-stop lock is a perfectly good place to centralise the
   "drain all TLABs, scan all roots, run the cycle, refill no one"
   protocol.

**The cost of (a):** mutator-side allocation cannot hold any reference
into `PageHeap` while the collector is running. Concretely, this means
TLAB refill must release its lock on the heap before bumping the TLAB.
That's fine: the TLAB itself lives on the mutator side, and the lock
on the heap is only held during refill, not during bump.

### 2.4 Allocation from the mutator side

```rust
impl<L: HeapLayout> Mutator<L> {
    /// Allocate a cons cell. Returns None on OOM or if the heap is
    /// poisoned. Equivalent to the current `try_alloc_cons_in(G0)`
    /// but operates from a private TLAB.
    pub fn try_alloc_cons_in(&mut self, gen: Generation)
        -> Option<NonNull<u64>>;

    /// Allocate a boxed object of `n_cells` cells. Caller writes the
    /// header. Sets the boxed start bit on the returned cell.
    pub fn try_alloc_boxed_in(&mut self, gen: Generation, n_cells: usize)
        -> Option<NonNull<u64>>;

    /// Allocate a large object (≥ one page). Bypasses the TLAB and
    /// goes through the heap's central large-object path under the
    /// allocation mutex.
    pub fn try_alloc_large(&mut self, n_cells: usize, gen: Generation)
        -> Option<NonNull<u64>>;

    /// Card barrier. Forwards to `PageHeap::mark_card_at`, which
    /// already takes `&self` and is concurrent-safe.
    #[inline]
    pub fn mark_card_at(&self, slot_addr: *const u8) {
        self.heap.mark_card_at(slot_addr);
    }

    /// Explicit safepoint poll. Cheap (one relaxed load + compare).
    /// Clients call this at safe points in their interpreter loop /
    /// JIT back-edges. See §4.
    #[inline]
    pub fn poll_safepoint(&mut self);

    /// Update the snapshot of root words this mutator wants the
    /// collector to scan/update at its next safepoint. See §5.
    pub fn publish_roots(&mut self, roots: &[Word]);
}
```

`Generation` other than G0 is unusual from the mutator side (G1/Tenured
allocs are normally GC-internal during evacuation); we still allow them
because the existing API does and the cost is one extra TLAB array slot
per mutator. They can be cheap "rarely refilled" TLABs.

### 2.5 `try_collect_*` interaction

```rust
impl<L: HeapLayout> GcCoordinator<L> {
    pub fn try_collect_minor<F>(&self, visit_roots: F)
        -> Result<CollectResult, GcError>
    where F: FnMut(&mut PageEvacuator<'_, L>),
    {
        self.with_world_stopped(|heap| heap.try_collect_minor(visit_roots))
    }
}
```

The poison contract is unchanged in shape; only the concurrency
discipline is added:
- `with_world_stopped` parks all mutators, then calls
  `PageHeap::try_collect_minor` (existing method, unchanged).
- If that returns `Err`, the heap has set
  `self.poisoned.store(true, Ordering::Release)` (this is the only
  writer; runs while world-stopped).
- The world-resume `world_running.store(1, Release)` is sequenced after
  the poison store. Every mutator's unpark waits on a matching
  `world_running.load(Acquire)`, which establishes happens-before with
  the poison store.
- Mutator-side checks are `self.heap.is_poisoned()` which expands to
  `self.heap.poisoned.load(Ordering::Acquire)` — see §7. This is *one
  extra Acquire load per allocation* in the simplest implementation;
  see §9 D-POISON-CHECK for the option to only check at TLAB refill.

**Migration note:** the existing implementation (this PR's parent
commit) uses `poisoned: bool` (plain). Phase 1 of this design promotes
it to `AtomicBool`. Every existing `if self.poisoned { return None; }`
allocator gate becomes `if self.is_poisoned() { return None; }` with
`is_poisoned` returning `load(Acquire)`. The single STW writer
becomes `self.poisoned.store(true, Release)` at the existing site
(`run_catching_oom`'s `Err` branch).

---

## 3. TLAB design

### 3.1 Size: **dynamic, capped at one page**

- **Initial refill size:** 4 KB (512 cells). Small enough that a
  short-lived task doesn't waste a page; large enough to amortise the
  refill cost over ~256 cons allocations.
- **Growth:** Each subsequent refill doubles, capped at `PAGE_SIZE_CELLS`
  (8192 cells = 64 KB). Mutators that allocate heavily quickly reach a
  full-page TLAB.
- **No shrink.** A mutator that goes idle keeps its current TLAB size;
  the unused tail is reclaimed at the next GC (no start bits set →
  walker skips it).

**Rationale:** Open decision; see §9 D-TLAB-SIZE.

### 3.2 How many TLABs per mutator: **6 — one per (Generation, PageKind) ∈ {G0,G1,Tenured} × {Cons,Boxed}**

This mirrors the existing six `AllocRegion` slots. Each TLAB is bound
to one (gen, kind), so the start-bit semantics and page-kind invariants
are unambiguous. G1/Tenured TLABs will rarely refill from the mutator
side — they're hot only during GC-internal evacuation, which keeps
running through the central `AllocRegion` path under the world-stopped
lock (no contention).

Alternative considered: one mixed TLAB per generation (slab-style, like
the existing `try_alloc_g0_cons_slab`). Rejected because boxed-vs-cons
pages have different walker semantics — the existing slab path uses a
`Boxed` page for mixed content and relies on per-cell start bits to
distinguish. That works but spends a 64 KB page on every TLAB even for
pure cons workloads, where a dedicated cons-page TLAB has *zero* start
bits and faster walks.

### 3.3 Refill protocol

```rust
// Pseudocode for mutator-side bump:
fn try_alloc_cons_in(&mut self, gen: Generation) -> Option<NonNull<u64>> {
    if self.heap.is_poisoned() {  // Acquire load on AtomicBool
        return None;
    }
    self.poll_safepoint();        // §4 — may park

    let tlab = &mut self.tlabs[gen_idx(gen)][kind_idx(PageKind::Cons)];
    let n_cells = 2;
    if tlab.bump.wrapping_add(n_cells) <= tlab.end {
        let p = tlab.bump;
        tlab.bump = unsafe { tlab.bump.add(n_cells) };
        // Set cons start bit via Arc<[AtomicU64]> — no heap lock needed
        set_cons_start_bit_at(&self.start_bits, global_cell_idx(p));
        return Some(unsafe { NonNull::new_unchecked(p) });
    }
    // Slow path: refill.
    self.refill_tlab(gen, PageKind::Cons, n_cells)
}
```

**`refill_tlab`** is the only mutator operation that takes a heap-side
lock:

1. Compute requested refill size (4 KB → 8 KB → ... → 64 KB).
2. Take `PageHeap::alloc_mutex` (new — a `Mutex<()>` guarding the
   central `AllocRegion` and free-page acquisition).
3. **Retire the old TLAB:** reconcile `accounted_cells` against the
   bump pointer and write the delta back to `PageDesc::words_used`.
4. **Check young_page_cap:** if `gen == G0` and acquiring a new G0
   page would push `count_pages_in_gen(G0)` past `young_page_cap`, AND
   the recycle-counts-bypass is not active, **fail the refill**
   (return `None`). The mutator must then trigger or wait for GC. See
   §3.5.
5. **Acquire cells:** call into the existing
   `try_alloc_g0_cons_slab(refill_size)` (or boxed variant). This may
   open a new page or reuse the current `AllocRegion` page.
6. **Install:** update the TLAB's `start`, `end`, `bump`, `page_idx`,
   `reserved_cells`, `accounted_cells = reserved_cells` (the page is
   pre-charged for the whole TLAB; mutator-side bumps don't touch
   `words_used` until retirement reconciles).
7. Release `alloc_mutex`.
8. Retry the bump (which now fits).

**Why pre-charge `words_used` at refill, not at each bump:** Because
`words_used` lives in the shared `PageDesc`, atomic-incrementing it on
every cons alloc would cost a full atomic RMW. By pre-charging the
whole TLAB at refill and reconciling at retirement, the mutator's
fast path touches *only* mutator-private memory and the global start-
bit bitmap (atomic OR — cheap and already concurrent-safe).

The cost: between refill and retirement, `PageDesc::words_used` slightly
overstates the live data in the page. GC, when it runs (with mutators
parked, TLABs retired by the parking protocol — see §4), sees an
accurate value because parking forces retirement.

### 3.4 Large-object alloc: **stays direct, behind `alloc_mutex`**

Large objects (>= one page) bypass TLABs. `Mutator::try_alloc_large`
takes `alloc_mutex` and calls `PageHeap::try_alloc_large` (existing).
Large is so rare and so expensive per call that lock contention is
negligible.

### 3.5 `young_page_cap` race handling

Today, `young_page_cap` is checked inside `try_alloc_in_region` /
`try_alloc_large`. With concurrent mutators, two TLAB refills could
both observe "G0 has cap-1 pages, OK to grow" and each open one,
pushing G0 over the cap.

**Fix:** the cap check happens *inside* `alloc_mutex`, and reads
`count_pages_in_gen(G0)` while holding the mutex. Because TLAB refills
serialize on this mutex, only one refill can be "deciding to grow G0"
at a time.

`recycle_live_counts_active_for(G0)` returns true only during a GC
cycle — which means `with_world_stopped` is held — which means no
mutator can be running a refill — so no race on that flag.

---

## 4. Safepoint protocol

### 4.1 Atomic flag location: **per-mutator + global epoch**

```rust
pub(crate) struct Safepoint {
    /// Global epoch. Coordinator increments when requesting a safepoint;
    /// mutators compare against their last-seen value.
    epoch: AtomicU64,
    /// Number of mutators that have arrived at the current safepoint
    /// epoch. Coordinator waits until this equals
    /// `mutators.read().len()`.
    parked_count: AtomicUsize,
    /// Coordinator-controlled flag that mutators wait on after parking.
    /// 0 = world is stopped, mutators must block. 1 = world is running.
    world_running: AtomicU8,
    /// Park/unpark coordination — Mutex + Condvar pair (Linux/Win
    /// portable; std).
    park_mutex: Mutex<()>,
    park_cv: Condvar,
}
```

And per-mutator:

```rust
struct MutatorInner {  // lives in PageHeap::mutators[id]
    /// Last safepoint epoch this mutator observed and acted on. When
    /// `Safepoint::epoch > self.last_epoch`, this mutator owes a park.
    last_epoch: AtomicU64,
    /// Most-recent roots snapshot, owned by the mutator, read by
    /// the collector. See §5.
    roots_snapshot: Mutex<Vec<Word>>,
    /// Back-channel handle the mutator uses to notify the coordinator
    /// it has parked. Just a `parked_count.fetch_add(1)` plus a wake
    /// of `park_cv`.
}
```

### 4.2 Where mutators check: **explicit `poll_safepoint` + implicit check at every TLAB refill**

Three options, in order of cost-per-check vs. worst-case latency:

| Check location | Cost per alloc | Park latency worst case |
|---|---|---|
| Only at TLAB refill | Zero in fast path | One full TLAB of allocs (~64 KB) |
| Every allocation | One Acquire load + branch | One alloc |
| Explicit `poll_safepoint` calls | Zero if not polled | Whatever the client decides |

**Recommendation:** combine *implicit-at-TLAB-refill* with *explicit
`poll_safepoint` calls the client emits*.

- The TLAB refill already takes `alloc_mutex` — checking the safepoint
  flag there is free.
- Explicit `poll_safepoint` lets the client (NCL interpreter, future
  JIT) decide where the cheap polls go: function entry, loop back-edge,
  immediately after a syscall, etc. NCL's interpreter loop can place
  one per dispatch iteration; a JIT can place one per back-edge as in
  HotSpot / SBCL.
- **Bench-only fallback:** an internal feature flag
  `safepoint_per_alloc` adds the check to every fast-path bump.
  Useful for tests that need deterministic parking latency without
  the client cooperating, but not the default.

**Justification:** The cost of an Acquire load on the fast path of cons
allocation is small but not free; in microbenchmarks it routinely costs
~1-2 ns, doubling the cost of a cons alloc. Putting the check at TLAB
refill costs nothing measurable. Explicit polls let the client emit
checks at frequencies appropriate to its workload. The worst case is
a mutator running a no-alloc no-poll loop, which can stall GC forever;
this is the same limitation as HotSpot's "no-safepoint" intrinsics.
Documented as a constraint, not a bug.

### 4.3 Parking mechanism: **Mutex + Condvar**

```rust
impl<L> Mutator<L> {
    #[inline]
    pub fn poll_safepoint(&mut self) {
        let sp = &self.heap.safepoint;
        let global_epoch = sp.epoch.load(Ordering::Acquire);
        let my_epoch = self.inner().last_epoch.load(Ordering::Relaxed);
        if global_epoch == my_epoch {
            return;  // Fast path: no safepoint pending.
        }
        self.park(global_epoch);
    }

    #[cold]
    fn park(&mut self, target_epoch: u64) {
        // 1. Retire all TLABs so PageDesc::words_used is accurate.
        self.retire_all_tlabs();
        // 2. Snapshot roots (see §5).
        self.refresh_roots_snapshot();
        // 3. Announce arrival.
        self.inner().last_epoch.store(target_epoch, Ordering::Release);
        self.heap.safepoint.parked_count.fetch_add(1, Ordering::AcqRel);
        // Wake the coordinator if it was waiting for the last parker.
        self.heap.safepoint.park_cv.notify_all();
        // 4. Block until world_running == 1.
        let mut guard = self.heap.safepoint.park_mutex.lock().unwrap();
        while self.heap.safepoint.world_running.load(Ordering::Acquire) == 0 {
            guard = self.heap.safepoint.park_cv.wait(guard).unwrap();
        }
    }
}
```

### 4.4 Coordinator side

```rust
impl<L: HeapLayout> GcCoordinator<L> {
    pub fn with_world_stopped<R, F: FnOnce(&mut PageHeap<L>) -> R>(
        &self, f: F,
    ) -> R {
        let sp = &self.safepoint;
        // Increment epoch — mutators on next poll will park.
        let new_epoch = sp.epoch.fetch_add(1, Ordering::AcqRel) + 1;
        // Mark world as stopped (mutators that arrive will block).
        sp.world_running.store(0, Ordering::Release);
        // Wait for every registered mutator to arrive at the new epoch.
        let expected = self.heap.mutator_count();
        let mut guard = sp.park_mutex.lock().unwrap();
        while sp.parked_count.load(Ordering::Acquire) < expected {
            guard = sp
                .park_cv
                .wait_timeout(guard, Duration::from_secs(10))
                .unwrap().0;
            // Diagnostic on timeout: log which mutator hasn't arrived
            // and how long it's been since its last epoch update.
            // Then keep waiting (do not abort) — the user's choice.
        }
        drop(guard);
        // World is stopped. Acquire write access to PageHeap.
        // PageHeap is wrapped in an internal RwLock or we use the
        // fact that all mutator handles are now blocked (they can't
        // touch &PageHeap during park because they only hold &Arc<>).
        let heap_mut: &mut PageHeap<L> = ...;
        let result = f(heap_mut);
        // Reset epoch-leaving counter; mutators that haven't yet
        // observed `world_running == 1` will increment this when
        // they leave the wait loop. See "unpark sequence" below.
        sp.parked_count.store(0, Ordering::Release);
        // Resume world. The Release on world_running pairs with the
        // Acquire load inside the mutator's wait loop, so any heap
        // mutation done while world-stopped is visible to mutators
        // before their next fast-path alloc.
        sp.world_running.store(1, Ordering::Release);
        sp.park_cv.notify_all();
        result
    }
}
```

**Happens-before audit — TLAB retirement → coordinator reads `descs`:**

1. Mutator side (`park()` in §4.3): calls `retire_all_tlabs()` (plain writes
   to `PageDesc::words_used`), then `parked_count.fetch_add(1, AcqRel)`. The
   `AcqRel` Release-half publishes the prior writes.
2. Coordinator side (this function): the `while sp.parked_count.load(Acquire)`
   load is the matching Acquire. When the loop exits, the most recent
   Acquire load saw `count >= expected`, establishing happens-before with
   every contributing mutator's `fetch_add`.
3. Subsequent reads of `descs` in the closure `f` are sequenced-after the
   Acquire load. They see every parked mutator's retired-TLAB writes.

The `park_mutex` also provides happens-before independently — mutators
release it via `park_cv.wait()`'s internal release-then-block, the
coordinator acquires it at line 552 — but the `parked_count` AcqRel/Acquire
pairing is the *primary* synchronization. Belt-and-braces: if we move
TLAB retirement *inside* `alloc_mutex` (one extra lock acquisition per
park, ~50 ns), the mutex lock/unlock cycle gives a second happens-before
chain, defending against any future change to the safepoint flow that
weakens the `parked_count` path. **Recommendation: retire under
`alloc_mutex`** — the cost is negligible compared to a GC cycle.

**Exclusive coordinator entry:** `with_world_stopped` is sequenced — only
one coordinator call at a time can be inside the function. Implementation:
hold a `coord_mutex: Mutex<()>` for the duration of the function (locked
at entry, released after `notify_all`). This makes the "reset
`parked_count` then set `world_running`" window safe — no second cycle
can begin between those two stores because the second cycle's
`coord_mutex.lock()` blocks until the first finishes.

**Unpark sequence diagram:**

```
   Mutator M1                     Coordinator C
   ----------                     -------------
                                  coord_mutex.lock()
                                  epoch.fetch_add(1, AcqRel)  // = N
                                  world_running.store(0, Release)
                                  park_mutex.lock()
                                  loop: parked_count.load(Acquire) < N
                                       park_cv.wait_timeout(...)
   poll_safepoint
   epoch.load(Acquire) = N
   last_epoch.load(Relaxed) = N-1
   park(N):
       retire_all_tlabs()          [plain writes to descs]
       last_epoch.store(N, Release)
       parked_count.fetch_add(1, AcqRel)
       park_cv.notify_all()
       park_mutex.lock()
       loop: world_running.load(Acquire) == 0
            park_cv.wait(guard)    [releases park_mutex, sleeps]
                                       wakes from notify_all
                                       parked_count.load(Acquire) == N  --> exit
                                       drop(guard)
                                       /* sees M1's retire_all_tlabs writes */
                                       f(heap_mut)
                                       parked_count.store(0, Release)
                                       world_running.store(1, Release)
                                       park_cv.notify_all()
                                       coord_mutex.unlock()
       wakes from notify_all
       world_running.load(Acquire) == 1  --> exit
   continues
```

**Why this is race-free:**
- Mutators only check `world_running == 1` (not `parked_count == 0`).
  The reset of `parked_count` to 0 is preparation for the *next* cycle,
  and is invisible to mutators leaving the current one.
- `coord_mutex` ensures no second cycle starts until M1 has had a chance
  to observe `world_running == 1` (or the coordinator's `notify_all`
  has reached every blocked mutator). The cv's `notify_all` is best-effort
  but the wait loop's `Acquire` load on `world_running` re-runs on every
  spurious wake, so loss of a wake-up is harmless.
- A second cycle's `epoch.fetch_add` bumps the value mutators compare
  against. Even if M1 is "slow" (still in `world_running.load` loop) when
  the second cycle starts, M1 will see `world_running == 0` and re-wait.
  M1 then sees the next `notify_all`. No infinite loop, no stale state.

The "Acquire write access to PageHeap" step needs care. Two reasonable
implementations:

**(I)** Wrap `PageHeap`'s mutating internals in a `RwLock<PageHeapInner>`.
Mutator allocation paths take read locks (which permit concurrent
readers, but mutators don't *write* through `&PageHeap` — they bump
inside their TLAB and use atomics for shared bookkeeping). The
coordinator takes a write lock during `with_world_stopped`.

**(II)** Use `UnsafeCell<PageHeapInner>` and rely on the safepoint
protocol as the synchronization mechanism: when `parked_count ==
mutator_count`, no mutator can be executing PageHeap code, so the
coordinator can take an exclusive `&mut` via `unsafe { &mut *cell.get() }`.
This is faster but trickier; the safety argument rests on the safepoint
invariants.

**Recommendation:** start with (I) — a `RwLock` — for safety. The
mutator fast path doesn't touch the lock at all (it bumps in TLAB);
the slow path takes a read lock during refill. Profile later; switch
to (II) if profiling shows the read lock matters.

### 4.5 Deadlock and timeout handling

- **Timeout:** the coordinator waits with a 10s timeout per
  condition-variable wait, then logs which mutator(s) haven't arrived.
  It does NOT force-resume; that would race with the unarrived mutator.
  This is intentional: deadlocks are bugs to fix, not paper over.
- **Test hook:** a `force_timeout_panic_after(Duration)` builder option
  on `GcCoordinator` lets tests assert that a stuck mutator causes the
  coordinator to bail loudly rather than hang.
- **Re-entrant GC:** a coordinator call from inside a mutator's
  `with_world_stopped` callback (e.g. test code that calls GC explicitly
  while holding a mutator handle) would deadlock — that mutator would
  never park. Solved by: the explicit `gc.collect_*` API requires
  `&self` on coordinator (not on mutator), and the convenience
  `mutator.request_gc()` *internally drops the mutator's claim* (parks
  the calling mutator) before signalling the coordinator. See §9
  D-GC-FROM-MUTATOR for the open call.

---

## 5. Root enumeration

### 5.1 Shape

Two viable shapes:

**(A) Closure-per-mutator at registration:**
```rust
let mutator = heap.register_mutator(RootsSource::Walker(Box::new(
    |out: &mut Vec<Word>| my_thread_stack_walker(out),
)));
```
The coordinator invokes the closure for each parked mutator while the
world is stopped. The closure walks the thread's stack/locals and
pushes `Word` values.

**(B) Explicit snapshot before park:**
```rust
mutator.publish_roots(&current_roots);  // copies into roots_snapshot
mutator.poll_safepoint();
```
The mutator publishes its current root vector before every safepoint
poll; the coordinator reads from `roots_snapshot`.

**Recommendation: support both via an enum.**

```rust
pub enum RootsSource<L: HeapLayout> {
    /// Coordinator invokes this closure on the parked mutator's thread
    /// (via a small trampoline — see below) while the world is
    /// stopped. The closure must be Send.
    Walker(Box<dyn FnMut(&mut Vec<Word>) + Send>),
    /// Mutator publishes a snapshot via `publish_roots` before each
    /// poll; coordinator reads the latest snapshot.
    Snapshot,
}
```

- `Walker` matches the existing `evacuate_with_roots(F)` closure shape
  and is convenient for clients that already have a stack walker.
- `Snapshot` matches statepoint-style precise-roots clients (NCL's
  future JIT) that compute the precise root set at every safepoint
  and stash it.

### 5.2 Lifetime concerns

Roots are typed `Word` (8-byte values, see `crate::traits::Word`). The
evacuator updates roots *in place* — it must write back forwarded
pointers. So the roots storage must be `&mut [Word]`, owned by the
mutator, with the coordinator borrowing it while the world is stopped.

For `Snapshot` mode:
- `roots_snapshot: Mutex<Vec<Word>>` on the mutator. Coordinator locks
  it under the world-stopped barrier, walks `&mut [Word]`, drops the
  lock. The lock is uncontended because the mutator is parked.
- The mutator must, after `poll_safepoint` returns, copy the updated
  values back to its actual root locations (registers, stack slots).
  This is the same "stack-relative" pattern statepoints already use.

For `Walker` mode:
- The closure runs *on the coordinator's thread*, but is `Send`. It
  walks the parked mutator's stack via whatever mechanism the client
  has — typically by reading saved stack bounds the mutator published
  at park time (via a thread-local or via a `parked_stack_range` field
  on `MutatorInner`).
- The walker pushes `Word` values into a `Vec<Word>` provided by the
  coordinator, then the coordinator passes `&mut [Word]` to the
  evacuator and after evac is done, *writes back* through stack pointers
  the walker remembered.

The `Walker` write-back is tricky because the walker effectively needs
to be invertible — push location AND value, get value-after-evac back
from the same location. **Recommendation: in the v1 design, `Walker`
mode is restricted to mutators whose roots are held in their published
`Vec<Word>` (the walker fills it; the evacuator updates it; the mutator
copies back at unpark)**. This is the same shape as `Snapshot` but
populated lazily. If a client needs in-place updates of stack slots, it
uses `Snapshot` with the caller managing the back-copy.

### 5.3 Conservative pins from mutator stacks

The existing `pin_pointers_in_ranges` API consumes `&[(usize, usize)]`
of stack address ranges. With multiple mutators, the coordinator
collects each mutator's stack range (published at park time) into one
combined slice and passes it through unchanged. No conceptual change
to the pin pass.

---

## 6. Card barrier interaction

`PageHeap::mark_card_at(&self, ...)` already takes `&self` and uses
`AtomicU8::store(Relaxed)` on the card table. **Confirmed
concurrent-safe.** No changes needed for the multi-mutator design.

New paths the mutator should call but doesn't yet:
- After every store of a heap-pointer Word into an object in
  G1/Tenured (i.e., into a card that backs a non-G0 page), the mutator
  must call `self.mark_card_at(slot_addr)`. This is the same
  responsibility the single-mutator client has today; we just
  re-expose `mark_card_at` on `Mutator<L>` as a `#[inline]` forwarder.

One potential edge case: the card table's address range is fixed (it
covers the whole reservation), so concurrent `mark_card_at` from N
threads stores into N independent bytes most of the time. Only when
two mutators store into objects on the same 512-byte card do their
writes collide — and the write is idempotent (set a byte to 1), so
collision is harmless.

---

## 7. Memory ordering and atomics audit

### `PageHeap` fields after the migration

| Field | Current | Multi-mutator | Notes |
|---|---|---|---|
| `_phantom`, `storage`, `n_pages` | plain | plain | immutable after construction |
| `committed_bits: Vec<AtomicU64>` | atomic | atomic | already concurrent-safe; loads `Acquire`, stores `AcqRel` (unchanged) |
| `committed_count: AtomicUsize` | atomic | atomic | unchanged |
| `commit_lock: Mutex<()>` | mutex | mutex | redundant per comment; keep as-is |
| `descs: Vec<PageDesc>` | plain `&mut self` | **plain, behind RwLock** | Read by mutator slow-path under read lock; written by collector under write lock. **Critical:** `PageDesc::words_used` accessed only at TLAB refill/retire under `alloc_mutex` and at GC under world-stopped, so plain is fine. |
| `alloc_regions` | plain | **plain, behind `alloc_mutex`** | Only touched during refill/retire (mutator) or during GC (coordinator). |
| `start_bits: Arc<[AtomicU64]>` | atomic | atomic | unchanged; mutators set via `fetch_or(Relaxed)` |
| `mark_bits: Box<[u64]>` | plain | plain | STW-only; no mutator access |
| `pinned_cells: HashSet<usize>` | plain | plain | STW-only |
| `recycle_live_counts: Vec<u16>` | plain | plain | STW-only |
| `recycle_live_counts_target: Option<Generation>` | plain | plain | STW-only |
| `last_mark_*`, `last_zero_live_*`, `last_pin_summary` | plain | plain | STW-only |
| `minors_since_g0_promote`, `g0_promotes_since_g1_promote` | plain | plain | STW-only |
| `cards: Arc<CardTable>` | atomic interior | atomic interior | already concurrent-safe |
| `young_page_cap` | plain | plain | written once at construction; read under `alloc_mutex` |
| `bytes_alloc_since_gc` | plain | **AtomicUsize** | Bumped on every alloc fast-path. `fetch_add(Relaxed)` on bump, `load(Relaxed)` in `should_collect`. Slight inaccuracy across cycles is fine — `should_collect` is a heuristic. |
| `auto_gc_trigger_bytes` | plain | plain | written only at end of cycle (STW) |
| `gc_budget_min_bytes`, `tenured_full_threshold_bps` | plain | plain | configured at startup; read only inside STW |
| `poisoned: bool` | plain | **AtomicBool** | `is_poisoned` becomes a `load(Acquire)`; set to true with `store(Release)` from the STW collector exit. |

### New fields

| Field | Type | Ordering |
|---|---|---|
| `mutators: RwLock<Vec<Option<Arc<MutatorInner>>>>` | hot path: never; coordinator: read lock for enumeration | |
| `mutator_count: AtomicUsize` | `Relaxed` reads from coordinator (cross-checked against RwLock contents) | |
| `safepoint: Arc<Safepoint>` | see §4 for internal orderings | |
| `alloc_mutex: Mutex<()>` | std mutex (acquire/release implicit via lock) | also wraps TLAB retirement at park (belt-and-braces happens-before; see §4.4) |
| `coord_mutex: Mutex<()>` | std mutex; serializes coordinator entries | held by `with_world_stopped` from entry to `park_cv.notify_all` — prevents a second cycle from starting while a first is in its reset window |

### `PageDesc`

`PageDesc` stays plain `#[repr(C)]` for now. Mutators never write to a
`PageDesc` outside of TLAB refill/retire (which holds `alloc_mutex`).
Collector writes only under world-stopped. No atomicity needed.

If we later add concurrent GC, `PageDesc::generation` and
`PageDesc::pin_byte` are the candidates for atomic conversion — same as
the GC_DESIGN.md sub-phase 9 comment already says. **Out of scope for
this design.**

### `AllocRegion`

Lives inside `PageHeap::alloc_regions`. Touched only under `alloc_mutex`
(or under world-stopped). Stays plain. The semantics shift from "the
mutator's bump cursor" to "the source of TLAB refills" — the TLAB
itself is the mutator's bump cursor now.

### Ordering summary

- Bump-pointer fast path: zero atomics on the bump itself. One
  `fetch_or(Relaxed)` per allocation to set the start bit. One
  `fetch_add(Relaxed)` per allocation on `bytes_alloc_since_gc`. One
  `load(Acquire)` per allocation on `poisoned`. (Or zero if we trust
  TLAB pre-charging and only check `poisoned` at refill — see §9
  D-POISON-CHECK.)
- TLAB refill: take `alloc_mutex` (std mutex). Acquire/Release implicit.
- Safepoint poll fast path: one `load(Acquire)` on `safepoint.epoch`,
  one `load(Relaxed)` on `my_last_epoch`, one compare. Zero on the
  hot path.
- Safepoint park: `Mutex<()>` + `Condvar`. Standard happens-before via
  mutex lock/unlock.
- Card barrier: `AtomicU8::store(Relaxed)` (existing).

---

## 8. Phasing

Five chunks. Each is independently mergeable, has its own tests, and
leaves the heap usable for the previous shape's clients.

### Phase 1 — `Mutator<L>` handle with serialised allocation (~700 lines)

**What it adds:**
- `Mutator<L>` struct and `register_mutator(&Arc<Self>)` constructor.
- Methods `try_alloc_cons_in`, `try_alloc_boxed_in`, `try_alloc_large`,
  `mark_card_at` — all of which delegate to the existing `&mut self`
  methods on `PageHeap` via an internal `Mutex<PageHeap>` wrapper.
- Wraps `PageHeap` in `Arc<Mutex<...>>` internally so multiple
  `Mutator<L>`s can co-exist; allocation is still serialised by the
  mutex.

**What it leaves unchanged:**
- The existing `&mut self` API on `PageHeap`. Direct `heap.try_alloc_cons_in`
  still works; the old single-mutator client compiles unchanged.
- The GC entry points (`collect_minor`, `try_collect_*`) are unchanged.
- All seven existing `tests/threading.rs` tests pass.

**Tests:**
- `mutator_handle_alloc_round_trips`: create 1 mutator, allocate, GC
  with no roots, verify reclamation.
- `two_mutators_share_one_heap_via_handle`: 2 threads register, both
  allocate `N` cells, total cell count is correct, GC reclaims them.
- `mutator_alloc_returns_none_when_poisoned`: induce poison via
  small-heap mid-evac OOM, verify subsequent `mutator.try_alloc_*`
  returns `None`.
- `mutator_drop_releases_slot`: drop a mutator, verify
  `heap.mutator_count()` decreases.

**Size estimate:** ~600 lines of mutator.rs + ~50 lines of changes to
space.rs + ~150 lines of tests. Fits one focused agent run.

### Phase 2 — Real per-mutator TLABs (~800 lines)

**What it adds:**
- `Tlab` struct, `tlabs: [[Tlab; 2]; 3]` field on `Mutator<L>`.
- Refill protocol per §3.3, using existing `try_alloc_g0_cons_slab` and
  a new mirror `try_alloc_boxed_slab`.
- TLAB retirement at mutator drop and at safepoint (stub, since
  safepoints land in Phase 3).
- `alloc_mutex: Mutex<()>` on `PageHeap` (or `parking_lot` if we want
  to avoid std's poisoning).
- Fast-path bump in `Mutator::try_alloc_cons_in` and
  `try_alloc_boxed_in` — no heap lock if the TLAB has room.

**What it leaves unchanged:**
- `PageHeap`'s public `&mut self` API.
- The GC entry shape (still `&mut self`).
- Large-object alloc still serializes through the central path.

**Tests:**
- `tlab_bump_no_heap_lock`: instrument `alloc_mutex` with a counter;
  allocate 4096 cons cells in one thread; assert the lock was taken
  fewer than 16 times (TLAB amortisation).
- `concurrent_cons_alloc_no_torn_pointers`: 4 threads each allocate
  10k cons cells into G0 with payload `(i, hash(i))`; after, walk every
  cons start in G0 and verify `cdr == hash(car)`. No torn cells.
- `tlab_refill_respects_young_page_cap`: 2 mutators, young_page_cap=4,
  attempt to allocate enough conses to want 8 pages; verify total G0
  page count <= 4 and the second mutator's alloc returned None.
- `tlab_drop_reconciles_words_used`: allocate 1000 cells via TLAB,
  drop the mutator, verify `page.words_used` matches actual cells used.
- `start_bits_set_correctly_under_concurrent_alloc`: assert every
  allocated cell has its start bit set (and only those).

### Phase 3 — Safepoint + cooperative parking (~700 lines)

**What it adds:**
- `Safepoint` struct on `PageHeap`.
- `Mutator::poll_safepoint`, `Mutator::park` (cold path).
- `GcCoordinator::with_world_stopped`.
- TLAB retirement at park (correct version, replacing Phase 2 stub).
- `GcCoordinator::collect_minor`, `try_collect_minor`, etc., as thin
  wrappers around `with_world_stopped` + existing
  `PageHeap::collect_minor`.

**What it leaves unchanged:**
- Root enumeration is still the existing closure shape (one closure
  passed to the coordinator's `collect_*`). Per-mutator root walking
  lands in Phase 4.
- Mutator fast-path bump is unchanged.

**Tests:**
- `safepoint_parks_all_mutators`: 4 mutators, each running an alloc
  loop with `poll_safepoint` per iteration; coordinator requests
  world-stopped, verifies `parked_count == 4` before the closure runs,
  verifies mutators resume after.
- `safepoint_during_alloc_loop_no_torn`: 4 mutators alloc + 1
  coordinator-thread calling `collect_minor` 10× over 5 seconds; assert
  no torn cells, no double-frees, all roots survive.
- `safepoint_with_explicit_polls`: a mutator with no allocs in its hot
  loop must still park via `poll_safepoint`.
- `safepoint_pending_mutator_blocks_gc`: a mutator that never calls
  `poll_safepoint` and never allocates makes `with_world_stopped` block
  past the timeout; assert the diagnostic log message identifies the
  stuck mutator. (Use a short timeout via the builder.)
- `safepoint_preserves_poison`: poison the heap from inside a parked
  collection; assert subsequent `mutator.try_alloc_*` from any mutator
  returns None.

### Phase 4 — Per-mutator root enumeration (~500 lines)

**What it adds:**
- `RootsSource<L>` enum.
- `Mutator::publish_roots`.
- `roots_snapshot: Mutex<Vec<Word>>` on `MutatorInner`.
- Coordinator collects every mutator's published roots, passes the
  combined slice through to `PageEvacuator::visit`.
- Conservative-pin path: combine all mutators' `parked_stack_range`s
  into the slice passed to `pin_pointers_in_ranges`.

**What it leaves unchanged:**
- The `collect_minor(F)` closure-based API still works for clients with
  one global root set. `RootsSource::Snapshot` is opt-in.

**Tests:**
- `two_mutators_roots_enumerate_separately`: each mutator owns one cons
  pointer; both call `publish_roots`; GC runs; both pointers are
  forwarded; no cross-mutator interference.
- `mutator_unregistered_after_gc_doesnt_lose_roots`: mutator A
  publishes roots, mutator B is dropped mid-cycle (illegal — test
  asserts this panics); also: mutator B drops cleanly between cycles.
- `walker_roots_round_trip`: `RootsSource::Walker` mode; verify
  closure is called once per parked mutator and updates flow back.
- `precise_roots_only_mode`: with `--no-default-features
  --features=precise-roots-only` (open decision; see §9), assert no
  conservative pin scan runs.

### Phase 5 — Hardening and cleanup (~400 lines)

**What it adds:**
- `Mutator::request_gc()` convenience that parks the calling mutator
  then signals the coordinator.
- Builder for `GcCoordinator` (timeout, `safepoint_per_alloc` debug
  flag, telemetry hooks).
- Stress tests: 100k iterations of alloc + GC under 8 mutators.
- Doc updates: update `THREADING.md` to reflect the new state of the
  world.

**Tests:**
- `stress_8_mutators_100k_iterations`: long-running torture test.
- `loom_safepoint_protocol` (if we adopt `loom`): tiny model of the
  safepoint state machine, asserts no deadlock + correct happens-before
  on `poisoned` and `epoch`.

---

## 9. Open decisions

Each item: options, tentative recommendation, **user must decide**.

### D-TLAB-SIZE — TLAB size policy

- **Options:**
  - (a) Fixed 4 KB (cheap to retire, lots of pages used).
  - (b) Fixed 64 KB (one page per TLAB, max amortisation).
  - (c) Dynamic 4 KB → 8 KB → 16 KB → ... → 64 KB.
- **Tentative:** (c). Best of both.
- **Decision needed:** confirm (c), and set the growth schedule (double
  every refill? double every other refill? on a timer?). Recommendation
  is "double every refill" for simplicity.

### D-SAFEPOINT-FREQ — Safepoint check frequency

- **Options:**
  - (a) Only at TLAB refill (~64 KB worst case latency).
  - (b) Every fast-path allocation (~10s of ns extra per alloc).
  - (c) TLAB refill + client-emitted `poll_safepoint` calls.
- **Tentative:** (c).
- **Decision needed:** is the "client emits polls" contract OK for
  NCL's interpreter loop? If the future JIT can emit polls at back-
  edges, (c) is right. If not, we may want (b) under a feature flag.

### D-GC-API-SHAPE — Does the collector take `&mut self` or `&self`?

- **Options:**
  - (a) `&mut self` via `with_world_stopped` (this doc's recommendation).
  - (b) `&self` everywhere, with internal atomics covering every field
    mutator and collector both touch.
- **Tentative:** (a).
- **Decision needed:** confirm (a). The alternative is a much bigger
  refactor (every `PageHeap` mutating method touched). (a) is also
  closer to the existing shape.

### D-ROOTS-SHAPE — Root enumeration default

- **Options:**
  - (a) `Walker` (closure) is the default; `Snapshot` is opt-in.
  - (b) `Snapshot` is the default; `Walker` is opt-in.
  - (c) Always require the client to pick one at registration.
- **Tentative:** (c) — make the client decide explicitly. The default
  surface is one extra enum variant, not a behavioural mystery.
- **Decision needed:** confirm (c).

### D-CONSERVATIVE-PIN — Keep conservative pins in multi-mutator?

- **Options:**
  - (a) Keep the existing `pin_pointers_in_ranges` and run it on every
    parked mutator's stack range. Same robustness for ad-hoc clients
    that don't emit statepoints.
  - (b) Drop conservative pins entirely; require all clients to emit
    precise root sets via `Snapshot`.
- **Tentative:** (a). Conservative pin is cheap and already gated by
  the `conservative-pin` feature.
- **Decision needed:** confirm (a). Or pick (b) if you're committed to
  always-precise roots from NCL.

### D-MUTEX-IMPL — `std::sync::Mutex` or `parking_lot`?

- **Options:**
  - (a) `std::sync::Mutex` — no extra deps.
  - (b) `parking_lot::Mutex` — faster, no poisoning, smaller.
- **Tentative:** (a) for now (fewer deps); profile later.
- **Decision needed:** confirm. (a) means accepting Rust's lock poisoning
  semantics on `alloc_mutex` (which we'd never want to recover from
  anyway, so the unwrap pattern is fine).

### D-GC-FROM-MUTATOR — Can a mutator request GC explicitly?

- **Options:**
  - (a) Yes: `mutator.request_gc()` parks the mutator, signals the
    coordinator, and the coordinator runs GC with all other mutators
    also parked.
  - (b) No: GC is always driven from outside the mutator API. A mutator
    that wants GC sets a flag the coordinator polls.
- **Tentative:** (a). It matches what NCL's `MutatorState::trigger_minor_gc`
  does today.
- **Decision needed:** confirm (a). If (a), then the coordinator must be
  designed for "any mutator can be the one to signal, but the signal
  is idempotent during a cycle."

### D-POISON-CHECK — Where do we check `poisoned`?

- **Options:**
  - (a) Once at TLAB refill. Mutator can allocate from a stale TLAB for
    one TLAB's worth after poison — but TLABs are small.
  - (b) On every fast-path alloc (`Acquire` load).
- **Tentative:** (a). Mid-evac OOM is rare; one TLAB of "phantom"
  allocations after poison is acceptable.
- **Decision needed:** confirm (a) is acceptable. If we want strict
  immediate-stop semantics, go (b).

### D-WIN-VS-LINUX-TLS — Per-thread storage on Windows vs Linux

- **Options:**
  - (a) No thread-local; the client passes the `Mutator<L>` explicitly
    (this doc's design).
  - (b) `#[thread_local]` cached pointer to the mutator for syntactic
    convenience, with the explicit `Mutator<L>` as a fallback.
  - (c) `std::thread::LocalKey` for the same purpose, cross-platform.
- **Tentative:** (a). No thread-local in newgc-core; the binding (NCL)
  can add `LocalKey` on its side if desired.
- **Decision needed:** confirm (a). If NCL needs (c) for ergonomic
  reasons, we can layer it on top without changing the core API.

### D-RETIRE-ON-IDLE — Do we retire TLABs on idle to reclaim memory?

- **Options:**
  - (a) No. TLABs are kept until next safepoint / mutator drop. Unused
    tail is wasted memory but not "leaked" — GC will reclaim the page
    when the *whole* page is empty.
  - (b) Yes. A periodic timer or "no-alloc-for-N-ms" heuristic retires
    idle TLABs back to the central region.
- **Tentative:** (a). Simpler. Worst case: idle mutators consume
  N_mutators × 6 × 64 KB = (for 8 mutators) 3 MB of "stale" TLAB.
- **Decision needed:** confirm (a).

### D-RWLOCK-VS-UNSAFECELL — Coordinator-side locking

See §4.4 (I) vs (II). **Tentative: (I) RwLock**. **Decision needed:**
confirm (I).

---

## 10. Test plan

### Existing tests (`tests/threading.rs`)

All seven existing tests must keep passing:

| Test | Action |
|---|---|
| `pageheap_is_send` | Still passes — Send is preserved. |
| `pageheap_is_sync` | Still passes — Sync is preserved. |
| `n_independent_heaps_allocate_in_parallel` | Still passes — uses raw `&mut PageHeap`. |
| `independent_heaps_are_genuinely_independent` | Still passes. |
| `shared_heap_via_mutex_serializes_allocation` | Still passes — `Mutex<PageHeap>` wrapping still compiles, just becomes suboptimal compared to `Mutator<L>`. |
| `shared_heap_can_gc_after_concurrent_alloc` | Still passes. |
| `read_only_accessors_work_concurrently` | Still passes. |

No retirements. Phase 5 adds a doc-only note pointing at the new tests.

### New tests by phase

(Listed in §8.)

### Cross-cutting invariants

| Invariant | Asserted by |
|---|---|
| Concurrent allocation produces no torn pointers | `concurrent_cons_alloc_no_torn_pointers` (Phase 2) |
| Concurrent allocation + safepoint produces no torn pointers | `safepoint_during_alloc_loop_no_torn` (Phase 3) |
| No mutator escapes the safepoint barrier | `safepoint_parks_all_mutators` (Phase 3) |
| GC sees a consistent heap (every TLAB retired before GC reads `PageDesc::words_used`) | `tlab_drop_reconciles_words_used` (Phase 2) + parking integration test (Phase 3) |
| Poison visible to all mutators | `safepoint_preserves_poison` (Phase 3) |
| `young_page_cap` respected under concurrent refill | `tlab_refill_respects_young_page_cap` (Phase 2) |
| `recycle_live_counts_active_for(G0)` bypass works inside `with_world_stopped` | `recycle_bypass_during_world_stop` (Phase 3) |

---

## 11. Risks and unknowns

### What could go wrong

1. **TLAB retirement at park is not atomic-with-park.** A mutator that
   has bumped its TLAB but not yet retired could be racing with a
   concurrent reader of `PageDesc::words_used`. **Mitigation:** TLAB
   retirement happens *before* the mutator announces it has parked
   (`parked_count.fetch_add`), and the coordinator does not read
   `descs` until `parked_count == mutator_count`. Strict happens-
   before via `AcqRel` on `parked_count`. **Risk if I'm wrong:**
   incorrect `tenured_used_bytes`, false GC triggers.

2. **`alloc_mutex` becomes contended.** With 16+ threads all
   simultaneously triggering TLAB refills, the single `alloc_mutex`
   serialises them. Workaround: bigger TLABs (cap raised to 64 KB).
   If still contended: shard the allocator (multiple
   `alloc_regions` arrays guarded by separate mutexes). **Risk:** if
   profiling shows >5% time in `alloc_mutex`, redesign with sharding.

3. **A misbehaving client never polls safepoint.** GC hangs. The
   10-second timeout *diagnoses* but does not *fix*. Documented as a
   known limitation; out-of-band fix is to make NCL emit polls at
   every back-edge. **Risk:** angry user filing "GC hangs forever"
   bugs. Mitigation: very clear docs + timeout-with-loud-log.

4. **`young_page_cap` semantics under concurrent refill.** Two mutators
   simultaneously try to grow G0 from N to N+1 pages. Both hit the
   check, both see "OK to grow", both open. Cap is exceeded by one.
   **Mitigation:** the cap check is *inside* `alloc_mutex`, so the
   refills serialize. **Risk if wrong:** transient cap overrun by
   N_mutators pages.

5. **`Mutator<L>` lifetime vs. heap shutdown.** If a mutator outlives
   `Drop` of the last `Arc<PageHeap>`... wait, it can't, because the
   mutator holds an `Arc<PageHeap>`. Fine. But if a mutator is held
   inside a thread that the test infra never joins, the heap won't
   drop. Test discipline: every spawned test thread must join before
   the heap is dropped. **Risk:** flaky tests on CI if a thread panics
   between alloc and join.

6. **Drop while parked.** If a mutator is dropped while
   `with_world_stopped` is running on another thread, the drop tries
   to retire TLABs (write to `PageDesc`) — but the coordinator owns the
   heap exclusively. **Mitigation:** `Drop` impl checks
   `safepoint.world_running == 1` and panics if not (debug builds).
   The cleaner design: refuse to drop the mutator while parked, by
   panic. Or: register a deferred drop that runs on unpark. **Risk:**
   needs explicit decision; I picked panic but the user may prefer
   defer.

7. **Stack-range publication for conservative pins.** A mutator's stack
   range needs to be set before parking (so the coordinator can read it
   while world-stopped). The natural API is "thread's current stack
   bounds, queried via `std::thread::current_stack_bounds`" — which
   doesn't exist in stable Rust. NCL clients typically have their own
   stack-range mechanism (the JIT knows). **Mitigation:** add
   `Mutator::set_stack_range(low, high)` that the client calls at
   safepoint (or once at startup if the stack is fixed). **Risk:**
   if the client forgets to set it, conservative pins are empty and
   precise-roots-only correctness is required.

### Unknowns worth a prototype before committing

1. **Microbench of TLAB fast path.** Confirm the fast-path bump is
   actually < 10 ns and that the `poll_safepoint` check is cheap.
   Should be true; verify before Phase 2 lands.

2. **TLAB retirement under `loom`.** Run a `loom`-based model of:
   mutator bumps → world-stopped reads `words_used`. Confirm the
   `AcqRel` on `parked_count` is sufficient. Should be true; verify.

3. **Lock-free `mutators` vec.** Phase 1 uses `RwLock<Vec<Option<...>>>`
   for the mutator list. Read of "current mutator count" is hot during
   `with_world_stopped`. Whether the RwLock contention matters depends
   on how often we GC. Prototype + measure.

### Things I spotted but don't have a clean answer to

- **Mutator-side `mark_card_at` overhead.** The existing card barrier
  is `Relaxed` byte store — fine for a single mutator. Under N
  mutators, the same card byte may be hit by multiple threads. The
  store is idempotent (write 1 → 1), but the cacheline ping-pong can
  be expensive. NCL's typical workload (heap pointers stored into
  Tenured objects) is rare enough that it probably doesn't matter, but
  if it does, we'd want a CAS-or-load-then-store pattern. **No clean
  answer; flag for future profiling.**

- **`bytes_alloc_since_gc` as a global counter.** Every cons alloc
  does `fetch_add(Relaxed)` on a global. That's a global cacheline
  ping-pong. **Alternative:** per-mutator counters summed at
  `should_collect`. **Open** — not flagged in §9 because the change
  is purely internal and can land later. But worth noting.

- **`start_bits` `fetch_or(Relaxed)` cacheline contention.** Two
  mutators bumping into adjacent cells (e.g. both filling adjacent
  TLABs on the same page) hit the same start-bits word. `Relaxed`
  `fetch_or` still does a cacheline transfer. Probably not a hot
  problem because mutator TLABs are 4 KB+ each (different start-bits
  words), but worth profiling.

- **What `Drop` on a `Mutator` should do if the heap is poisoned.**
  Currently I said "retire and reconcile `words_used`". But the heap
  is in indeterminate state. Maybe `Drop` should bail without
  touching `descs` if `poisoned`. **Open** — not flagged in §9.

---

## Appendix A — Estimated touchpoints

| File | Changes |
|---|---|
| `src/page_heap/space.rs` | Add `safepoint`, `mutators`, `alloc_mutex`, `mutator_count` fields. Convert `poisoned` and `bytes_alloc_since_gc` to atomics. (~150 lines.) |
| `src/page_heap/mutator.rs` (new) | `Mutator<L>`, `Tlab`, `MutatorId`, `MutatorInner`, `RootsSource`. (~600 lines including docs.) |
| `src/page_heap/safepoint.rs` (new) | `Safepoint` struct + park/unpark protocol. (~250 lines.) |
| `src/page_heap/coordinator.rs` (new) | `GcCoordinator` wrappers around `with_world_stopped`. (~300 lines.) |
| `src/page_heap/alloc.rs` | No changes — the `&mut self` API stays. `try_alloc_in_region` etc. continue to be called by the central refill path. |
| `src/page_heap/coordinator_api.rs` | Reuse existing slab/TLAB primitives. Minor signature work. (~50 lines.) |
| `src/page_heap/cycle.rs` | Unchanged. Called from `with_world_stopped`. |
| `src/page_heap/evac.rs` | Unchanged. |
| `src/lib.rs` / `src/page_heap/mod.rs` | Export `Mutator`, `GcCoordinator`, `MutatorId`, `RootsSource`. (~10 lines.) |
| `tests/multi_mutator.rs` (new) | New test file for Phases 2–5 tests. (~600 lines across phases.) |
| `tests/threading.rs` | Unchanged. |

Total estimated diff: 2000–2500 lines across the 5 phases.
````

### `END DESIGN DOC`

---

## Summary

**Recommended phasing:** Phase 1 introduces a `Mutator<L>` handle backed by an internal `Mutex<PageHeap>` (small, ~700 lines, unblocks the new API surface without touching the alloc fast path). Phase 2 adds real per-(gen, kind) TLABs with a dedicated `alloc_mutex` and dynamic 4KB → 64KB sizing — this is where concurrent fast-path bump lands. Phase 3 introduces the safepoint protocol (per-mutator epoch + global condvar) and a `GcCoordinator::with_world_stopped` that keeps `PageHeap`'s collector API as `&mut self`. Phase 4 adds per-mutator root enumeration via a `RootsSource::{Walker, Snapshot}` enum and combined conservative-pin slices. Phase 5 hardens (timeouts, telemetry, stress tests).

**Most consequential open decisions for you:** (1) **D-GC-API-SHAPE** — confirm the collector keeps `&mut self` and uses `with_world_stopped` rather than going fully `&self`-with-atomics; this is the largest API direction call. (2) **D-SAFEPOINT-FREQ** — confirm "safepoint check at TLAB refill + explicit `poll_safepoint` calls the client emits," which trades a fast-path cost (~1-2 ns per alloc avoided) against a parking-latency worst case (one TLAB of allocs ≈ 64 KB). (3) **D-ROOTS-SHAPE** — pick whether `Walker` or `Snapshot` is the default, or force the client to pick at registration; this shapes the binding contract NCL will write against.

**Risks I couldn't resolve in the doc:** (a) Card-barrier and start-bit cacheline contention under heavy concurrent allocation on adjacent cells — design is correct but performance may need profiling-driven mitigation that I don't have a clean answer for ahead of measurement. (b) Drop semantics of a `Mutator` while the heap is poisoned — currently I say "retire as normal" but that touches `descs` on an indeterminate heap; might need to bail without reconciling, but I don't have enough context on whether the slight `words_used` inconsistency matters versus the risk of touching an unsafe state. (c) Stack-range publication for conservative pins requires the client to call `set_stack_range` correctly; if the client forgets, conservative pinning is silently empty — there's no clean way to enforce this in the type system that I could see.
