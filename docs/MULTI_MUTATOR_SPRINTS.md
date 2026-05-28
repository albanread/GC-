# Multi-Mutator + FFI-Pin — Sprint Plan

**Companion to `MULTI_MUTATOR_DESIGN.md` (v6).** That doc is the *what/why*;
this is the *delivery sequence*. Each sprint below is independently
mergeable, has named acceptance tests, a rough size, explicit
dependencies, and a back-reference to the design section it implements.
All work is in `newgc-core` (this repo) — NCL/Dylan frontend work
(fork-sync, ncl-llvm back-edge polls, Life re-measure) is the frontends'
own backlog and is out of scope here.

## Readiness / gating

- **MM-0 (FFI pin API) is ready to pull now.** It is self-contained,
  needs no multi-mutator machinery, runs on the current single-mutator
  heap, and unblocks both frontends dialing back conservative scan.
- **MM-1 … MM-8 (multi-mutator) are gated on actual need.** Per the
  review, neither frontend needs concurrent-allocation throughput yet
  (single-digit threads, small live sets, STW acceptable). Build these
  when concurrent allocation becomes a measured requirement — not before.
- **Hard ordering constraint (design A-2):** MM-4 and MM-5 must ship
  *together* before multi-mutator GC is enabled — a STW collection with
  >1 live mutator is unsound without every mutator's roots.

## Dependency graph

```
MM-0 (FFI pin)  ─────────────┐ (independent; do now)
                             ▼
MM-1 (Mutator handle) ─► MM-2 (SharedHeap split) ─► MM-3 (TLABs)
                                                       │
                                                       ▼
                              MM-4 (safepoint/park) ─► MM-5 (per-mutator roots)
                                                       │   [MM-4+MM-5 = sound MT-GC]
                                                       ├─► MM-6 (InNative)  [needs MM-0]
                                                       ├─► MM-7 (conservative MT + precise-only)
                                                       └─► MM-8 (hardening/loom/stress)
```

Total ≈ 30 engineer-days for the full sequence (MM-0 ≈ 3; multi-mutator ≈ 27).

---

## MM-0 — Explicit FFI `pin` / `unpin` API  *(≈3 days, no deps, DO NOW)*

**Design:** §5.4. **Why first:** smallest, self-contained, single-mutator-
compatible, unblocks both frontends; the others can wait.

**Deliverables**
- `PageHeap::pin(w: Word) -> PinHandle` and `unpin(PinHandle)`.
- Refcounted pin set (single-mutator: a `HashMap<usize,u32>` field on
  `PageHeap`; migrates into `SharedHeap` in MM-2).
- Union the explicit-pin set into the pin set at the start of each
  evacuation (alongside `pinned_cells`); reuse the existing
  `desc.has_pins()` in-place page-flip (the large-object path already
  flips a pinned run rather than evacuating it).
- `#[must_use] PinHandle` so a dropped-without-unpin handle is caught.

**Acceptance tests** (new `tests/pin_api.rs`)
- `pinned_object_keeps_address_across_minor`: pin a cons; run several
  `collect_minor`; address unchanged; value intact.
- `pinned_object_survives_full_collect`: pin a boxed object; `collect_full`;
  address unchanged even though Tenured is compacted.
- `unpin_lets_object_move`: pin → collect (stays) → unpin → collect →
  address may change / object reclaimed if unrooted.
- `pin_refcount`: two `pin`s of the same object need two `unpin`s before
  it becomes movable.
- `pinned_g0_object_does_not_promote`: pinned G0 object stays G0 across
  promotion-threshold cycles (its page flips in place).
- `pin_large_object`: pinning a multi-page large object keeps the whole
  run fixed; neighbors unaffected.

**Done when:** all of the above pass; existing 250 tests still green; a
pinned object is provably immovable from `pin()` to `unpin()` across any
cycle kind.

---

## MM-1 — `Mutator<L>` handle, serialized  *(≈3 days, no deps)*

**Design:** §2.1–2.3, Phase 1. **Goal:** introduce the handle API shape
with **no perf change** — `Arc<Mutex<PageHeap>>` inside; `Mutator`
delegates `try_alloc_*`/`mark_card_at` to the locked heap. Multiple
handles coexist (allocation serialized by the mutex). No safepoints, no
TLABs yet.

**Deliverables**
- `Mutator<L>`, `MutatorId`, `register_mutator(&self) -> Mutator`,
  `GcCoordinator` factory holding `Arc<Mutex<PageHeap>>` + `coord_mutex`.
- `Drop` deregisters the slot (no heap-state touch).

**Acceptance tests** (extend `tests/threading.rs`)
- All 7 existing threading tests still pass.
- `two_mutators_share_one_heap_via_handle`: 2 threads, N allocs each,
  total correct, GC reclaims with no roots.
- `mutator_alloc_returns_none_when_poisoned`.
- `mutator_drop_releases_slot`.

**Done when:** the handle API exists and is `Send`-correct; the old
`Mutex<PageHeap>` usage pattern in tests can migrate to it.

---

## MM-2 — `SharedHeap` extraction (refactor)  *(≈2 days; deps: MM-0, MM-1)*

**Design:** §2.0. **Goal:** pure refactor, **zero behavior change**. Move
the lock-free-read fields into `Arc<SharedHeap>`: `poisoned: AtomicBool`,
`bytes_alloc_since_gc: AtomicUsize`, `start_bits`, `cards`,
`explicit_pins` (from MM-0), plus a placeholder `safepoint`. `PageHeap`
gains `shared: Arc<SharedHeap>` and redirects those accessors. `Mutator`
holds `Arc<SharedHeap>` + `Arc<Mutex<PageHeap>>` — never a bare
`&PageHeap` (the soundness premise for the collector's `&mut PageHeap`).

**Acceptance tests**
- **All existing tests pass unchanged** (this is the bar for a refactor).
- `alloc_microbench` (criterion or a coarse timing test): no regression
  on single-thread cons alloc throughput.
- A compile-time `assert_send!/assert_sync!` on `SharedHeap`.

**Done when:** the field split is in, the collector code is untouched
(reaches moved fields via `self.shared`), and nothing regresses.

---

## MM-3 — Per-mutator TLABs (lock-free bump)  *(≈4 days; deps: MM-2)*

**Design:** §3. **Goal:** real per-`(gen,kind)` TLABs on `Mutator`;
lock-free bump fast path touching only `SharedHeap` atomics; refill takes
`Mutex<PageHeap>`. Dynamic 4 KB→64 KB growth (double-every-refill).
`young_page_cap` checked inside refill. Publish-cursors machinery present
but inert (no safepoint yet).

**Acceptance tests** (`tests/multi_mutator.rs`)
- `tlab_bump_no_heap_lock`: instrument the refill mutex with a counter;
  4096 cons allocs take the lock < 16 times.
- `concurrent_cons_alloc_no_torn_pointers`: 4 threads × 10k conses with
  payload `(i, hash(i))`; walk every G0 cons start, assert `cdr ==
  hash(car)`.
- `tlab_refill_respects_young_page_cap`.
- `start_bits_set_correctly_under_concurrent_alloc`.
- `tlab_drop_abandons_tail_safely` (per §2.1: drop doesn't reconcile;
  next evac rebuilds `words_used`; page reclaims when empty).

**Done when:** concurrent allocation is correct and the fast path takes
no lock.

---

## MM-4 — Safepoint protocol + cooperative parking  *(≈5 days; deps: MM-3)*

**Design:** §4.1–4.5. **Goal:** the parking machinery — `Safepoint`
(`epoch`, `world_running`, condvar), `Mutator::poll_safepoint`/`park`,
`Mutator::with_world_stopped` (driver self-parks, sets
`is_acting_coordinator`), per-mutator wait on `last_epoch`/`is_active`,
`gc_epoch` dedup, registration-serializes-with-STW.
**Multi-mutator GC is NOT yet sound here** (roots land in MM-5) — test
with ≤1 mutator + the driver.

**Acceptance tests**
- `driver_does_not_wait_on_itself` (B-2): single registered mutator
  triggers `try_collect_minor`, completes (no self-deadlock).
- `safepoint_with_explicit_polls`: a no-alloc loop still parks via poll.
- `lagging_mutator_times_out`: diagnostic names the stuck mutator.
- `concurrent_registration_serializes_with_stw` (A-1/B-3).
- `safepoint_preserves_poison`.

**Done when:** the world stops and resumes correctly with the driver and
at most one other (parked) mutator; ordering audited (§4.4).

---

## MM-5 — Per-mutator snapshot roots → sound multi-mutator GC  *(≈4 days; deps: MM-4)*

**Design:** §5.1, §4.4, A-2. **Goal:** `roots_snapshot` per mutator,
`publish_roots`, the coordinator gathers every active mutator's snapshot
(+ the driver's own + the caller closure) and the evacuator updates them
in place. **Ships together with MM-4** — only now is multi-mutator STW
sound.

**Acceptance tests**
- `two_mutators_roots_independent`: A and B each hold one cons; one cycle;
  both forwarded; no cross-interference.
- `concurrent_alloc_plus_gc_no_torn`: 4 mutators alloc + one drives
  `try_collect_minor` 10× over a few seconds; every published root
  survives and is forwarded; no torn cells / double-frees.
- `mutator_drop_during_cycle_unblocks_driver` (B-1): a mutator
  panics/unwinds mid-cycle; driver's wait drops it (`is_active=false` +
  `notify_all`); cycle completes.

**Done when:** N mutators can allocate while one drives GC, soundly.

---

## MM-6 — Native-call `InNative` convention  *(≈3 days; deps: MM-5, MM-0)*

**Design:** §4.6. **Goal:** `state: AtomicU8` (InDylan/InNative),
`enter_native`/`leave_native`, collector skips `InNative` in the wait
predicate, `leave_native` re-parks if the world is stopped, root+TLAB
publish on native entry.

**Acceptance tests**
- `innative_thread_skipped_by_collector`: a thread parked in `InNative`
  doesn't block a cycle driven by another mutator.
- `leave_native_reparks_during_cycle`: returning thread blocks on
  `world_running` before touching the heap.
- `ffi_object_pinned_across_native_call`: object passed to a simulated
  blocking call survives a GC during the call (ties MM-0 pin + MM-6).
- `unwind_cleanup_reaches_safepoint` (option a, §4.6).

**Done when:** a thread blocked in foreign code can't hold the collector
hostage, and objects it passed out are safe (via MM-0 pin).

---

## MM-7 — Conservative pins across mutators + precise-only feature  *(≈3 days; deps: MM-5)*

**Design:** §5.3. **Goal:** per-mutator `parked_stack_range` +
`set_stack_range`; the coordinator combines all active mutators' ranges
into one slice for `pin_pointers_in_ranges`. Add a `precise-roots-only`
build that compiles the conservative scan out.

**Acceptance tests**
- `conservative_pins_combine_across_mutators`: two mutators, each with a
  fake stack range holding a pointer; both targets pinned in one cycle.
- `precise_roots_only_mode`: with the feature, no conservative scan runs;
  snapshot roots alone keep objects alive.
- Both feature configs compile + pass.

**Done when:** conservative builds (NCL-shaped) work multi-mutator, and a
precise-only build drops the scan cleanly.

---

## MM-8 — Hardening  *(≈4 days; deps: MM-7)*

**Design:** §10–§11. **Goal:** stress + model-checking + ergonomics.

**Deliverables / tests**
- `stress_8_mutators_100k_iterations`: long torture run, alloc + GC +
  enter/leave native + pin/unpin, asserts no corruption.
- `loom_safepoint_protocol`: loom model of publish→`last_epoch`
  Release/Acquire→reconcile, and the `is_active`/`is_acting_coordinator`
  predicate — no deadlock, no missed wakeup, correct happens-before on
  `poisoned`/`gc_epoch`.
- `GcCoordinator` builder (timeout, `safepoint_per_alloc` debug flag,
  telemetry hooks).
- Update `THREADING.md` to describe the shipped state.

**Done when:** stress + loom green; docs reflect reality.

---

## Not in this plan (frontend backlog, tracked elsewhere)

These are the higher-leverage items for NCL *today*, but they live in
NCL's vendored fork, not `newgc-core`:
1. Fork-sync the **auto-major trigger + recoverable `try_collect`/poison**
   from `newgc-core` HEAD (NCL sessions currently never reclaim Tenured).
2. ncl-llvm: emit **back-edge safepoint polls** (with the §4.2 root-
   consistency guarantee) so non-allocating Lisp loops are parkable.
3. **Re-measure `life.lisp` retention** on HEAD with precise roots before
   trusting any old number.

Dylan's analogue: confirm its precise-root maps + polls satisfy the §4.2
contract; wire `enter_native`/`leave_native` (MM-6) around its Win32
message loop.
