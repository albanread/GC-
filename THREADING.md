# NewGC — Threading Analysis

**Question:** Does the GC work with multiple threads?

**Short answer:** **Partially.** You can use multiple threads with `PageHeap`,
but only in three patterns — and all three have caveats. There is no
concurrent-mutator support today, by design. The roadmap to add it
is mapped out below.

This document records what works, what doesn't, and what's missing,
backed by [`crates/newgc-core/tests/threading.rs`](crates/newgc-core/tests/threading.rs)
(7 tests, all passing).

---

## What works today

### 1. `PageHeap<L>: Send + Sync`

Type-system-verified by `pageheap_is_send` and `pageheap_is_sync`.

```rust
fn assert_send<T: Send>() {}
fn assert_sync<T: Sync>() {}
assert_send::<PageHeap<LispLayout>>();
assert_sync::<PageHeap<LispLayout>>();
```

This compiles, so any thread can hold a `PageHeap` and the type is safe
to wrap in `Arc<Mutex<_>>`, `RwLock`, etc.

The `unsafe impl Send + Sync for Backing` in
[space.rs:235](crates/newgc-core/src/page_heap/space.rs:235) plus the
atomic-bit + mutex-protected commit/decommit machinery is the
underpinning. Everything else in `PageHeap` (`Vec<PageDesc>`,
`Box<[u64]>`, `Arc<CardTable>`, `Arc<[AtomicU64]>`) is naturally
`Send + Sync` already.

### 2. N independent heaps in parallel

Each thread owns its own `PageHeap`. They share nothing — no
coordination, no contention.

```rust
let handles: Vec<_> = (0..4).map(|_| thread::spawn(|| {
    let mut heap = PageHeap::<LispLayout>::with_reservation(8 * 64 * 1024);
    for _ in 0..1000 { heap.try_alloc_cons_in(Generation::G0); }
    heap.evacuate_from_word_roots(Generation::G0, Generation::G1, &mut []);
})).collect();
```

This is the **embarrassingly parallel** pattern: independent
work-units, no shared state. Useful for:
- Test fuzzers spawning isolated heaps per thread.
- Per-shard data processing where each shard has its own heap.
- Compiler workers that produce separate, non-interacting outputs.

### 3. One heap shared via `Mutex` — serialized allocation

The heap can be wrapped in `Arc<Mutex<PageHeap>>` and shared. All
allocators take the lock, allocate, release.

```rust
let heap = Arc::new(Mutex::new(PageHeap::<LispLayout>::with_reservation(32 * 64 * 1024)));
for _ in 0..n_threads {
    let h = Arc::clone(&heap);
    thread::spawn(move || {
        for _ in 0..500 {
            let mut h = h.lock().unwrap();
            h.try_alloc_cons_in(Generation::G0);
        }
    });
}
```

**What this gives you:** correctness — no races, no corruption.
The GC sees the heap in a consistent state.

**What this doesn't give you:** parallelism. Allocation throughput
collapses to "one mutator at a time, plus mutex overhead." For
allocation-heavy workloads this is *worse* than single-threaded
because of the lock contention.

### 4. Lock-free concurrent reads of stats

The `&self` accessors — `committed_pages`, `count_pages_in_gen`,
`page_count`, `committed_bytes`, `is_committed(idx)`, `desc(idx)`,
`is_marked(idx)` — work concurrently across many threads, no lock
required. They use atomic loads internally.

The `read_only_accessors_work_concurrently` test runs 8 threads each
making 6,000 read calls against a shared `Arc<PageHeap>`. All threads
see consistent values (no concurrent mutator to disturb the state).

This pattern is what a "GC stats endpoint" or "live diagnostic
dashboard" thread would use.

---

## What doesn't work — and why

### Concurrent mutators on a shared heap

You **cannot** have two threads simultaneously calling
`try_alloc_cons_in` on the same heap. The signature is `&mut self`;
the borrow checker rejects any attempt to share `&mut PageHeap`
across threads.

This is **deliberate type-system enforcement of single-mutator
discipline.** The data races a concurrent allocator would face are
real:

- The current alloc fast path bumps a single per-(gen, kind)
  `AllocRegion`. Two threads bumping the same region's pointer
  without coordination would tear allocations.
- The start-bit bitmap uses `AtomicU64::fetch_or` for the bit set,
  which IS thread-safe. So the bitmap could in principle accept
  concurrent writes — but the bump pointer can't.
- `commit_page` takes a `Mutex<()>` so concurrent page-commit calls
  serialize correctly. The commit-bit bitmap is atomic. But once a
  page is committed, allocation into it is serial.

### Cooperative GC parking

When the collector runs, it takes `&mut PageHeap`. While that
exclusive borrow is held, no other thread can do anything — read or
write — on the heap. There's no way to "park" mutator threads at a
safepoint and run the collector while they wait.

For a real multi-threaded mutator runtime, you'd want:

1. Each mutator thread polls a per-thread "should-park" word at every
   back-edge and function entry.
2. When the GC needs to run, it sets every thread's poll word.
3. Each mutator hits its next poll, calls `gc_pitstop`, and blocks on
   a condition variable.
4. The GC waits for all mutators to park, then runs.
5. The GC signals the condition variable; mutators resume.

This is the **safepoint / poll-word protocol** — NCL's GC_DESIGN.md
Phase 4 work, deferred until multi-threading is a real requirement.
**Not present in newgc-core.**

### Per-thread root walking

A real multi-mutator GC would need to enumerate roots from EACH
thread's stack, not just one. Today the root-walking is
single-stream: `evacuate_with_roots(F)` calls F once with one
`PageEvacuator`. The closure feeds it whatever roots there are —
in tests, a `Vec<Word>`; in NCL, the mutator's spill area.

For multi-threading, the protocol would be:

1. All mutators park at safepoints (above).
2. The collector enumerates each parked thread's stack and stack-map
   to gather precise roots.
3. The roots from all threads feed into one evacuation pass.

The per-thread enumeration is whatever-the-language-binding-provides
work. The collector-side aggregation is straightforward once the
input shape is fixed — but the protocol/API doesn't exist yet.

### Per-thread TLABs

The current `AllocRegion` design is "one bump pointer per (gen,
kind)" — six regions total. For multiple mutators allocating
concurrently, you'd want **per-thread TLABs**: each thread reserves
a slab from a region's free pages, bumps within the slab locally,
refills from the central region when empty.

The page-heap design has the bones for this — `young_try_alloc_slab`
in `coordinator_api.rs` reserves a TLAB-sized chunk. But there's no
thread-local cache wrapped around it; the mutator-side bump pointer
still requires `&mut self` on the heap.

---

## Summary

| Pattern | Today | Required to enable concurrent mutator |
|---|---|---|
| `PageHeap<L>: Send + Sync` | ✅ | — |
| Independent heaps in parallel | ✅ | — |
| Shared heap, single mutator, multiple readers | ✅ | — |
| Shared heap, multiple mutators serialised via `Mutex` | ✅ (slow) | — |
| **Shared heap, multiple mutators concurrent** | ❌ | per-thread TLABs |
| **Cooperative GC parking** | ❌ | safepoint / poll-word API |
| **Per-thread root walking** | ❌ | safepoint + binding-provided stack enum |

## Roadmap to true multi-thread support

In dependency order:

1. **Per-thread TLAB.** Wrap `young_try_alloc_slab` in a per-thread
   cache. Mutators allocate from the cache without taking a lock;
   refills go through the heap's mutex.
2. **Safepoint / poll-word API.** Add a `Heap::request_safepoint()`
   that flips a per-thread atomic word. Mutators check the word at
   every back-edge (JIT-emitted in real clients; explicit
   `(safepoint-check)` in test clients) and park if set.
3. **Cooperative parking.** Mutators that hit a safepoint block on a
   condition variable until the GC says "go." The GC waits for all
   mutators to park before starting collection.
4. **Per-thread root enumeration.** Each parked mutator hands the GC
   a `Vec<Word>` of its current roots (or a callback that walks its
   stack map, for statepoint-emitting clients).
5. **Concurrent reads during collection.** Optional — most STW GCs
   forbid concurrent reads too. If wanted, the collector's STW
   exclusivity could be relaxed for `&self` accessors by marking
   them `&self` with internal atomics.

Each step is implementable without disrupting earlier ones. Steps
1–2 unlock benchmarkable concurrent mutators. Steps 3–5 are the
production-quality finish.

Until those land, the **correct pattern for multi-threaded clients
today is**:

- One heap per thread for independent work, **or**
- A `Mutex<PageHeap>` shared between threads for joint work, accepting
  serialized allocation, **or**
- A single mutator thread with multiple reader threads consulting
  stats via `&self` accessors.

NCL and (eventual) NewOpenDylan bindings will need steps 1–4 before
they can support multi-threaded user programs. That's a separate
sprint, scheduled when multi-threading is a real requirement rather
than speculative.
