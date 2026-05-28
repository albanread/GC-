# Multi-Mutator Design for NewGC's PageHeap

**Status:** Draft v6 — three red-team rounds + both frontends'
code-grounded answers (Dylan + NCL), 2026-05-27/28. No code yet.
Supersedes the "Roadmap to true multi-thread support" section of
`THREADING.md`.

**v6 (2026-05-28):** NCL's second, code-grounded pass. Both frontends
confirmed **precise-primary** (corrected the interim "NCL conservative
today" framing). Closed the three gaps where the design *named* an NCL
need but didn't *specify* it: **(1)** poll-site root-consistency
contract (§4.2); **(2)** the explicit FFI `pin`/`unpin` API is now fully
designed (§5.4) — `SharedHeap::explicit_pins`, in-place page-flip reuse,
process-lifetime guarantee; **(3)** fork-reconciliation adoption note
(§8) + a **frontend coverage audit (Appendix B)** confirming every
core-side need of both frontends is accounted for. Recorded that NCL
already has hand-rolled parking (adoption = reconciliation) and that the
auto-major/recoverable-OOM pieces NCL lacks already exist in
`newgc-core` HEAD (fork-sync, not a core gap).


> **Re-baseline (v5, from the NewOpenDylan team).** Earlier drafts kept
> hedging toward a *conservative-scanning, signal-suspension* runtime —
> that is **Open Dylan's Boehm heritage, which this project never
> touched.** NewOpenDylan shares the language, not the runtime.
>
> **NewGC serves TWO frontends with opposite root models — both
> first-class on the page-heap, selected by the `conservative-pin`
> Cargo feature:**
> - **NewOpenDylan** — **precise-roots, cooperative-poll, fully-moving**
>   *today*. Builds the page-heap with `conservative-pin` **off**
>   (`pin_stack_range` is a no-op façade, `heap.rs:1773`); precise roots
>   via Sprint-45c safepoint maps; `nod_safepoint_poll()` at function
>   entry + every loop back-edge (Sprint 45e).
> - **NCL (Common Lisp)** — **also precise-primary today** (corrected
>   from a stale briefing). Its LLVM JIT emits `ncl_push_root`/
>   `ncl_pop_root` around *all* GC-triggering call sites (calls, cons,
>   closure-make, build-rest, global/function load, bignum-promote slow
>   paths). It *additionally* runs a conservative scan — but of **stack
>   ranges only** (`[rsp, stack_hi)` per thread), plus static + G1/Tenured
>   dirty cards. It does **not** walk arbitrary Rust-heap containers.
>   Conservative pin is retained for two reasons: belt-and-suspenders
>   over the precise set, and — load-bearing — it **incidentally pins
>   FFI objects** whose address sits on the native stack across a Win32
>   call (§4.6). NCL vendors its own fork of the page-heap (not a
>   `newgc-core` crate dep), so its feature set can lag/lead HEAD
>   independently.
>
> **Consequence:** both frontends are **precise-primary**. Conservative
> pinning is **NOT** legacy, **NOT** semispace-only, and **NOT** anyone's
> *primary* root source — it is a feature-gated *page-heap* complement
> (`conservative-pin`) that NCL keeps for belt-and-suspenders + FFI
> incidental pinning. (NCL has in fact *deleted* its semispace backend
> entirely — page-heap is its only backend — so "conservative ==
> semispace" was doubly wrong.) The multi-mutator design supports
> **precise `Snapshot` roots** (§5.1) as the primary path and
> **conservative stack-range pins** (§5.3) as the optional complement,
> and must stay correct under either feature configuration. Per-thread
> stack-range publication is **required wherever `conservative-pin` is
> enabled** (NCL), not moot.
>
> **FFI pinning is a real core requirement, not just a question (§4.6,
> Q7).** NCL's Win32 callback path hands Lisp closures to the OS for the
> *process lifetime*; copy-to-native-buffer is not viable there. Once
> conservative scan is dialed back, the incidental FFI pin disappears, so
> the core needs an **explicit pin/unpin API for arbitrary (incl.
> indefinite) lifetimes**, distinct from the conservative *stack* pin.
>
> Single-mutator-only today is, for both frontends, purely because the
> heap `Mutex` and parking protocol haven't been generalized — not a
> root-model or signal constraint. The one genuinely new work item is
> the **native-call-boundary safepoint convention** (§4.6) — a thread
> blocked in `GetMessageW`/Win32 (Dylan UI thread *or* NCL `%ffi-call`)
> must be treated as parked-at-safepoint so it can't hold the collector
> hostage.

**Revision history:**
- v5 (2026-05-28): **Both** frontends answered (Dylan Q1–Q7 + NCL
  briefing). Re-baselined off Boehm (above). **Two opposite root
  models, both first-class:** Dylan precise / NCL conservative-now-
  precise-later — `conservative-pin` is a feature-gated *page-heap*
  path, NOT legacy (this corrects an interim v5 draft that demoted it).
  Stack-range publication is **required** for NCL multi-threaded
  conservative scan (§5.3) — not moot. **New: §4.6 native-call-boundary
  convention** + `state: AtomicU8` on `MutatorInner`; collector skips
  `InNative` threads (serves Dylan UI thread *and* NCL `%ffi-call`).
  **Q6:** core stays handle-explicit; each frontend owns a
  `thread_local!` mutator wrapper with a fast `Cell<*mut Mutator>` fetch
  (§2.6) — NCL `(make-thread)` gives each Lisp thread its own mutator +
  conservative range + push/pop root stack. **Q1/Q2/Q7:** recorded
  constraints — single-digit thread count + shared mutable graph (single
  central lock, sharding YAGNI); IDE pause budget ~16 ms/frame with small
  live sets (STW OK; concurrent-marking is the documented escape hatch);
  long-lived threads + dynamic 4 KB→64 KB
  TLABs, larger for the compile worker. **New round-two question:**
  FFI object pinning across blocking native calls (§4.6).
- v4 (2026-05-28): Third round (reviewers A + B). **B-2 (critical):**
- v4 (2026-05-28): Third round (reviewers A + B). **B-2 (critical):**
  GC trigger moved onto `Mutator::try_collect_*`; the driver
  self-publishes + sets `is_acting_coordinator` so the wait loop skips
  it — fixes the coordinator-waits-for-its-own-park deadlock (§2.5,
  §4.4). **B-1:** removed the global `parked_count`; the coordinator now
  waits per-mutator on `last_epoch`/`is_active` (§4.1, §4.4). **A-1/B-3:**
  `register_mutator` serializes with STW via `coord_mutex` (§2.2).
  **B-4:** corrected reconciliation math (cell==word; `offset_from`, no
  `/8`, no `*2`) (§3.3). **A-4/B-5:** cut `RootsSource::Walker`
  (write-back fiction for moving GC); Snapshot-only (§5, §9). **A-2:**
  safepoint + per-mutator roots must land together (Phase 3); softened
  the "independently mergeable" claim (§8). **A-3:** corrected
  cons-tail wording (over-stated `words_used` is harmless because cons
  cells carry start bits) — codebase comments tracked separately.
  **A-5:** removed stale §9 decisions (D-RWLOCK, D-GC-API-SHAPE,
  D-ROOTS-SHAPE, D-GC-FROM-MUTATOR now RESOLVED); fixed `start_bits`
  type (SharedHeap owns `Box<[AtomicU64]>`; Mutator caches a raw view).
- v3 (2026-05-28): Second red-team round (team feedback). **Point 1
  (load-bearing):** replaced the unsound `UnsafeCell<PageHeap>` /
  fast-path-killing `RwLock<PageHeap>` options with the §2.0
  `SharedHeap` split — lock-free atomics extracted into
  `Arc<SharedHeap>`, `PageHeap` stays monolithic behind a
  `Mutex<PageHeap>`, mutators never hold a bare `&PageHeap`. Preserves
  the existing ~5000-line collector. **Point 2:** GC-epoch dedup in
  `try_collect_*` to coalesce thundering-herd triggers (§2.5).
  **Point 3:** replaced "retire TLAB under `alloc_mutex` at park"
  (v2's mistaken belt-and-braces) with publish-cursors —
  mutators publish `(page_idx, start, bump)` lock-free at park, the
  coordinator reconciles `words_used` single-threaded (§3.3, §4.3,
  §4.4). **Point 4:** STW-aware `Drop` — deregister + `notify_all` +
  coordinator re-reads `mutator_count` each wait iteration so a
  panicking mutator can't hang GC (§2.1, §4.4). **Point 5:** confirmed
  the start-bits cache is safe (fixed up-front reservation, no realloc)
  — non-issue (§2.1).
- v2 (2026-05-27): Added happens-before audit (§4.4), `coord_mutex`,
  unpark sequence diagram, `poisoned` → `AtomicBool` migration language.
  (NOTE: v2's "retire under alloc_mutex" recommendation was reverted in
  v3 by Point 3.)
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
7. **Both root models, fully-moving.** The multi-mutator design supports
   **precise `Snapshot` roots** (§5.1 — Dylan today, NCL after its
   push/pop work) *and* **conservative stack-range pins** (§5.3 — NCL
   today), selected by the `conservative-pin` feature. The collector
   moves every non-pinned object under either model.

### Target-workload constraints (from both frontends)

- **Scale:** single-digit concurrent mutator threads, sharing one
  mutable object graph. Dylan: a Windows GUI/IDE (one UI thread + small
  worker pool). NCL: `(make-thread)` Lisp threads, each with its own
  mutator handle, conservative-scan stack range, and push/pop root
  stack. → one central allocator lock; sharding is YAGNI for both.
- **Pause budget:** Dylan's interactive IDE wants STW under ~16 ms (one
  frame), occasional ~50–100 ms tolerable; batch/AOT compile and NCL's
  REPL/compile are throughput-tolerant. Dylan live sets are small (one
  file's AST/rope/tokens). **Caveat (NCL, unmeasured):** an old NCL
  doc reported a `life.lisp` live set ~95,000× the game state, blamed on
  conservative scanning. Per NCL's code-grounded reconciliation, that
  figure **predates precise roots and should not be trusted** — NCL's
  current scan is `push_root` precise roots + stack-range conservative
  (not arbitrary Rust-heap containers) + cards, and nobody has measured
  retention on HEAD with precise roots active. **Action item for NCL:
  re-measure before anyone designs around it.** It is not a multi-mutator
  concern; flagged only so the sub-frame claim isn't read as proven for
  NCL until re-measured.
- **Allocation pattern:** long-lived threads, moderate/bursty volume.
  NCL is **cons-dominated** (2-word `Tag::Cons`, the hot path by a wide
  margin — macroexpansion, list processing, closures); Dylan churns
  `<token>`/`<stretchy-vector>` graphs. → dynamic 4 KB→64 KB TLABs fit;
  the cons-page fast path matters most for NCL.

### Non-goals (explicitly excluded from this design)

- **Concurrent GC.** The collector remains STW. Mutators do not allocate
  or read pointers while the collector runs. *Escape hatch:* if the IDE
  ever holds a large *project-wide* graph live (so STW exceeds the
  ~16 ms frame budget), the relax-to-concurrent/parallel-marking work
  becomes justified — but the measured live sets today do not justify
  it, so it stays out of scope.
- **Parallel mark/evac.** The collector is single-threaded internally.
- **Lock-free GC.** No CAS-based concurrent BFS, no work-stealing queues.
- **Pre-emptive parking via signals.** Mutators park at *cooperative*
  safepoint polls (already emitted at function entry + loop back-edges —
  Sprint 45e). No signals, no `SuspendThread`, no debug-trap insertion.
  A thread in a tight no-alloc loop still polls at the back-edge; a
  thread blocked in *native* code is handled by the §4.6 convention, not
  by signals.
- **Thread-local mutator in the core.** `Mutator<L>` is an explicit
  handle. Each frontend owns its `thread_local!` "current mutator"
  wrapper (§2.6) — the core stays portable and TLS-free.

(Note: conservative stack scanning is explicitly **in scope** — it is
NCL's current primary root path, feature-gated behind `conservative-pin`
on the page-heap. It is *not* a non-goal. See §5.3.)

---

## 2. API surface

### 2.0 Structural decomposition: `SharedHeap` vs the locked `PageHeap`

**This is the load-bearing decision; everything else hangs off it.**
(Added in v3 after red-team review — see "Why the naive shape is UB"
below.)

The collector mutates `PageHeap` through `&mut self` (≈5000 lines in
`evac.rs` / `cycle.rs` / `mark.rs` / `alloc.rs` rely on this). A
multi-mutator design must let the collector keep that `&mut PageHeap`
while N mutator threads are *parked*. The naive shape — `Mutator`
holds `Arc<PageHeap>`, coordinator materializes `&mut PageHeap` via
`UnsafeCell` once all mutators are parked — **is undefined behavior**:
a parked mutator blocked inside `park()` holds a live `&PageHeap`
borrow across the `Condvar::wait` (it reached `self.heap.safepoint`
through the `Arc`). `&mut` aliasing a live `&` is UB under
Stacked/Tree Borrows regardless of the runtime guarantee that the
borrow is "idle."

Wrapping the *whole* `PageHeap` in a `RwLock` is also wrong: every
fast-path access a mutator makes (set a start bit, check `poisoned`,
bump `bytes_alloc_since_gc`, poll the safepoint) would then sit behind
the lock, killing the lock-free fast path.

**Decision: a two-structure split.**

```rust
/// Lock-free, atomically-accessed state. Cloned (via Arc) into every
/// Mutator AND held by the PageHeap. Never behind a lock.
pub struct SharedHeap<L: HeapLayout> {
    base: *mut u8,                     // immutable after construction
    n_pages: usize,                    // immutable
    poisoned: AtomicBool,              // moved out of PageHeap
    bytes_alloc_since_gc: AtomicUsize, // moved out of PageHeap
    start_bits: Box<[AtomicU64]>,      // was Arc<[AtomicU64]> in PageHeap
    cards: CardTable,                  // was Arc<CardTable> in PageHeap
    safepoint: Safepoint,              // §4
    gc_epoch: AtomicU64,               // §4 (Point 2 dedup)
    mutators: RwLock<Vec<Option<Arc<MutatorInner>>>>,
    mutator_count: AtomicUsize,
    _phantom: PhantomData<fn() -> L>,
}

/// Everything the collector needs `&mut` on. Stays MONOLITHIC and
/// keeps its current `&mut self` method surface unchanged. Lives
/// behind ONE lock held by the coordinator.
pub struct PageHeap<L: HeapLayout> {
    shared: Arc<SharedHeap<L>>,  // so existing methods still read
                                 // poisoned / start_bits / cards
    descs: Vec<PageDesc>,
    alloc_regions: [[AllocRegion; 2]; 3],
    mark_bits: Box<[u64]>,
    pinned_cells: HashSet<usize>,
    recycle_live_counts: Vec<u16>,
    // ... all remaining fields unchanged ...
}

pub struct GcCoordinator<L: HeapLayout> {
    shared: Arc<SharedHeap<L>>,
    heap: Mutex<PageHeap<L>>,    // or hand-rolled world lock; §4.4
    coord_mutex: Mutex<()>,      // serializes coordinator entries; §4.4
}
```

**Why this is sound *and* cheap:**

- A `Mutator<L>` holds `Arc<SharedHeap>` and a clone of the
  `Mutex<PageHeap>` handle (for TLAB refill only). It **never holds a
  bare `&PageHeap`.** `park()` touches `self.shared.safepoint`, a
  separate allocation. So when the coordinator does
  `heap.lock()` → `&mut PageHeap`, no parked mutator aliases it. Sound
  under Stacked/Tree Borrows.
- The fast path (bump in TLAB, `start_bits.fetch_or`, `poisoned.load`,
  `bytes_alloc_since_gc.fetch_add`, `safepoint` poll) touches only
  `SharedHeap` atomics — **zero locks.**
- TLAB **refill** is the only mutator op that needs `descs` /
  `alloc_regions` / the free list; it takes `heap.lock()` (slow path,
  ~1 per TLAB). At STW the coordinator holds the same lock,
  uncontended.
- The collector's existing `&mut PageHeap` code is **unchanged** —
  `PageHeap` stays monolithic. `start_bits` and `cards` were already
  `Arc`; moving them into `SharedHeap` and having `PageHeap` reach them
  via `self.shared` is a field-access redirect, not a rewrite.

**Rejected alternative (the red-team's literal proposal):** splitting
`PageHeap` into `SharedHeap` + `ExclusiveHeapState` and having the
collector take `&mut ExclusiveHeapState`. Correct, but it forces a
rewrite of every collector method that currently touches `descs`,
`mark_bits`, and `start_bits` together — thousands of lines, high bug
risk. The monolith-behind-one-lock shape above buys the same soundness
for ~150 lines of field migration.

**Migration cost:** ~150 lines in `space.rs` (extract `SharedHeap`,
move 4 fields, redirect accessors). Lands as the **first step of
Phase 2** (see §8) — Phase 1's serialized `Arc<Mutex<PageHeap>>` shape
does not hit the aliasing problem because it has no lock-free fast path
and no park, so Phase 1 ships unchanged.

### 2.1 The `Mutator<L>` handle

A `Mutator<L>` is the per-thread allocation handle. It owns the
mutator's TLABs and its safepoint-pending flag. It holds an
`Arc<SharedHeap>` (lock-free fast path) plus a refill handle to the
locked `PageHeap`, and is `!Send + !Sync` — a mutator is bound to the
thread that registered it.

```rust
pub struct Mutator<L: HeapLayout> {
    /// Lock-free shared state — start bits, poisoned flag, cards,
    /// safepoint, alloc counter. Fast path touches ONLY this.
    shared: Arc<SharedHeap<L>>,
    /// Refill handle into the locked PageHeap. Taken only on the TLAB
    /// slow path (~1 per TLAB) and never held across a safepoint park.
    /// NOT a bare `&PageHeap` — see §2.0 for why that would be UB.
    heap: Arc<Mutex<PageHeap<L>>>,
    /// Stable identifier (index into SharedHeap::mutators). Used by the
    /// coordinator to look up this mutator's metadata.
    id: MutatorId,
    /// Cached raw view of `shared.start_bits`, set once at registration
    /// (avoids a `shared` deref per cons alloc on the fast path). The
    /// start bits are OWNED by `SharedHeap` as `Box<[AtomicU64]>`; this
    /// is a borrowed view, not a second `Arc` (red-team A-5/B-5 — v3's
    /// `PageStartBits = Arc<[AtomicU64]>` field double-owned the data).
    /// Safe to cache as a raw slice because the reservation is fixed up
    /// front: `start_bits` is sized once in `with_reservation`
    /// (space.rs:392) and never reallocated — no heap-growth path
    /// (`MEM_RESERVE` of the full size at construction). The `Mutator`
    /// holds `Arc<SharedHeap>`, so the backing outlives this view.
    /// (Red-team round-1 Point 5: confirmed safe under fixed reservation.)
    start_bits: *const [AtomicU64],
    /// Per-(gen, kind) TLABs. See §3 — 6 entries total, indexed by
    /// `region_index(gen, kind)`.
    tlabs: [[Tlab; 2]; 3],
    /// Roots provider — see §5. v4 ships Snapshot only: the client
    /// publishes a `Vec<Word>` via `mutator.publish_roots` before each
    /// safepoint; the coordinator updates it in place; the client
    /// copies the forwarded values back. (`Walker` was cut — A-4/B-5.)
    roots: RootsSource,
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

#### Drop behavior — **STW-aware (v3 Point 4, refined v4 B-1)**

`Drop` runs not only on orderly shutdown but **during panic unwinding**
— a `panic!` anywhere in the mutator thread will drop a live
`Mutator<L>`. The drop path must therefore be both deadlock-free and
liveness-safe with respect to an in-progress collection. Naive
retirement (grab the refill lock, reconcile `words_used`) can deadlock
against the collector and, worse, leave the coordinator waiting forever
for a dead thread to reach a safepoint.

The drop sequence, in order:

1. **Mark inactive + deregister.** Set
   `inner.is_active.store(false, Ordering::Release)`, then
   `shared.mutators[id] = None`. The coordinator's per-mutator wait
   (§4.4) tests `is_active` in its predicate, so a mutator that goes
   inactive mid-cycle is dropped from the wait set — the coordinator
   never blocks forever on a departed thread (red-team B-1). The
   `is_active` Release pairs with the coordinator's Acquire load.
   (`mutator_count` is decremented too, but it's diagnostics-only in
   v4 — the wait iterates the registered set, not a counter.)
2. **Wake the coordinator.** `shared.safepoint.park_cv.notify_all()` so
   a coordinator blocked in its wait re-evaluates the predicate and
   observes this mutator's `is_active == false`.
3. **Do NOT touch the refill lock or `descs`.** TLAB tails are *not*
   reconciled on drop. The unused cells past each TLAB's bump pointer
   have no start bits set, so GC walkers already skip them; the
   already-consumed cells will be reclaimed when the page next goes
   fully empty. Losing the precise `words_used` delta for a dying
   mutator is acceptable slack (it only makes a trigger heuristic
   slightly conservative). This avoids both the deadlock (refill lock
   may be held by the coordinator) and any access to a possibly-poisoned
   heap mid-unwind.

**Why not reconcile `words_used` on drop:** it requires
`heap.lock()`, which the coordinator may hold for the whole STW cycle;
a panicking thread blocking on that lock would hang the unwind and, if
the coordinator is itself waiting on this thread to park, deadlock the
process. The non-reconciling drop is strictly safer; the only cost is
imprecise accounting that the next full cycle corrects.

**Orderly (non-panic) drop** does the same three steps. There is no
"retire under lock" fast path — a dropping mutator is leaving anyway,
so its TLAB tails are abandoned uniformly. The next GC reconciles the
real `words_used` from the page contents.

**Drop while this thread is itself parked** cannot happen: `park()`
borrows `&mut self`, so the handle can't be dropped concurrently from
the same thread, and `Mutator` is `!Send` so no other thread holds it.

### 2.2 Registration

Registration goes through the `GcCoordinator` (which owns both the
`Arc<SharedHeap>` and the `Arc<Mutex<PageHeap>>`), not through
`PageHeap` directly — `PageHeap` is now an implementation detail behind
the coordinator's lock.

```rust
impl<L: HeapLayout> GcCoordinator<L> {
    /// Register a new mutator on the current thread. The returned
    /// `Mutator<L>` is `!Send + !Sync` — keep it on the thread that
    /// called this method.
    pub fn register_mutator(&self, roots: RootsSource) -> Mutator<L> {
        // 0. Serialize with any in-flight STW cycle (red-team A-1/B-3).
        //    coord_mutex is held start-to-finish of a cycle (§4.4), so
        //    this blocks until no cycle is running. Registering OUTSIDE
        //    a cycle means the newcomer can't escape a world-stop it was
        //    never part of.
        let _g = self.coord_mutex.lock().unwrap();
        // 1. Allocate a MutatorInner. Crucially, initialize
        //    last_epoch = shared.safepoint.epoch.load(Acquire) — the
        //    CURRENT epoch. Because we hold coord_mutex, no cycle is in
        //    progress, so "current epoch" means "already at the latest
        //    safepoint." is_active = true, is_acting_coordinator = false.
        // 2. Push into shared.mutators; bump mutator_count (diagnostics).
        // 3. Clone shared + heap + coord_mutex Arcs into the Mutator.
        // 4. TLABs start empty (first alloc triggers a refill).
    }
}
```

**Why serialize registration with STW (red-team A-1 / B-3):** the v3
draft let registration run fully concurrently (`&self`, brief write
lock). A thread registering *during* an active cycle would copy the
already-bumped `epoch` into its `last_epoch`, conclude it has nothing to
do, and start mutating the heap **while the world is supposed to be
stopped** — an STW escape. Taking `coord_mutex` makes registration wait
out any in-flight cycle; the newcomer then joins at a quiescent point
with `last_epoch == epoch`, and participates normally in the *next*
cycle. (A fresh mutator has empty TLABs, so even absent this rule its
first alloc is a refill that blocks on `Mutex<PageHeap>` — but relying
on that is fragile; the explicit serialization is the correct fix.)

**Why still `&self`:** several threads hold coordinator clones and
register concurrently with each other; `coord_mutex` orders them and
orders them against cycles. Registration never needs `&mut PageHeap`.

**Coordinator handle sharing.** `GcCoordinator` is itself `Clone`
(both fields are `Arc`); clone it to each thread that needs to register
a mutator or trigger GC. The `coord_mutex` ensures only one collection
runs at a time regardless of how many coordinator clones exist.

### 2.3 The GC entry shape — **`&mut self`, via the §2.0 split**

This was the most-debated decision; §2.0 resolves the *how*. The
collector keeps its `&mut PageHeap` signatures unchanged. Exclusive
access is obtained by `heap.lock()` on the coordinator's
`Mutex<PageHeap>` — **sound precisely because, per §2.0, no parked
mutator holds a reference into the `PageHeap` allocation** (mutators
hold `Arc<SharedHeap>` for the fast path and only `lock()` the heap on
the refill slow path, never across a park).

Two shapes were considered and rejected:

- **`&self` everywhere with full interior mutability** — would force
  *every* collector-mutated field (descs, mark bits, alloc regions,
  recycle counts, pin sets) behind atomics or locks. Enormous
  bookkeeping cost for state that is exclusively owned during STW
  anyway. Rejected.
- **`UnsafeCell<PageHeap>` + "trust the safepoint"** — UB, because
  parked mutators hold live `&PageHeap` borrows (see §2.0). Rejected.

We keep `collect_minor`, `collect_major`, `collect_full`,
`try_collect_*` taking `&mut self` on `PageHeap` — *no change to their
signatures* — and add a top-level coordinator type:

```rust
pub struct GcCoordinator<L: HeapLayout> {
    shared: Arc<SharedHeap<L>>,    // lock-free; cloned into Mutators
    heap: Arc<Mutex<PageHeap<L>>>, // the monolith; §2.0
    coord_mutex: Arc<Mutex<()>>,   // serializes STW drivers; §4.4
}

impl<L: HeapLayout> GcCoordinator<L> {
    /// Build a coordinator + the shared handle.
    pub fn new(young_bytes: usize, old_bytes: usize) -> Self { ... }
    /// Hand out a `Mutator` that clones `shared`, `heap`, and
    /// `coord_mutex`. Serializes with an in-flight STW cycle (§2.2).
    pub fn register_mutator(&self, roots: RootsSource) -> Mutator<L>;
}
```

**The STW driver is a `Mutator`, not the coordinator (red-team B-2).**
The actual `with_world_stopped` and `try_collect_*` live on `Mutator`
(§2.5, §4.4): the triggering thread self-publishes and skips itself in
the wait loop. `GcCoordinator` is just the factory + shared-handle
owner; it does not itself drive cycles (a dedicated GC thread would
register its own `Mutator` handle — §2.5).

**Why `&mut PageHeap` (not `&self` everywhere):**

1. The collector mutates *enormous* amounts of state — page descriptors,
   start bits, alloc regions, mark bits, recycle counts, pin sets, card
   tables, the poison flag. Making every one of those atomic just to
   pretend the collector is `&self` is bookkeeping cost we never pay back.
   STW means the collector *is* the exclusive owner; the type system
   should say so.
2. The `try_collect_*` poison contract relies on exclusive heap access:
   the heap sets `shared.poisoned.store(true, Release)` while world-
   stopped, and the `world_running` Release/Acquire publishes it to
   mutators at resume (§2.5). No extra fences beyond those.
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

    /// Card barrier. Forwards to the card table, now owned by
    /// `SharedHeap` (it was already `Arc<CardTable>` with atomic
    /// interior mutability, so this is lock-free and concurrent-safe).
    #[inline]
    pub fn mark_card_at(&self, slot_addr: *const u8) {
        self.shared.mark_card_at(slot_addr);
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

### 2.5 GC trigger lives on `Mutator` + GC-epoch dedup (red-team B-2, round-1 Point 2)

**The trigger is a `Mutator` method, not a free-standing coordinator
call.** With no background GC thread, the thread that hits a trigger
*becomes* the STW driver. If that entry sat on a `GcCoordinator` and the
wait loop required every registered mutator (including the caller's own)
to park, the driver would deadlock waiting for itself to park (red-team
B-2). Putting the entry on `Mutator` lets the driver self-publish and
mark `is_acting_coordinator` so the wait loop skips it (§4.4).

```rust
impl<L: HeapLayout> Mutator<L> {
    pub fn try_collect_minor<F>(&mut self, visit_extra_roots: F)
        -> Result<CollectOutcome, GcError>
    where F: FnMut(&mut PageEvacuator<'_, L>),
    {
        // Snapshot the GC epoch BEFORE we queue for the world-stop.
        let seen = self.shared.gc_epoch.load(Ordering::Acquire);
        // with_world_stopped (§4.4): self-publishes our TLABs+roots,
        // sets is_acting_coordinator, parks every OTHER active mutator,
        // then runs the closure with &mut PageHeap.
        self.with_world_stopped(|heap| {
            // Coalesce: another driver may have run a cycle while we
            // queued on coord_mutex. If so, the allocation that
            // triggered us likely has room now — skip the redundant
            // (probably empty) cycle.
            if self.shared.gc_epoch.load(Ordering::Relaxed) != seen {
                return Ok(CollectOutcome::AlreadyCollected);
            }
            // Roots = every active mutator's published snapshot (incl.
            // ours, published during self-park) + the caller's extra
            // closure. The evacuator visits/updates each in place; §5.
            let r = heap.try_collect_minor_with_published_roots(
                &self.shared, visit_extra_roots)?;
            self.shared.gc_epoch.fetch_add(1, Ordering::Release);
            Ok(CollectOutcome::Ran(r))
        })
    }
}
```

**Why the dedup (the "thundering herd"):** when `young_page_cap` is hit
under N concurrent mutators, several threads each get `None` from
`try_alloc_*` and each calls `try_collect_minor`. `coord_mutex`
serializes them, so without dedup the second, third, … drivers each
park the world and run a *fresh, empty* cycle right after the first
freed space. The `gc_epoch` snapshot-then-compare turns those into
no-ops: a driver whose snapshot is stale knows a cycle already ran and
returns `AlreadyCollected`. The triggering allocation then retries (see
the alloc-fail → trigger → retry loop in §3.5) and usually succeeds.

`CollectOutcome` is a new enum (`Ran(CollectResult)` |
`AlreadyCollected`); the single-mutator `PageHeap::collect_*` methods
keep returning bare `CollectResult` unchanged.

**Dedicated-GC-thread variant (optional).** A non-mutator thread *can*
drive GC, but it must register a `Mutator` handle of its own (so it
participates in the `is_acting_coordinator` protocol) or call a
`GcCoordinator::collect_*` that first registers a transient driver
handle. The common, recommended path is mutator-triggered as above.

The poison contract is unchanged in shape; only the concurrency
discipline is added:
- `with_world_stopped` parks all mutators, then calls
  `PageHeap::try_collect_minor` (existing method, unchanged).
- If that returns `Err`, the heap has set
  `shared.poisoned.store(true, Ordering::Release)` (the only writer;
  runs while world-stopped). The `?` above propagates the `Err` out of
  the closure; `gc_epoch` is *not* bumped on the error path (a failed
  cycle didn't free anything, so it shouldn't suppress a retry — though
  the poison flag will make that retry fail fast anyway).
- The world-resume `world_running.store(1, Release)` is sequenced after
  the poison store. Every mutator's unpark waits on a matching
  `world_running.load(Acquire)`, establishing happens-before with the
  poison store.
- Mutator-side checks are `self.shared.poisoned.load(Ordering::Acquire)`
  — see §7. One extra Acquire load per allocation in the simplest
  implementation; see §9 D-POISON-CHECK for the check-at-refill option.

**Migration note:** the existing implementation (this PR's parent
commit) uses `poisoned: bool` (plain) *inside `PageHeap`*. The split
(§2.0) moves it to `SharedHeap::poisoned: AtomicBool`. Every existing
`if self.poisoned { return None; }` allocator gate becomes
`if self.shared.poisoned.load(Acquire) { return None; }`. The single
STW writer becomes `shared.poisoned.store(true, Release)` at the
existing site (`run_catching_oom`'s `Err` branch). `PageHeap` reaches
`shared.poisoned` through its `self.shared` Arc, so single-mutator
callers see identical behavior.

### 2.6 Thread-local mutator integration (frontend-owned, Q6)

The core keeps `Mutator<L>` an **explicit handle** — no `thread_local!`
inside `newgc-core`, so it stays portable and TLS-free. But neither
frontend wants to thread a handle argument through every allocation call
site (NCL's JIT emits bare `nod_make`/`ncl_alloc_cons` C-ABI calls;
Dylan's codegen is similar). So **each frontend owns a `thread_local!`
"current mutator"** and its alloc shim fetches it:

```rust
// In the FRONTEND runtime (NCL / Dylan), not in newgc-core:
thread_local! {
    // Raw pointer, not the Mutator itself — fetch must be a single TLS
    // load + deref on the allocation fast path, NOT an Rc/HashMap lookup.
    static CURRENT: Cell<*mut Mutator<L>> = Cell::new(ptr::null_mut());
}
#[no_mangle] extern "C" fn ncl_alloc_cons(...) -> *mut u64 {
    let m = CURRENT.with(|c| c.get());           // 1 TLS load
    unsafe { (*m).try_alloc_cons_in(G0) }.map_or(null, |p| p.as_ptr())
}
```

Requirements this places on the core:
- `Mutator::try_alloc_*` must be cheap to call through a raw pointer
  (it is — fast path is a TLAB bump touching only `SharedHeap` atomics).
- The frontend sets `CURRENT` when a thread registers its mutator and
  clears it at thread exit / mutator drop.
- NCL's `(make-thread)` model fits directly: each Lisp thread registers
  its own `Mutator`, stashes it in `CURRENT`, and owns its own
  conservative stack range + `push_root`/`pop_root` root stack.

Windows-vs-Linux TLS is a non-issue: Rust's `thread_local!` /
`std::thread::LocalKey` works on both, and the frontends are Windows-
first. The core exposes nothing platform-specific.

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
2. Take the heap lock (`heap.lock()` on the coordinator's
   `Mutex<PageHeap>` — this is the "alloc_mutex" role; it guards the
   central `AllocRegion`, `descs`, and free-page acquisition).
3. **Account the exhausted TLAB:** the old TLAB was pre-charged for its
   full reserved size at *its* refill, and a refill happens because it's
   now (near) full — so no correction is needed in the common case. If
   the refill is triggered with the old TLAB only partially used (rare:
   a too-large allocation), correct `words_used` for the unused tail
   here while the lock is held. (This is the only place a *running*
   mutator touches `words_used`; the common park path does not — see
   §3.3's reconciliation note.)
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

The cost: between refill and reconciliation, `PageDesc::words_used`
overstates the live data in the page by the unused TLAB tail. GC, when
it runs, corrects this — but **not** by having each mutator grab
`alloc_mutex` at park (that would serialize N parking mutators on one
lock and inflate STW latency — red-team Point 3). Instead:

- **At park, each mutator *publishes* its live TLAB cursors** — for
  every non-empty TLAB, store `(page_idx, start, bump)` into its
  `MutatorInner` slot — then **clears** the TLAB (`start = end = bump =
  null`). The publish is a handful of plain stores into mutator-private
  memory; it touches no shared lock.
- **The coordinator reconciles single-threaded under STW.** After all
  mutators have reached the safepoint, it walks every `MutatorInner`'s
  published cursors and corrects each page's `words_used` down to the
  true high-water mark. It holds the heap exclusively, so no lock is
  needed for these writes.

  **Units — `cell` == `word` == 8 bytes in this codebase.** `words_used`
  is counted in 8-byte cells (`alloc.rs` bumps it by `n_cells`;
  `PAGE_SIZE_CELLS = 65536/8 = 8192`), and a cons occupies 2 cells.
  `Tlab.start`/`bump` are `*mut u64`, so `bump.offset_from(start)`
  already yields the count of u64s = **cells** — no byte-scaling. The
  reconciliation is therefore:

  ```rust
  // per published (page_idx, start, bump), with reserved_cells from refill:
  let used_cells = unsafe { bump.offset_from(start) } as u32;
  debug_assert!(used_cells <= reserved_cells);
  desc[page_idx].words_used -= reserved_cells - used_cells;
  ```

  (v3 wrote `(bump-start)/8`, which double-divides given the `*mut u64`
  typing — fixed here per red-team B-4. Note B's suggested `reserved_cells
  * 2` is *also* wrong for this codebase: it assumes `cell` = cons =
  2 words, but here `cell` is the 8-byte word itself.)

Clearing the TLAB at the safepoint is also *required for correctness*,
not just tidiness: a TLAB's page is a live G0 page that GC may evacuate
and reclaim. The mutator must not keep bumping into a page that GC moved
out from under it. After resume, the mutator's TLABs are empty and its
next allocation triggers a fresh refill onto a fresh page.

The `accounted_cells` field on `Tlab` (§2.1) is therefore vestigial in
this model — reconciliation is driven by the published `(start, bump)`
pair, not an incrementally-maintained counter. It can be dropped.

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

### 4.1 Atomic flag location: **per-mutator epoch, no global counter**

v4 (red-team B-1/B-2) replaces the global `parked_count` integer with
**per-mutator state the coordinator inspects directly.** A bare counter
conflates "N park-events happened" with "these specific N live mutators
are at the safepoint," and that ambiguity becomes a real hazard once
registration (§2.2) and mutator-triggered GC (§2.5) can change the live
set mid-cycle. The coordinator instead snapshots the registered set and
waits on each mutator's own `last_epoch`.

```rust
pub(crate) struct Safepoint {
    /// Global epoch. Bumped (under coord_mutex) when a safepoint is
    /// requested; mutators compare against their last-seen value.
    epoch: AtomicU64,
    /// Coordinator-controlled flag mutators wait on after reaching the
    /// safepoint. 0 = world stopped, must block. 1 = world running.
    world_running: AtomicU8,
    /// Park/unpark coordination — Mutex + Condvar (std; portable).
    park_mutex: Mutex<()>,
    park_cv: Condvar,
}
```

And per-mutator (in `SharedHeap::mutators[id]`):

```rust
struct MutatorInner {
    /// Last safepoint epoch this mutator has reached (parked at OR is
    /// driving as the acting coordinator). The coordinator waits for
    /// `last_epoch >= target_epoch`. Released-stored by the mutator,
    /// Acquire-loaded by the coordinator.
    last_epoch: AtomicU64,
    /// False once this mutator has begun Drop/deregister. The
    /// coordinator stops waiting on a mutator whose `is_active` is
    /// false (red-team B-1) — it has left and will touch no heap state.
    is_active: AtomicBool,
    /// True while THIS mutator is the thread driving the current STW
    /// cycle. The coordinator wait loop skips itself (red-team B-2) —
    /// the driver self-publishes its TLABs+roots up front instead of
    /// parking, so requiring it to "park" would deadlock.
    is_acting_coordinator: AtomicBool,
    /// Thread execution state for the native-call convention (§4.6):
    /// `InDylan` = running Dylan code, may touch the heap, must reach
    /// safepoints by polling; `InNative` = blocked in foreign code
    /// (Win32/COM), touches no Dylan heap, treated by the collector as
    /// already-at-safepoint. The Dylan target's UI thread spends most of
    /// its life `InNative` (blocked in `GetMessageW`) — without this it
    /// would hold every GC hostage for the 10 s timeout.
    state: AtomicU8,  // 0 = InDylan, 1 = InNative
    /// Most-recent roots snapshot, owned by the mutator, read+updated
    /// in place by the collector. See §5.
    roots_snapshot: Mutex<Vec<Word>>,
    /// Published TLAB cursors, written by the mutator when it reaches a
    /// safepoint, read by the coordinator while world-stopped to
    /// reconcile `words_used` (red-team round-1 Point 3). One entry per
    /// non-empty TLAB: (page_idx, start, bump). Cleared by the
    /// coordinator after it reconciles.
    published_tlabs: Mutex<Vec<(usize, *mut u64, *mut u64)>>,
}
```

The `published_tlabs` / `roots_snapshot` `Mutex`es are per-mutator and
never contended in practice: the owning mutator writes them only when
reaching a safepoint, and the coordinator reads them only after
observing `last_epoch >= target` for that mutator. They exist for
`Send`-safety of the raw cursors across the publish/read boundary, not
for mutual exclusion.

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

> **Poll-site contract (frontend obligation; NCL feedback).** A
> `poll_safepoint` call site must be a **GC-safe point**: at that site,
> the mutator's published precise-root set (§5.1) must be *complete and
> consistent* — every live in-flight `Word` is already in the published
> snapshot (or reconstructible by the frontend on resume). A poll
> emitted mid-expression, where a live temporary sits in a register the
> frontend hasn't yet recorded as a root, lets the collector move that
> object and resume the thread with a stale pointer. **The core cannot
> detect this** — it trusts the published snapshot. Placing polls only
> at GC-safe points (function entry, loop back-edges, post-call — points
> where the frontend's `push_root`/statepoint state is settled) is the
> *frontend's* codegen obligation. This is the one piece of the
> safepoint protocol the core defines but cannot enforce; it is why a
> frontend can't bolt polls on naively. Both NCL's ncl-llvm and Dylan's
> codegen own this guarantee for their own back-edge polls.

### 4.3 Parking mechanism: **Mutex + Condvar**

```rust
impl<L> Mutator<L> {
    #[inline]
    pub fn poll_safepoint(&mut self) {
        let sp = &self.shared.safepoint;   // NB: self.shared, not self.heap
        let global_epoch = sp.epoch.load(Ordering::Acquire);
        let my_epoch = self.inner().last_epoch.load(Ordering::Relaxed);
        if global_epoch == my_epoch {
            return;  // Fast path: no safepoint pending.
        }
        self.park(global_epoch);
    }

    #[cold]
    fn park(&mut self, target_epoch: u64) {
        let sp = &self.shared.safepoint;
        // 1. Publish TLAB cursors and CLEAR the local TLABs. No lock on
        //    the central heap — just per-mutator stores. The coordinator
        //    reconciles words_used single-threaded once we're parked
        //    (round-1 Point 3). Clearing is mandatory: our TLAB page may
        //    be evacuated by this GC, so we must refill fresh on resume.
        self.publish_and_clear_tlabs();
        // 2. Snapshot roots (see §5).
        self.refresh_roots_snapshot();
        // 3. Announce arrival by advancing OUR epoch. The Release store
        //    publishes steps 1-2 to the coordinator's per-mutator Acquire
        //    load (§4.4). No global counter — the coordinator polls this
        //    mutator's last_epoch directly (red-team B-1).
        self.inner().last_epoch.store(target_epoch, Ordering::Release);
        // Wake the coordinator in case we're the last one it's waiting on.
        sp.park_cv.notify_all();
        // 4. Block until world_running == 1.
        let mut guard = sp.park_mutex.lock().unwrap();
        while sp.world_running.load(Ordering::Acquire) == 0 {
            guard = sp.park_cv.wait(guard).unwrap();
        }
        // 5. (Optional) re-check poison here, before resuming, via
        //    self.shared.poisoned.load(Acquire) — the Acquire on
        //    world_running already established happens-before with the
        //    coordinator's poison store, so the next fast-path alloc
        //    will observe it; an explicit recheck just fails faster.
    }
}
```

Note both `poll_safepoint` and `park` reach the safepoint through
`self.shared` (an `Arc<SharedHeap>`), **never** through a `&PageHeap`.
This is what makes the coordinator's `&mut PageHeap` sound — see §2.0.

### 4.4 Coordinator side

The STW driver is **always a mutator** (red-team B-2): there is no
background GC thread, so the thread that hits a GC trigger drives the
cycle. It therefore can't "park itself" — instead it self-publishes its
TLABs+roots, marks `is_acting_coordinator`, and the wait loop skips it.
`with_world_stopped` is the inner primitive; the public entry is
`Mutator::try_collect_*` (§2.5).

```rust
impl<L: HeapLayout> Mutator<L> {
    /// Internal: drive a world-stop from THIS mutator's thread.
    fn with_world_stopped<R>(
        &mut self,
        f: impl FnOnce(&mut PageHeap<L>) -> R,
    ) -> R {
        let sp = &self.shared.safepoint;
        // Serialize coordinator entries; also blocks registration (§2.2)
        // and makes the gc_epoch dedup (§2.5) correct.
        let _coord = self.coord_mutex.lock().unwrap();

        // (a) Self-park: publish our own state up front so the cycle
        // sees this thread's roots/TLABs WITHOUT us blocking. Mark
        // ourselves as the driver so the wait loop skips us.
        self.publish_and_clear_tlabs();
        self.refresh_roots_snapshot();
        self.inner().is_acting_coordinator.store(true, Ordering::Release);

        // (b) Request the safepoint.
        let target = sp.epoch.fetch_add(1, Ordering::AcqRel) + 1;
        self.inner().last_epoch.store(target, Ordering::Release);
        sp.world_running.store(0, Ordering::Release);

        // (c) Wait for every OTHER active mutator to reach `target`.
        // We snapshot the registered set, then wait on each one's own
        // last_epoch — no global counter (red-team B-1). A mutator that
        // is the driver (us) or that has gone inactive (Drop, B-1) is
        // not waited on.
        let others: Vec<Arc<MutatorInner>> = self
            .shared.mutators.read().unwrap()
            .iter().flatten().cloned().collect();
        let mut guard = sp.park_mutex.lock().unwrap();
        for m in &others {
            // A mutator counts as "at the safepoint" if it has reached
            // this epoch by polling, OR it is blocked in native code
            // (InNative — §4.6: touches no Dylan heap, safe to collect
            // around). We wait only on active, in-Dylan, non-driver
            // mutators that haven't yet reached `target`.
            while m.is_active.load(Ordering::Acquire)
                && !m.is_acting_coordinator.load(Ordering::Acquire)
                && m.state.load(Ordering::Acquire) == IN_DYLAN
                && m.last_epoch.load(Ordering::Acquire) < target
            {
                guard = sp.park_cv
                    .wait_timeout(guard, Duration::from_secs(10))
                    .unwrap().0;
                // On timeout: log which mutator id is lagging and how
                // long since its last epoch tick. Keep waiting (do not
                // force-resume — that would race the laggard).
            }
        }
        drop(guard);

        // (d) World is stopped. Take the monolith. Sound because no
        // mutator holds a reference INTO PageHeap (§2.0): the others
        // are blocked in park() on Arc<SharedHeap>, and we (the driver)
        // hold only our own Arc<SharedHeap> until this lock() call.
        let mut heap = self.heap.lock().unwrap();
        // Reconcile published TLAB cursors (ours + every parked
        // mutator's) into PageDesc::words_used — single-threaded.
        heap.reconcile_published_tlabs(&self.shared);
        let result = f(&mut *heap);
        drop(heap);

        // (e) Resume. The Release on world_running pairs with the
        // Acquire in each mutator's park() wait, so heap mutations are
        // visible before any mutator's next fast-path alloc.
        sp.world_running.store(1, Ordering::Release);
        sp.park_cv.notify_all();
        self.inner().is_acting_coordinator.store(false, Ordering::Release);
        result
    }
}
```

**No `parked_count` reset, no leaving-counter.** v3 reset a global
counter between cycles and reasoned about a fast-waking-mutator race;
v4 has no such counter. Re-running a cycle just bumps `epoch` again
(under `coord_mutex`), and each mutator parks afresh because its
`last_epoch` lags the new `epoch`. The driver's `is_acting_coordinator`
is cleared last, after `notify_all`, all under `coord_mutex` — so a
subsequent cycle (which must re-take `coord_mutex`) always sees a clean
`is_acting_coordinator == false` state.

**Happens-before audit — TLAB cursor publication → coordinator reconciles:**

1. Mutator side (`park()` in §4.3): `publish_and_clear_tlabs()` stores
   `(page_idx, start, bump)` into its own `MutatorInner.published_tlabs`,
   then `last_epoch.store(target, Release)`. The Release publishes those
   stores.

2. Coordinator side (this function, step (c)): for each `m`, the
   `m.last_epoch.load(Acquire)` in the wait predicate is the matching
   Acquire. The loop exits for `m` only after observing
   `last_epoch >= target`, establishing happens-before with that
   mutator's publish stores.
3. `heap.reconcile_published_tlabs(&self.shared)` runs after the loop
   exits for every `m`, so it observes each mutator's published cursors
   and corrects `PageDesc::words_used` single-threaded.

The `park_mutex` provides the same happens-before independently —
mutators release it via `park_cv.wait()`'s internal release-then-block,
the coordinator acquires it before reading — but the per-mutator
`last_epoch` Release/Acquire pairing is the *primary* synchronization.

**Why not retire under `alloc_mutex` at the safepoint (rejected v2 idea):**
an earlier draft had each arriving mutator grab `alloc_mutex` to
reconcile its own `words_used`. Round-1 Point 3 flagged that this
serializes all N arriving mutators on one lock, inflating time-to-
safepoint (= STW pause start latency). Publish-cursors moves that work
to the single-threaded, already-exclusive coordinator — zero lock
contention on the arrival path.

**Unpark sequence diagram** (driver = mutator C, one other mutator M1):

```
   Mutator M1                     Driver-mutator C (in with_world_stopped)
   ----------                     ----------------------------------------
                                  coord_mutex.lock()
                                  publish_and_clear_tlabs()  [C's own]
                                  refresh_roots_snapshot()   [C's own]
                                  C.is_acting_coordinator = true
                                  target = epoch.fetch_add(1) + 1  // = N
                                  C.last_epoch = N
                                  world_running.store(0, Release)
                                  park_mutex.lock()
                                  for M1: while M1.is_active
                                          && !M1.is_acting_coordinator
                                          && M1.last_epoch < N:
                                            park_cv.wait_timeout(...)
   poll_safepoint
   epoch.load(Acquire) = N
   last_epoch = N-1  → park(N):
       publish_and_clear_tlabs()  [stores to MutatorInner; TLABs emptied]
       last_epoch.store(N, Release)
       park_cv.notify_all()
       park_mutex.lock()
       while world_running == 0:
            park_cv.wait(guard)   [releases park_mutex, sleeps]
                                          wakes; M1.last_epoch==N >= N → exit for M1
                                          drop(guard)
                                          heap.lock()  /* &mut PageHeap */
                                          reconcile_published_tlabs() [C + M1]
                                          f(&mut heap)
                                          world_running.store(1, Release)
                                          park_cv.notify_all()
                                          C.is_acting_coordinator = false
                                          coord_mutex.unlock()
       wakes from notify_all
       world_running.load(Acquire) == 1  --> exit
   /* M1's TLABs are empty; next alloc refills onto a fresh page */
   continues
```

**Why this is race-free:**
- There is no global counter to reset, so no cross-cycle ABA on it.
  Mutators leaving the cycle only check `world_running == 1`.
- `coord_mutex` makes cycles strictly sequential: a second cycle
  can't begin until the first clears `is_acting_coordinator` and unlocks,
  so the driver flag is always observed `false` at the start of the next.
- A second cycle bumps `epoch` again; a straggler M1 still in its
  `world_running == 0` wait simply sees the resume of the *first* cycle
  (`world_running == 1`), leaves, and re-parks on its next poll when it
  sees the new epoch. No infinite loop, no stale state.
- The wait predicate's `is_active` term (set false by Drop, §2.1) means
  a mutator that departs mid-cycle is dropped from the wait — the
  coordinator never blocks forever on a thread that has left
  (red-team B-1). A departed mutator touches no heap state post-Drop, so
  proceeding without it is sound.

**"Acquire exclusive access to PageHeap" — resolved by §2.0.** The
coordinator holds `heap: Arc<Mutex<PageHeap>>` and obtains `&mut
PageHeap` via `heap.lock()`. This is sound because no parked mutator
holds a reference into the `PageHeap` allocation (they hold
`Arc<SharedHeap>`; §2.0). The two implementation options the v1 draft
debated are both retired:

- The v1 **(I)** "`RwLock<PageHeap>`, mutators take a *read* lock on
  refill" was wrong: refill *writes* `descs` (flips a free page), so it
  needs exclusive access among mutators anyway. A plain `Mutex<PageHeap>`
  is the right primitive — refills serialize with each other (slow
  path, fine) and the collector takes it uncontended at STW.
- The v1 **(II)** "`UnsafeCell<PageHeap>`, trust the safepoint" was
  *unsound* (parked mutators alias it — §2.0). Rejected.

The mutator **fast path takes no lock at all** — it bumps inside its
TLAB and touches only `SharedHeap` atomics. Only TLAB refill (~1 per
TLAB) takes `heap.lock()`. If that `Mutex` shows up in profiles, the
escalation path is to shard the central allocator (multiple
`AllocRegion` arrays under separate mutexes), not to reach for
`UnsafeCell`.

### 4.5 Deadlock and timeout handling

- **Timeout:** the coordinator waits with a 10s timeout per
  condition-variable wait, then logs which mutator(s) haven't arrived.
  It does NOT force-resume; that would race with the unarrived mutator.
  This is intentional: deadlocks are bugs to fix, not paper over.
- **Test hook:** a `force_timeout_panic_after(Duration)` builder option
  on `GcCoordinator` lets tests assert that a stuck mutator causes the
  coordinator to bail loudly rather than hang.
- **Re-entrant GC:** calling a trigger (`mutator.try_collect_*`) from
  *inside* the `with_world_stopped` closure would re-take `coord_mutex`
  on the same thread and self-deadlock (std `Mutex` is not reentrant).
  The closure runs collector code with `&mut PageHeap` and must never
  call a `Mutator` trigger. This is a hard rule, not a runtime guard;
  document it on `with_world_stopped`. The acting-coordinator's own
  GC is *not* re-entrant — it's the single top-level `with_world_stopped`
  call; `is_acting_coordinator` only suppresses the wait-on-self, it
  does not enable nesting.

### 4.6 Native-call boundary convention (Dylan Q4 caveat — **the one new piece**)

Cooperative polling assumes a thread periodically runs Dylan code that
hits a poll. A thread **blocked in foreign code** — the UI thread in
`GetMessageW`, a worker in a D3D present or a COM call via the `windows`
crate — runs no Dylan code and hits no poll, sometimes for seconds. With
the §4.4 wait loop as-is, such a thread would hold every GC hostage
until the 10 s timeout. This is the standard "thread is in native code =
at a safepoint by convention" problem; the thread isn't touching the
Dylan heap while in foreign code, so it is *safe to collect around it.*

**Thread state machine.** `MutatorInner.state` is `InDylan` or
`InNative`. The collector's wait predicate (§4.4) already skips
`InNative` mutators. Transitions are explicit calls the Dylan runtime
emits around foreign calls that may block:

```rust
impl<L> Mutator<L> {
    /// Call immediately BEFORE a foreign call that may block / run long.
    /// After this returns the thread must not touch the Dylan heap until
    /// `leave_native` returns.
    pub fn enter_native(&mut self) {
        // 1. Publish state the collector will need to GC around us:
        //    our TLAB cursors (so words_used reconciles) and our root
        //    snapshot (the collector MOVES objects — any live Dylan
        //    pointer in our frame must be updated in place while we
        //    block; we copy the updated values back in leave_native).
        self.publish_and_clear_tlabs();
        self.refresh_roots_snapshot();
        // 2. Announce InNative. Release so steps 1's stores are visible
        //    to a collector that observes InNative.
        self.inner().state.store(IN_NATIVE, Ordering::Release);
        // (We do NOT touch last_epoch; the predicate skips InNative
        //  regardless of epoch, so we stay "arrived" across any number
        //  of cycles while blocked.)
    }

    /// Call immediately AFTER the foreign call returns, before touching
    /// the Dylan heap again.
    pub fn leave_native(&mut self) {
        let sp = &self.shared.safepoint;
        // If a cycle is in progress, block until it resumes BEFORE going
        // back to InDylan — otherwise we'd race the collector that is
        // mutating the heap right now.
        let mut guard = sp.park_mutex.lock().unwrap();
        while sp.world_running.load(Ordering::Acquire) == 0 {
            guard = sp.park_cv.wait(guard).unwrap();
        }
        // Re-enter Dylan at the current epoch (we never owed a poll
        // while native; adopt the latest so our next poll is a fast no-op).
        self.inner().last_epoch.store(
            sp.epoch.load(Ordering::Acquire), Ordering::Release);
        self.inner().state.store(IN_DYLAN, Ordering::Release);
        drop(guard);
        // Our TLABs are empty (cleared at enter_native); next alloc
        // refills. Our root snapshot now holds forwarded values; the
        // caller copies them back to real slots (same as post-poll, §5).
    }
}
```

**Transition races — the two windows that must be closed:**

1. *Entering as a cycle starts.* If `enter_native` flips to `InNative`
   just as the driver bumps `epoch`, the driver simply skips us (good —
   we published roots+TLABs in step 1 *before* the `Release` store, so
   the collector sees consistent state). If instead the driver already
   passed us in its scan and is now collecting, we must NOT have touched
   the heap after the `Release` — guaranteed by the contract that no
   heap access happens between `enter_native` and `leave_native`.
2. *Leaving as a cycle runs.* `leave_native` blocks on `world_running`
   *before* flipping to `InDylan`, so a returning thread never resumes
   heap access while the collector owns `&mut PageHeap`. This mirrors
   the tail of `park()`.

**Short vs. blocking foreign calls.** Only calls that may *block or run
long* need the `InNative` transition (the cost is two atomic stores + a
root publish). A short, non-blocking foreign call (a few instructions of
C) can stay `InDylan`: it completes well before the next back-edge poll,
so no GC can start mid-call. The frontend decides which boundaries
warrant the transition — a reasonable default is "any call that can
block" (message pumps, I/O, locks, GPU present).

**Unwinding through frames (NCL Q6 — real, not paper).** NCL requires
that GC not run during an active unwind. Today that holds *by accident*:
pure Rust/SEH panic propagation doesn't allocate or poll, so it never
reaches a safepoint. But `unwind-protect` / `handler-case` cleanup forms
are ordinary Lisp that *can* allocate — and an allocation during unwind
hits the alloc slow path, which can trigger or park for a GC mid-unwind.
There are two consistent positions, and the frontend must pick one:
(a) **cleanup forms are normal Dylan/Lisp code** that reach safepoints
like anything else — then nothing special is needed, the unwinding
thread participates in STW normally (this is NCL's *current* de-facto
behavior); or (b) **unwinding is a distinct phase** that should be
treated like `InNative` (parked, not collected-through) — then the
frontend must publish unwind-enter/exit transitions, which NCL does not
do today. Recommendation: **(a)** — cleanup Lisp is just Lisp; it
allocates through the same safepoint-cooperating path. Choose (b) only
if there's cleanup that must run with GC structurally forbidden (e.g.
touching half-torn state); none is known today. Flagged so the choice is
explicit rather than "works by accident."

**⚠ FFI object pinning (NCL Q7 — resolved as a core requirement).**
`enter_native` makes the *thread* safe to collect around, and updates
the thread's *own* root slots. It does **not** protect an object whose
address was *passed into* the foreign call and is dereferenced by the
foreign code while we're blocked — e.g. `SetWindowTextW(hwnd, str_ptr)`
where a concurrent GC moves `str`. The foreign code holds a raw copy of
the old address that the in-place root update cannot reach.

**Resolved with NCL (Q7): the core needs an explicit pin/unpin API —
designed in §5.4.** Today NCL pins FFI objects only *incidentally* (the
address sits on the native stack, so the conservative scan pins it);
a precise-primary build dials the scan back, so that incidental pin
vanishes. NCL needs in-place pinning, not copy-to-buffer, because its
Win32 **callback** path holds Lisp closures for the **process
lifetime**. The full `pin(w) → PinHandle` / `unpin` design — semantics,
the `SharedHeap::explicit_pins` set, and how it reuses the existing
in-place page-flip — is **§5.4**. Self-contained `newgc-core` work;
does not block the multi-mutator core.

---

## 5. Root enumeration

Two **complementary** root sources, not alternatives — a build can use
either or both:
- **§5.1 Precise `Snapshot` roots** — exact live `Word` values the client
  publishes and the collector updates in place. **Primary for both
  frontends today** (Dylan: Sprint-45c safepoint maps; NCL:
  `ncl_push_root`/`pop_root` at all GC-triggering sites). (The `Walker`
  closure variant was cut — §5.1.)
- **§5.3 Conservative stack-range pins** — `conservative-pin` feature;
  NCL's current primary path. Pins pointer-shaped stack words against
  movement.

A precise-only build (Dylan) uses §5.1 alone. A conservative build (NCL
today) uses §5.3, optionally plus §5.1 once push/pop lands. The
collector moves every object that is neither a published precise root's
target nor conservatively pinned.

### 5.1 Precise roots — **Snapshot only (`Walker` cut per red-team A-4/B-5)**

```rust
mutator.publish_roots(&current_roots);  // copies values into roots_snapshot
mutator.poll_safepoint();               // or any trigger entry
```

The mutator publishes its current root *values* into
`MutatorInner.roots_snapshot` before reaching a safepoint; the
coordinator reads and updates that `Vec<Word>` in place while the world
is stopped; after resume the mutator copies the (possibly forwarded)
values back to its real root locations.

`RootsSource` collapses to a unit marker (or is dropped entirely):

```rust
pub enum RootsSource {
    /// Mutator publishes a Vec<Word> snapshot before each safepoint;
    /// the coordinator updates it in place. The mutator owns the
    /// mapping from snapshot index → real slot and copies back on resume.
    Snapshot,
}
```

**Why `Walker` was cut.** v3 offered a `Walker(Box<dyn FnMut(&mut
Vec<Word>)>)` variant that "pushes root values" during STW. Red-team
B-5 showed this is **fiction for a moving collector**: a closure that
pushes *values* into a `Vec` retains no mapping back to the *slots*
those values came from, so there is nowhere to write the forwarded
pointers. (And v3's two passages contradicted each other on whether the
closure even runs on the mutator's thread or the coordinator's — red-team
A-4 — and the park protocol provides no way to run coordinator code on a
blocked mutator's thread.) A correct walker would have to record slot
*addresses*, at which point it *is* a `Snapshot`. So we ship `Snapshot`
only; it matches the statepoint model both frontends already use (NCL's
`push_root`/`pop_root`, Dylan's Sprint-45c maps).

### 5.2 Lifetime / write-back

Roots are typed `Word` (8-byte values). The evacuator updates them *in
place* — it writes forwarded pointers back into the snapshot `Vec`. So:

- `roots_snapshot: Mutex<Vec<Word>>` lives on `MutatorInner`. The
  coordinator locks it under the world-stopped barrier (uncontended —
  the owner is at the safepoint), updates `&mut [Word]`, unlocks.
- **The mutator owns the index → real-slot mapping.** It built the
  snapshot from its known root locations (registers, stack slots), so
  after `poll_safepoint`/trigger returns it copies the updated values
  back to those locations. This is exactly the statepoint contract; the
  GC never needs the slot addresses, only the values + the client's
  promise to copy back.
- The acting-coordinator mutator publishes its *own* snapshot during
  self-park (§4.4 step (a)) and copies back when `with_world_stopped`
  returns — same contract, no special case.

### 5.3 Conservative pins from mutator stacks — **first-class for NCL**

This path is **not** legacy and **not** semispace-only — it is NCL's
*current primary root source* on the page-heap (`--features
conservative-pin`, sub-phase 6, landed). NCL's LLVM JIT spills tagged
Lisp `Word`s and plain native ints/pointers onto one stack with no
compiler-enforced separation, so the collector cannot treat the stack
as precise; it conservatively pins anything pointer-shaped. (Dylan
builds with this feature *off*; NCL will keep it as belt-and-suspenders
after its push/pop precise roots land.)

The existing `pin_pointers_in_ranges` API consumes `&[(usize, usize)]`
of stack address ranges. **Multi-mutator extension:** each mutator
publishes its own stack range at the safepoint (a `parked_stack_range`
on `MutatorInner`, set via `Mutator::set_stack_range` — §11); the
coordinator combines all active mutators' ranges into the slice passed
to `pin_pointers_in_ranges`. Per NCL's thread model, each
`(make-thread)` Lisp thread has its own range + its own push/pop root
stack. The pin pass itself is unchanged.

**Two things conservative scanning forces the design to keep that
a pure-precise build would not:**
1. **Stack-range publication is required** wherever `conservative-pin`
   is on. Each mutator must publish accurate `[lo, hi)` stack bounds
   before reaching a safepoint, or its conservative roots are missed.
   NCL's scan is **stack-ranges only** — it does not walk arbitrary
   Rust-heap `Vec<Value>`/`HashMap` containers; those reach the GC only
   if a `Word` is stack-resident or static-rooted at scan time.
2. **Conservative *over-retention* is a known risk, but unquantified on
   HEAD.** An old NCL doc reported a ~95,000× `life.lisp` inflation and
   blamed Rust-heap container scanning — but that hypothesis doesn't
   match NCL's implemented scan (stack-ranges + `push_root` + cards), and
   the figure predates precise roots. **It must be re-measured** before
   it informs any design; NCL has no per-root-source attribution
   (their "Path R") implemented yet. The multi-mutator design neither
   causes nor fixes retention; this is flagged only so the §1 pause
   budget isn't read as proven for conservative builds.

### 5.4 Explicit object pinning for FFI (core API, both frontends) — **IMPLEMENTED (MM-0)**

> **Status:** landed as sprint MM-0. `PageHeap::pin(Word) -> PinHandle` /
> `unpin(PinHandle)` with a persistent refcounted `explicit_pins` map,
> folded into each evacuation via `apply_explicit_pins`. Reuses the
> existing pin bits + in-place page-flip. Tests in `tests/pin_api.rs`
> pass under both `conservative-pin` on and off. The design below
> describes the shipped shape (single-mutator; migrates into
> `SharedHeap` at MM-2).


Distinct from the conservative *stack* pin (§5.3): an explicit,
client-driven **"this object must not move until I say so"**, for
objects whose address escapes into foreign code. Confirmed required by
both frontends (Win32). The conservative scan pins FFI objects only
*incidentally* (their address is on the native stack); a precise-primary
build dials the scan back, so that incidental pin disappears and an
explicit API is needed. NCL's Win32 **callback** path holds Lisp
closures for the **process lifetime**, so copy-to-native-buffer is not
an option — the object itself must be pinned in place.

**API (client-facing, on `Mutator` or `GcCoordinator`):**

```rust
#[must_use]
pub struct PinHandle(usize /* global cell index */, /* generation tag */);

impl<L> Mutator<L> {
    /// Pin `w`'s target so the collector never moves it until `unpin`.
    /// Valid across any number of GC cycles. Idempotent per object via
    /// an internal refcount (N pins need N unpins). Cheap: one insert
    /// into the shared explicit-pin set.
    pub fn pin(&self, w: Word) -> PinHandle;
    /// Release one pin. After the last unpin, the object is movable by
    /// the next cycle (and may then promote/evacuate normally).
    pub fn unpin(&self, h: PinHandle);
}
```

**Mechanism — reuses machinery that already exists.** A pinned cell is
treated exactly like a conservatively-pinned or large-object cell:
- **Phase 1** skips it (not copied; no forwarding marker written).
- **Phase 3** flips its *page* in place to the destination generation
  rather than releasing it (the large-object run-flip path,
  `evac.rs` phase3, already does this for `desc.has_pins()`).
- Consequence: a pinned object **never moves and never promotes** while
  pinned; its whole page stays resident. Fine for FFI buffers (few,
  transient) and process-lifetime callbacks (few, intentional). A
  warning worth documenting: pinning many small short-lived objects
  wastes pages (one live cell keeps a page) — same cost model as any
  pin.

**Where the pin set lives + thread-safety.** A new
`explicit_pins: Mutex<HashMap<usize, u32>>` (cell index → refcount) in
`SharedHeap` (or a sharded variant). `pin`/`unpin` run on a mutator
thread **in `InDylan` state**; the collector reads `explicit_pins` only
at STW, *after* that mutator has reached the safepoint. So the ordering
is the same as roots/TLAB publication: a pin established before the
mutator's next safepoint is honored by that cycle (Release on the
`explicit_pins` insert, Acquire when the collector reads after the park
— or simply: it's behind a `Mutex`, and the collector takes it at STW).
The collector **unions `explicit_pins` into the pin set** before Phase 1,
alongside the conservative `pinned_cells`.

**The guarantee, stated for the FFI caller:** *from the instant `pin(w)`
returns until the matching `unpin`, `w`'s target keeps its address
across every intervening GC cycle.* That is exactly what
`SetWindowTextW(hwnd, buf)` and a process-lifetime `win_callback`
closure need. The caller pins before handing the address to the OS and
unpins when the OS is done (or never, for lifetime-of-process
callbacks).

**Build-independence.** This API is **not** gated on `conservative-pin`
— it's needed by precise-primary builds (Dylan, NCL-after-dial-back)
precisely *because* they don't get the incidental stack pin. It composes
with both root models.

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

Per §2.0 the fields are partitioned into **`SharedHeap`** (lock-free,
`Arc`-shared with every mutator) and **`PageHeap`** (monolith behind
`Mutex<PageHeap>`, exclusive at refill and STW). The "Lands in" column
says which.

| Field | Lands in | Concurrency | Notes |
|---|---|---|---|
| `_phantom`, `storage`/`base`, `n_pages` | SharedHeap | plain (immutable) | set once at construction |
| `committed_bits: Vec<AtomicU64>` | PageHeap | atomic | touched only at commit/refill/STW; stays in monolith |
| `committed_count: AtomicUsize` | PageHeap | atomic | unchanged |
| `commit_lock: Mutex<()>` | PageHeap | mutex | subsumed by the outer `Mutex<PageHeap>`; may be removed |
| `descs: Vec<PageDesc>` | PageHeap | plain | written at refill (under `Mutex<PageHeap>`) and STW; `words_used` reconciled by coordinator from published cursors (§3.3) |
| `alloc_regions` | PageHeap | plain | TLAB-refill source; touched only under `Mutex<PageHeap>` |
| `mark_bits`, `pinned_cells`, `recycle_live_counts*`, `last_*` | PageHeap | plain | STW-only |
| `minors_since_g0_promote`, `g0_promotes_since_g1_promote` | PageHeap | plain | STW-only |
| `young_page_cap` | PageHeap | plain | read under `Mutex<PageHeap>` at refill |
| `auto_gc_trigger_bytes`, `gc_budget_min_bytes`, `tenured_full_threshold_bps` | PageHeap | plain | written at end of cycle (STW) |
| `start_bits: Box<[AtomicU64]>` | **SharedHeap** | atomic | was `Arc<[AtomicU64]>`; mutators set via `fetch_or(Relaxed)` on the fast path |
| `cards: CardTable` | **SharedHeap** | atomic interior | was `Arc<CardTable>`; `mark_card_at` is `Relaxed` store, concurrent-safe |
| `bytes_alloc_since_gc` | **SharedHeap** | **AtomicUsize** | `fetch_add(Relaxed)` on bump, `load(Relaxed)` in `should_collect`; cross-cycle drift is acceptable (heuristic). See §11 for the per-mutator-counter alternative if this ping-pongs. |
| `poisoned` | **SharedHeap** | **AtomicBool** | `load(Acquire)` at mutator check sites; `store(Release)` from the STW collector exit (sole writer) |

### `SharedHeap`-only new fields

| Field | Type | Ordering |
|---|---|---|
| `safepoint: Safepoint` | `epoch: AtomicU64` + `world_running: AtomicU8` + `Mutex<()>`/`Condvar`; **no `parked_count`** (v4, B-1) | |
| `gc_epoch: AtomicU64` | `Acquire` snapshot before STW, `Relaxed` compare inside STW, `Release` bump after a real cycle (§2.5) | |
| `mutators: RwLock<Vec<Option<Arc<MutatorInner>>>>` | write lock briefly at register/deregister; read-lock snapshot for the per-mutator wait at STW (§4.4) | |
| `mutator_count: AtomicUsize` | diagnostics only in v4 — the wait loop iterates the registered set and checks each `last_epoch`/`is_active`, not a count | |
| `explicit_pins: Mutex<HashMap<usize,u32>>` | cell-index → refcount for FFI pins (§5.4); written by mutators in `InDylan`, read by the collector at STW (after the pinner has parked); unioned into the pin set before Phase 1 | |

`MutatorInner` (v4/v5) carries the per-mutator STW state: `last_epoch:
AtomicU64` (Release by mutator, Acquire by coordinator), `is_active:
AtomicBool` (false on Drop), `is_acting_coordinator: AtomicBool` (driver
skip), `state: AtomicU8` (InDylan/InNative, §4.6), plus the
`roots_snapshot` / `published_tlabs` mutexes and (conservative builds)
`parked_stack_range`.

### `GcCoordinator` / `Mutator` shared field

| Field | Type | Ordering |
|---|---|---|
| `coord_mutex: Arc<Mutex<()>>` | std mutex; serializes STW drivers AND registration (§2.2) | held by `Mutator::with_world_stopped` from entry to after `notify_all` + clearing `is_acting_coordinator`; makes the `gc_epoch` dedup correct and keeps cycles strictly sequential |

### `PageDesc`

`PageDesc` stays plain `#[repr(C)]`. Mutators write to a `PageDesc`
only during TLAB refill (under `Mutex<PageHeap>`); the coordinator
writes only at STW. No atomicity needed. If concurrent GC is ever
added, `PageDesc::generation` and `pin_byte` are the candidates for
atomic conversion — out of scope here.

### `AllocRegion`

Lives inside `PageHeap::alloc_regions`. Touched only under
`Mutex<PageHeap>` (refill) or at STW. Stays plain. Its role shifts from
"the mutator's bump cursor" to "the source of TLAB refills" — the TLAB
is the mutator's bump cursor now.

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

Five chunks. Each is mergeable on its own and leaves the heap usable
for the previous shape's clients — **with one hard ordering constraint
(red-team A-2): the safepoint protocol and per-mutator roots must land
together (Phase 3), because a stop-the-world collection with more than
one live mutator is unsound without every mutator's roots.** Phases 1–2
keep the single-mutator GC entry, so they don't hit this; multi-mutator
*GC* is only sound from Phase 3 on. Phases 4–5 are independent again.

> **Adoption is reconciliation, not drop-in (NCL feedback).** A frontend
> that has already hand-rolled its own parking protocol (NCL's
> `mutator.rs` has cooperative parking, per-thread stack ranges, and a
> multi-handle precise-root walk *today*) adopts this design by
> *reconciling* its parking with the core's, not by dropping a release
> in. The core ships the protocol + the poll-word + the pin API; the
> frontend still (a) emits the back-edge polls its JIT currently lacks,
> and (b) guarantees per-poll root consistency (§4.2). Budget the merge
> cost. **The FFI `pin`/`unpin` API (§5.4) is independent of the
> multi-mutator phases** — it's needed by precise-primary single-mutator
> builds too, so it can land first, on its own.

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

### Phase 2 — `SharedHeap` split + real per-mutator TLABs (~950 lines)

**What it adds:**
- **`SharedHeap` extraction (§2.0) — the first step, ~150 lines.** Move
  `poisoned`, `bytes_alloc_since_gc`, `start_bits`, `cards` into an
  `Arc<SharedHeap>`; `PageHeap` gains `shared: Arc<SharedHeap>` and
  redirects those field accesses. This is the prerequisite for a
  lock-free fast path (and for Phase 3's sound `&mut PageHeap`). The
  existing collector code is untouched — it reaches the moved fields
  through `self.shared`.
- `Tlab` struct, `tlabs: [[Tlab; 2]; 3]` field on `Mutator<L>`.
- Refill protocol per §3.3, using existing `try_alloc_g0_slab` and a
  new boxed mirror, behind `Mutex<PageHeap>`.
- Fast-path bump in `Mutator::try_alloc_cons_in` / `try_alloc_boxed_in`
  — no lock when the TLAB has room; touches only `SharedHeap` atomics.
- TLAB cursor publish/clear is stubbed (no safepoint yet); drop
  abandons tails per §2.1.

**What it leaves unchanged:**
- `PageHeap`'s public `&mut self` collector API (`collect_*`, `evac`).
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
- `tlab_drop_abandons_tail_safely`: allocate 1000 cells via TLAB, drop
  the mutator (no GC running), verify (a) no panic / no deadlock, (b)
  the allocated objects are still walkable, (c) a subsequent GC with no
  roots reclaims the page. (Drop does NOT reconcile `words_used` — §2.1.
  The over-stated `words_used` is harmless: cons cells carry start bits,
  so the start-bit-driven walkers skip the start-bit-less abandoned tail;
  the next evacuation rebuilds `words_used` exactly on the dest page.)
- `start_bits_set_correctly_under_concurrent_alloc`: assert every
  allocated cell has its start bit set (and only those).

### Phase 3 — Safepoint + parking + per-mutator snapshot roots (~900 lines)

**Roots are folded into this phase (red-team A-2).** v3 staged "safepoint
mechanism" in Phase 3 and "per-mutator roots" in Phase 4 and claimed both
were independently mergeable. They are **not**: a real STW collection
with >1 live mutator that can only see one external closure's roots will
reclaim the other mutators' live objects — a soundness hole. So Phase 3
ships the safepoint protocol *and* per-mutator snapshot roots together;
with them, multi-mutator GC is sound.

**What it adds:**
- `Safepoint` struct (in `SharedHeap`; §4.1), `is_active` /
  `is_acting_coordinator` / `last_epoch` on `MutatorInner`.
- `Mutator::poll_safepoint`, `Mutator::park` (cold), `publish_roots`,
  `with_world_stopped`, and `Mutator::try_collect_*` (the driver entry,
  §2.5) with the `gc_epoch` dedup.
- `publish_and_clear_tlabs` + coordinator-side `reconcile_published_tlabs`
  (replaces the Phase-2 stub).
- Per-mutator snapshot roots: coordinator gathers every active mutator's
  `roots_snapshot` (plus the driver's own + the caller's extra closure)
  and feeds them to the evacuator, which updates them in place.

**What it leaves unchanged:**
- Mutator fast-path bump.
- `RootsSource` is Snapshot-only (§5); no `Walker`.

**Tests:**
- `safepoint_parks_all_mutators`: 4 mutators each polling per iteration;
  a 5th driver mutator triggers a cycle; assert every other mutator's
  `last_epoch == target` (not a global counter) before the closure runs;
  assert all resume.
- `driver_does_not_wait_on_itself` (B-2): a single registered mutator
  triggers `try_collect_minor`; assert it completes (does not deadlock
  waiting for its own park).
- `concurrent_alloc_plus_gc_no_torn`: 4 mutators alloc + one of them
  drives `try_collect_minor` 10× over a few seconds; assert no torn
  cells, no double-frees, every published root survives and is forwarded.
- `safepoint_with_explicit_polls`: a mutator with no allocs in its hot
  loop still reaches the safepoint via `poll_safepoint`.
- `lagging_mutator_times_out`: a mutator that never polls/allocs makes
  the driver hit the 10s timeout; assert the diagnostic names the stuck
  mutator id (short timeout via the builder).
- `mutator_drop_during_cycle_unblocks_driver` (B-1): driver waiting on
  mutator B; B's thread panics/unwinds → `Drop` sets `is_active=false`
  + `notify_all`; assert the driver's wait drops B and the cycle
  completes. The inverse hang is the guarded bug.
- `concurrent_registration_serializes_with_stw` (A-1/B-3): spawn a
  thread that calls `register_mutator` repeatedly while another drives
  cycles; assert no newcomer ever allocates during a world-stop (e.g.
  instrument with a "world stopped" flag the test checks at alloc).
- `two_mutators_roots_independent`: A and B each publish one cons
  pointer; a cycle runs; both are forwarded; no cross-mutator interference.
- `safepoint_preserves_poison`: poison from inside a cycle; assert
  subsequent `mutator.try_alloc_*` from any mutator returns None.

### Phase 4 — Conservative pins + precise-roots-only mode (~400 lines)

(Per-mutator *snapshot* roots moved to Phase 3.) This phase adds the
conservative-pin path for clients that don't emit precise roots.

**What it adds:**
- Each mutator publishes its stack range (`parked_stack_range` on
  `MutatorInner`, set via `Mutator::set_stack_range`; §11 risk #7) at the
  safepoint; the coordinator combines all ranges into the slice passed
  to `pin_pointers_in_ranges`.
- `--features=precise-roots-only` to compile the conservative pin scan
  out entirely (see §9 D-CONSERVATIVE-PIN).

**Tests:**
- `conservative_pins_combine_across_mutators`: two mutators each with a
  fake stack range holding a pointer; assert both targets are pinned in
  one cycle.
- `precise_roots_only_mode`: with `--features=precise-roots-only`,
  assert no conservative pin scan runs and snapshot roots alone keep
  objects alive.

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

### D-TLAB-SIZE — **RESOLVED: dynamic 4 KB→64 KB (both frontends)**

Both frontends report long-lived threads + moderate/bursty allocation
(no short-thread-storm), so dynamic growth has no idle-tail waste risk.
Confirmed: dynamic 4 KB → 64 KB, double-every-refill. Lean to the large
end for heavy allocators (Dylan compile worker; NCL cons churn). The
cons-page fast path is the one that matters most (NCL is cons-dominated).

### D-SAFEPOINT-FREQ — **RESOLVED: cooperative polls (already emitted)**

Dylan emits `nod_safepoint_poll()` at function entry + every loop
back-edge (Sprint 45e) — option (c). NCL today checks `stop_requested`
only at allocation sites (≈ option (a), coarser); for multi-mutator it
will add back-edge polls so a tight no-alloc Lisp loop (common!) can't
stall STW. Neither frontend needs signal-based suspension. **Residual
work is on the frontends, not the GC core:** the core just exposes
`Mutator::poll_safepoint`; where polls land is the frontend's codegen
choice. The native-call boundary (§4.6) covers the no-poll-while-blocked
case for both.

### D-GC-API-SHAPE — `&mut self` collector — **RESOLVED (§2.0, §2.3)**

Resolved in v3: collector stays `&mut PageHeap`, reached via
`heap.lock()` on the coordinator's `Mutex<PageHeap>`, sound under §2.0.
The `&self`-with-full-interior-mutability alternative is rejected (would
atomicify every collector field). No longer an open decision.

### D-ROOTS-SHAPE — **RESOLVED: Snapshot only (§5; red-team A-4/B-5)**

`Walker` was cut (write-back is fiction for a moving collector). v4
ships `Snapshot` only: the client publishes a `Vec<Word>`, the
collector updates it in place, the client copies back. No longer an
open decision.

### D-CONSERVATIVE-PIN — **RESOLVED: KEEP, first-class, feature-gated**

Settled by the mixed ecosystem: **NCL requires conservative pinning
today** (its JIT spills mixed tagged/native words with no separation),
so dropping it is not an option. **Dylan disables it** (`--no-default-
features`) because it emits precise roots. Decision: keep
`pin_pointers_in_ranges` as a first-class page-heap path behind the
`conservative-pin` feature; the multi-mutator extension combines every
active mutator's published stack range (§5.3). This corrects an interim
v5 draft that proposed demoting it to "legacy semispace only" — that was
wrong; conservative pin is on the *page-heap* and is NCL's primary path.

### D-MUTEX-IMPL — `std::sync::Mutex` or `parking_lot`?

- **Options:**
  - (a) `std::sync::Mutex` — no extra deps.
  - (b) `parking_lot::Mutex` — faster, no poisoning, smaller.
- **Tentative:** (a) for now (fewer deps); profile later.
- **Decision needed:** confirm. (a) means accepting Rust's lock poisoning
  semantics on `alloc_mutex` (which we'd never want to recover from
  anyway, so the unwrap pattern is fine).

### D-GC-FROM-MUTATOR — **RESOLVED: yes, mutator-driven (§2.5, §4.4; red-team B-2)**

A mutator drives GC by calling `Mutator::try_collect_*`, which
self-publishes its TLABs+roots, sets `is_acting_coordinator`, and parks
every *other* active mutator. This is the primary path (matches NCL's
`trigger_minor_gc`). The deadlock B-2 flagged (driver waiting for its
own park) is resolved by the `is_acting_coordinator` skip. A dedicated
GC thread is still possible (it registers its own driver handle).
Concurrent triggers are coalesced via `gc_epoch` (§2.5). No longer open.

### D-POISON-CHECK — Where do we check `poisoned`?

- **Options:**
  - (a) Once at TLAB refill. Mutator can allocate from a stale TLAB for
    one TLAB's worth after poison — but TLABs are small.
  - (b) On every fast-path alloc (`Acquire` load).
- **Tentative:** (a). Mid-evac OOM is rare; one TLAB of "phantom"
  allocations after poison is acceptable.
- **Decision needed:** confirm (a) is acceptable. If we want strict
  immediate-stop semantics, go (b).

### D-WIN-VS-LINUX-TLS — **RESOLVED: core TLS-free; frontends own TLS (§2.6)**

Confirmed by both frontends. The core keeps `Mutator<L>` explicit and
TLS-free (portable). Each frontend owns a `thread_local!` `Cell<*mut
Mutator>` with a single-TLS-load fast fetch in its alloc shim (§2.6).
Windows-first; Rust `thread_local!` works on both Windows and Linux.
No platform-specific surface in the core.

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

### D-RWLOCK-VS-UNSAFECELL — **RESOLVED / REMOVED (§2.0, §2.3)**

Superseded by the §2.0 split: the monolith lives behind a plain
`Mutex<PageHeap>` (not an `RwLock`, and not an `UnsafeCell`). Both v1
options are retired. No longer an open decision.

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

1. **TLAB cursor publication vs. coordinator read.** A mutator must
   publish its `(page_idx, start, bump)` cursors *before* the
   coordinator reconciles `words_used`. **Mitigation (v4):** the mutator
   does `publish_and_clear_tlabs()` then `last_epoch.store(target,
   Release)`; the coordinator's per-mutator wait predicate does
   `last_epoch.load(Acquire) >= target`, establishing happens-before
   before it calls `reconcile_published_tlabs` (§4.4). No global counter
   is involved (that was the v3 `parked_count`, removed per B-1).
   **Risk if wrong:** incorrect `tenured_used_bytes`, false GC triggers.

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
   `Drop` of the last `Arc<SharedHeap>` / `Arc<Mutex<PageHeap>>`... wait,
   it can't, because the mutator holds clones of both `Arc`s. (Was
   `Arc<PageHeap>` pre-split; same lifetime conclusion.) But if a
   mutator is held inside a thread that the test infra never joins, the
   heap won't drop. Test discipline: every spawned test thread must join
   before the heap is dropped. **Risk:** flaky tests on CI if a thread
   panics between alloc and join.

6. **Drop while a cycle runs (RESOLVED, §2.1; v4 B-1).** If a mutator
   thread panics and unwinds while a cycle runs on another thread,
   `Drop` must not touch `descs` / the heap lock (the driver holds it)
   and must not leave the driver waiting for a thread that will never
   park. Resolution: `Drop` (a) sets `is_active.store(false, Release)`
   and deregisters, (b) `park_cv.notify_all()`, (c) abandons TLAB tails
   as free space — never takes `Mutex<PageHeap>`. The driver's wait
   predicate (§4.4) tests `is_active`, so it drops a departed mutator
   from the wait set. A mutator cannot be dropped *while itself parked*
   (park borrows `&mut self`; `Mutator` is `!Send`). **Residual risk:**
   if a thread is `kill`ed (not unwound) so `Drop` never runs, the
   driver hits the 10s-timeout diagnostic path — same class as a no-poll
   hot loop (#3).

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

2. **Safepoint protocol under `loom`.** Model: mutator
   `publish_and_clear_tlabs` + `last_epoch.store(Release)` → driver
   `last_epoch.load(Acquire)` → `reconcile_published_tlabs`. Confirm the
   per-mutator `last_epoch` Release/Acquire is sufficient happens-before
   and that the `is_active`/`is_acting_coordinator` predicate has no
   missed-wakeup. Should be true; verify.

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

## Appendix B — Frontend coverage audit

Does the core design account for what each frontend needs *from the
core*? (Frontend-side work — fork-sync, ncl-llvm polls, Life re-measure
— is the frontends' own, deliberately out of scope here.)

| Frontend need (from the core) | NCL | Dylan | Covered by |
|---|---|---|---|
| Precise `Snapshot` roots, in-place update + copy-back | ✅ today | ✅ today | §5.1 |
| Conservative stack-range pin, feature-gated | ✅ (primary-ish, belt + FFI) | off | §5.3, `conservative-pin` |
| Per-thread stack-range publication | ✅ needed | n/a | §5.3, `set_stack_range` |
| Poll-word protocol that parks non-allocating threads | ✅ **the key gap** | ✅ | §4.1–4.4 |
| Poll-site root-consistency contract | ✅ | ✅ | §4.2 (frontend obligation, stated) |
| `InNative` state for blocking foreign calls | ✅ (Win32) | ✅ (GetMessageW) | §4.6 |
| `InNative`/normal handling for unwind cleanup | ✅ (option a) | ✅ | §4.6 |
| Explicit FFI `pin`/`unpin`, process-lifetime | ✅ (Win32 callbacks) | ✅ (Win32) | §5.4 |
| Per-thread mutator handle + thread-local fetch | ✅ `(make-thread)` | ✅ | §2.6 |
| Lock-free bump; refill via central mutex | ✅ (has it) | ✅ | §2.4, §3.3 |
| Recoverable `try_collect` + auto-major trigger | needs fork-sync* | n/a | already in `newgc-core` HEAD (space.rs) |
| Cons-dominated fast path, 2-cell, never split | ✅ critical | secondary | §1, cons pages |
| Shared mutable object graph, single-digit threads | ✅ | ✅ | one central lock; sharding YAGNI (§1) |

\* The auto-major / recoverable-OOM pieces **already exist in the core**;
"needs fork-sync" is purely NCL pulling them into its vendored copy —
not a core design gap.

**Verdict:** every core-side need of both frontends is accounted for in
the design. The only un-built core item is the FFI `pin`/`unpin` API
(§5.4) — designed here, independent of the multi-mutator phases, and the
natural first thing to implement. Everything else is either already in
`newgc-core` HEAD or specified in this doc awaiting Phase 1+.

---

## Appendix A — Estimated touchpoints

**Terminology:** throughout §3–§4 *alloc_mutex* names the central-
allocation lock. Per §2.0 / §7 (v3) there is **no separate `Mutex<()>`**
— that role is the coordinator's `Mutex<PageHeap>`. Read "take
alloc_mutex" as "take `heap.lock()`."

| File | Changes |
|---|---|
| `src/page_heap/shared.rs` (new) | `SharedHeap<L>` — lock-free fields extracted from `PageHeap` (`poisoned: AtomicBool`, `bytes_alloc_since_gc: AtomicUsize`, `start_bits`, `cards`, `safepoint`, `gc_epoch`, `mutators`, `mutator_count`). `mark_card_at` moves here. (~200 lines.) |
| `src/page_heap/space.rs` | `PageHeap` gains `shared: Arc<SharedHeap>`; the moved fields are deleted and their accessors redirect through `self.shared`. `poisoned`/`bytes_alloc_since_gc` reads/writes become atomic ops on `shared`. (~150 lines, mostly mechanical.) |
| `src/page_heap/mutator.rs` (new) | `Mutator<L>`, `Tlab`, `MutatorId`, `MutatorInner`, `RootsSource`. Holds `Arc<SharedHeap>` + `Arc<Mutex<PageHeap>>`. (~600 lines including docs.) |
| `src/page_heap/safepoint.rs` (new) | `Safepoint` struct + park/unpark + publish-cursors protocol. (~250 lines.) |
| `src/page_heap/coordinator.rs` (new) | `GcCoordinator { shared, heap: Mutex<PageHeap>, coord_mutex }`, `with_world_stopped`, `reconcile_published_tlabs`, `gc_epoch` dedup wrappers. (~350 lines.) |
| `src/page_heap/alloc.rs` | `&mut self` API stays; `try_alloc_in_region` called by the refill path. Poison/`bytes_alloc_since_gc` gates redirect to `self.shared`. (~30 lines.) |
| `src/page_heap/coordinator_api.rs` | Reuse existing slab primitives for refill. Minor signature work. (~50 lines.) |
| `src/page_heap/cycle.rs`, `evac.rs`, `mark.rs` | **Unchanged** — still `&mut PageHeap`. Reach `start_bits`/`cards`/`poisoned` via `self.shared` (accessor redirect only). |
| `src/lib.rs` / `src/page_heap/mod.rs` | Export `Mutator`, `GcCoordinator`, `MutatorId`, `RootsSource`, `SharedHeap`. (~12 lines.) |
| `tests/multi_mutator.rs` (new) | New test file for Phases 2–5 tests. (~600 lines across phases.) |
| `tests/threading.rs` | Unchanged (Phase 1 keeps the `Arc<Mutex<PageHeap>>` shape these tests assume). |

Total estimated diff: 2200–2700 lines across the 5 phases (the
`SharedHeap` split adds ~200 over the v1 estimate but removes the
collector-rewrite risk the red-team's literal split would have carried).
````

### `END DESIGN DOC`

---

## Summary

**Recommended phasing:** Phase 1 introduces a `Mutator<L>` handle backed by an internal `Mutex<PageHeap>` (small, ~700 lines, unblocks the new API surface without touching the alloc fast path). Phase 2 adds real per-(gen, kind) TLABs with a dedicated `alloc_mutex` and dynamic 4KB → 64KB sizing — this is where concurrent fast-path bump lands. Phase 3 introduces the safepoint protocol (per-mutator epoch + global condvar) and a `GcCoordinator::with_world_stopped` that keeps `PageHeap`'s collector API as `&mut self`. Phase 4 adds per-mutator root enumeration via a `RootsSource::{Walker, Snapshot}` enum and combined conservative-pin slices. Phase 5 hardens (timeouts, telemetry, stress tests).

**Most consequential open decisions for you:** (1) **D-GC-API-SHAPE** — confirm the collector keeps `&mut self` and uses `with_world_stopped` rather than going fully `&self`-with-atomics; this is the largest API direction call. (2) **D-SAFEPOINT-FREQ** — confirm "safepoint check at TLAB refill + explicit `poll_safepoint` calls the client emits," which trades a fast-path cost (~1-2 ns per alloc avoided) against a parking-latency worst case (one TLAB of allocs ≈ 64 KB). (3) **D-ROOTS-SHAPE** — pick whether `Walker` or `Snapshot` is the default, or force the client to pick at registration; this shapes the binding contract NCL will write against.

**Risks I couldn't resolve in the doc:** (a) Card-barrier and start-bit cacheline contention under heavy concurrent allocation on adjacent cells — design is correct but performance may need profiling-driven mitigation that I don't have a clean answer for ahead of measurement. (b) Drop semantics of a `Mutator` while the heap is poisoned — currently I say "retire as normal" but that touches `descs` on an indeterminate heap; might need to bail without reconciling, but I don't have enough context on whether the slight `words_used` inconsistency matters versus the risk of touching an unsafe state. (c) Stack-range publication for conservative pins requires the client to call `set_stack_range` correctly; if the client forgets, conservative pinning is silently empty — there's no clean way to enforce this in the type system that I could see.
