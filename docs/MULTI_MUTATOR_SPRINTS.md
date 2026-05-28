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

## MM-0 — Explicit FFI `pin` / `unpin` API  *(≈3 days, no deps)* — ✅ DONE

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

## MM-1 — `Mutator<L>` handle, serialized  *(≈3 days, no deps)* — ✅ DONE

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

## MM-2 — `SharedHeap` extraction (refactor)  *(≈2 days; deps: MM-0, MM-1)* — ✅ DONE

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

## MM-3 — Per-mutator TLABs (lock-free bump)  *(≈4 days; deps: MM-2)* — ✅ DONE

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

## MM-4 — Safepoint protocol + cooperative parking  *(≈5 days; deps: MM-3)* — ✅ DONE

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

## MM-5 — Per-mutator snapshot roots → sound multi-mutator GC  *(≈4 days; deps: MM-4)* — ✅ DONE

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

**Shipped (MM-4+MM-5, one commit):** `Safepoint { epoch, world_running,
park_mutex, park_cv }`; `Mutator::poll_safepoint`/`park`;
driver self-park via `drive_collect` (`collect_minor`/`collect_full`)
with `is_acting_coordinator`; per-mutator `roots_snapshot` gathered +
forwarded in place; per-mutator `last_epoch`/`is_active` wait;
registration serialized with STW via `coord_mutex`; `ResumeGuard`
resume-on-every-exit. Tests in `tests/safepoint.rs`:
`driver_does_not_wait_on_itself` (B-2), `poll_safepoint_noop_when_no_gc`,
`dropped_mutator_not_waited_on` (B-1), and
`multi_worker_rooted_survival_under_concurrent_gc` (3 workers poll-loop
while a driver runs 25+ cycles; every per-mutator root survives and is
forwarded). **Concurrency fixes found here:** (1) `ResumeGuard` must
resume under `park_mutex` (lost-wakeup); (2) a *straggler* parked for
epoch `N` that the driver laps into `N+1` must **re-arm** `last_epoch` to
the live epoch (else frozen → permanent deadlock); (3) the driver
publishes the stop (epoch bump + `world_running=0`) under `park_mutex` to
prevent a torn stale-epoch/fresh-stop read; (4) `flush_tlabs` must not
reconcile per-page `words_used` (multi-TLAB-per-page high-water hazard).

---

## MM-6 — Native-call `InNative` convention  *(≈3 days; deps: MM-5, MM-0)* — ✅ DONE

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

**Shipped:** `MutatorInner.state: AtomicU8` (`IN_DYLAN`/`IN_NATIVE`);
`Mutator::enter_native(&[Word])` (publishes roots + flushes TLABs, then
announces `IN_NATIVE` under `park_mutex` + notify so a driver already
waiting on us drops us immediately rather than stalling 10 s);
`Mutator::leave_native(&mut [Word])` (blocks on `world_running` before
flipping to `IN_DYLAN`, re-adopts the current epoch, copies forwarded
roots back); the driver's §4.4 wait predicate skips `IN_NATIVE` while
its root-visit loop still forwards the native thread's published
snapshot. Tests in `tests/native_call.rs`:
`driver_does_not_stall_on_native_thread` (skip + survival + a <5 s bound
that a broken skip's 10 s timeout would blow),
`enter_leave_native_without_gc_roundtrips_roots` (no-GC fast path +
epoch resync → poll is a no-op), `ffi_object_pinned_across_native_call`
(MM-0 pin keeps a passed-out object's address fixed across cycles during
the call), `native_and_polling_workers_survive_concurrent_gc`
(integration: `IN_NATIVE` skip composes with normal park/poll). Note:
unwind-cleanup (§4.6 option a) needs no core machinery — cleanup code
reaches safepoints via the ordinary poll path, already covered.

---

## MM-7 — Conservative pins across mutators + precise-only feature  *(≈3 days; deps: MM-5)* — ✅ DONE

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

**Shipped:** per-mutator `stack_lo`/`stack_hi: AtomicUsize` on
`MutatorInner` (cfg-gated to `conservative-pin`, so a precise-only build
carries zero extra surface); `Mutator::set_stack_range(lo, hi)`. The
driver, under the world-stopped barrier with the heap locked, unions
every active mutator's `[lo, hi)` window (its own slot included via the
registry snapshot) and calls `pin_pointers_in_ranges` for the moving
generations (minor: G0+G1; full: G0+G1+Tenured) *before* the evac reads
the pin set — the pin pass itself is unchanged (§5.3). `precise-roots-only`
is `--no-default-features`: the union/scan and the stack-range fields
compile out, and the `pin_gens` argument is discarded. Tests in
`tests/conservative_mt.rs`: `conservative_pins_combine_across_mutators`
(two mutators each publish a window holding the *sole* reference to a
cons; one driver cycle unions both windows and both conses survive in
place on the pin alone — cfg `conservative-pin`) and
`precise_roots_only_keeps_objects_alive` (snapshot roots alone keep +
forward an object, no window — unconditional). Full workspace green
under **both** `--features` configs (default and `--no-default-features`).

Note (scope): conservative pins keep stack-referenced objects *fixed*
(their core contract) and keep childless / precise-rooted pinned objects
alive. Survival of an unrooted pinned object's transitive *heap children*
relies on extension marking, which NCL's fork drives through its own
`mark_minor_with_static` + `collect_minor_with_static` path; the
multi-mutator core does not add that pass to the snapshot-roots driver.

---

## MM-8 — Hardening  *(≈4 days; deps: MM-7)* — ✅ DONE

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

**Shipped:**
- **Stress** (`tests/stress_mt.rs`) — `stress_multi_mutator_alloc_gc_native_pin`:
  N=6 workers concurrently alloc/churn, hold a rooted set with sentinels,
  poll, take `enter_native`/`leave_native` excursions, and pin/unpin
  across collections while a driver runs minor + occasional full cycles;
  every worker asserts its rooted invariant each iteration. Tunable via
  `NEWGC_STRESS_ITERS` (modest default keeps the suite fast). Validated at
  500k iters × 6 workers (3M iterations) in release. Also exercises the
  alloc/refill/pin-vs-collect heap-lock interaction (no deadlock: a worker
  holding the heap lock isn't parked, and the driver locks the heap only
  after all workers park).
- **Loom** (`tests/loom_safepoint.rs`, `#![cfg(loom)]`, wired via
  `[target.'cfg(loom)'.dependencies]` so normal builds never resolve it) —
  three models of the handshake orderings, each a standalone replica of a
  rule in `mutator.rs`: MM-5 root publication visibility, MM-4 Fix B
  torn-read prevention (verified meaningful — removing the lock makes loom
  report the torn read), and the MM-4 resume forwarding. Run with
  `RUSTFLAGS="--cfg loom" cargo test -p newgc-core --test loom_safepoint`.
  (Liveness — the cross-cycle straggler deadlock — is a progress property
  loom doesn't check directly; covered by the targeted analysis + stress.)
- **Config** — `GcCoordinator::set_safepoint_timeout` /
  `safepoint_timeout` make the driver's per-arrival wait budget
  configurable (`Safepoint::wait_timeout_ms`, default 10 s, clamped ≥ 1 ms).
  It is a diagnostic *re-check* backstop, not a "proceed after N s" — the
  protocol doesn't depend on it. Test: `safepoint_timeout_is_configurable`.
  (`safepoint_per_alloc` debug flag + telemetry hooks: deferred — not
  needed for soundness; flag here for the frontend backlog.)
- **Docs** — rewrote `docs/threading.md` from the pre-MM "no safepoint
  API / until steps 1–4" state to the shipped `GcCoordinator`/`Mutator`
  model (TLABs, safepoints, snapshot roots, native convention,
  conservative pins, FFI pin, precise-only build, and what stays
  single-threaded).

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
