# Threading

## Current state

`PageHeap<L>` implements `Send + Sync` — the type-system assertion that it is safe to share across threads. In practice, what "safe" means depends on how you use it.

### What works today

**Independent heaps per thread.** Each thread constructs its own `PageHeap` and never shares it. Allocations, collections, and root walks are all thread-local with no coordination. This is the intended deployment model for Phase 1 and the pattern used by the threading test suite.

```rust
let handles: Vec<_> = (0..4).map(|_| {
    std::thread::spawn(|| {
        let mut heap = PageHeap::<LispLayout>::with_reservation(64 * 1024 * 1024);
        // ... allocate and collect independently
    })
}).collect();
```

**Shared heap via `Mutex`.** One heap is wrapped in `Arc<Mutex<PageHeap<L>>>` and multiple threads allocate from it. Allocation is serialised through the lock. Collection is also serialised — the thread that acquires the lock runs the cycle, everyone else blocks.

```rust
let heap = Arc::new(Mutex::new(PageHeap::<LispLayout>::with_reservation(1 << 30)));
let h = Arc::clone(&heap);
std::thread::spawn(move || {
    let mut g = h.lock().unwrap();
    let cell = g.try_alloc_cons_in(Generation::G0);
});
```

This is correct but slow. Every allocation acquires a mutex; contention on the lock becomes the bottleneck under parallel workloads.

**Concurrent lock-free reads.** `committed_pages()`, `page_count()`, `reserved_bytes()`, and `base_ptr()` use `AtomicU64` loads and can be called from any thread without a lock, including while another thread holds the heap mutex.

### What does not work

**Concurrent mutators without a lock.** The bump-pointer allocator (`AllocRegion`) is not atomic. Two threads writing to the same `AllocRegion` simultaneously will corrupt it. Do not share a heap across threads without a `Mutex` or equivalent.

**Parallel GC.** The collection algorithms are single-threaded. There is no concurrent marking, no parallel evacuation, no work-stealing.

**Cooperative safepoint parking.** There is no `safepoint_poll()` API. The mutator cannot signal that it is safe to stop. GC cycles must be explicitly triggered — the caller is responsible for ensuring all mutators have stopped before calling any `collect_*` method.

**Per-thread root enumeration from stack maps.** There is no integration with Rust's stack-unwinding ABI or LLVM's statepoint intrinsics. Roots must be enumerated manually by the caller in the `visit_roots` closure.

## Roadmap to concurrent mutators

The steps required, in order:

1. **Per-thread TLABs (Thread-Local Allocation Buffers).** Each thread owns a slab of cells from the central free-page pool. Allocation bumps the thread-local pointer without a lock. The central pool uses an atomic CAS or mutex only when the TLAB is exhausted.

2. **Safepoint / poll-word API.** Each mutator thread checks a shared `AtomicBool` (the safepoint flag) on back-edges, function entries, or allocation sites. When the flag is set, the thread parks on a `Condvar` and waits for the GC to complete.

3. **Cooperative parking.** The GC thread sets the safepoint flag and waits until all mutator threads have parked. It then runs the collection, clears the flag, and wakes all threads.

4. **Per-thread root walking.** Each parked thread must enumerate its own roots (stack slots, registers saved to a `jmp_buf`-style structure) and pass them to the evacuator. This requires either precise stack maps (from the compiler) or conservative stack scanning (`pin_pointers_in_ranges` over the saved frame).

5. **Card table atomicity under concurrent writes.** The card table already uses `AtomicU8::store(Relaxed)` — concurrent mutator card-marks are already safe. No change needed here.

Until steps 1–4 are implemented, the correct model is: one mutator thread, or multiple threads using `Mutex<PageHeap>`.

A detailed phased design for implementing concurrent mutators — including TLAB retirement, safepoint coordination, happens-before analysis, and the `coord_mutex` protocol — is in [MULTI_MUTATOR_DESIGN.md](MULTI_MUTATOR_DESIGN.md).

---

Back to [Home](index.md).
