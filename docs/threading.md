# Threading

## Current state

There are **two** supported deployment models:

1. **Single heap, single owner** — `PageHeap<L>` used directly by one
   thread (or behind your own `Mutex`). The original model; still valid.
2. **Multi-mutator via `GcCoordinator<L>`** — many threads each hold a
   `Mutator<L>` handle, allocate from per-thread TLABs lock-free, and
   cooperate at safepoints so any one of them can drive a stop-the-world
   collection that sees every thread's roots. This is the model the
   multi-mutator sprints (MM-0..MM-8) delivered.

`PageHeap<L>` is `Send + Sync`. `Mutator<L>` is intentionally **`!Send`**
— a handle is bound to the thread that registered it (it owns
thread-local TLABs and a stack window). `GcCoordinator<L>` is `Clone +
Send + Sync`; clone it to hand a registration capability to each thread.

### Multi-mutator model (`GcCoordinator`)

```rust
let coord = GcCoordinator::<LispLayout>::with_reservation(1 << 30);

// Each thread registers its own (!Send) handle.
let c = coord.clone();
std::thread::spawn(move || {
    let mut m = c.register_mutator();
    let p = m.try_alloc_cons_in(Generation::G0).unwrap();
    // ... build roots ...
    let mut roots = [/* live Words */];
    loop {
        m.poll_safepoint(&mut roots); // cooperate; parks if a cycle is pending
        // ... mutate; roots are forwarded in place across a collection ...
    }
});

// Any registered mutator can drive a collection from its own thread:
let mut driver = coord.register_mutator();
driver.collect_minor(&mut driver_roots, |_evac| { /* extra roots */ });
```

What the coordinator provides:

- **Per-mutator TLABs, lock-free bump (MM-3).** Each `(generation, kind)`
  has a thread-local allocation buffer; the fast path is a pointer bump
  with no lock. The heap mutex is taken only to refill an exhausted TLAB
  or to run a collection.
- **Cooperative safepoints (MM-4).** A global `epoch` + `world_running`
  flag + a park condvar. `poll_safepoint` is a cheap epoch compare on the
  fast path; when a collection is pending it **parks** the caller
  (publishing roots, flushing TLABs) until the world resumes. A driver
  self-parks, stops the world, waits for every *other* active mutator to
  park at the same epoch, collects, and resumes. Registration is
  serialized with STW via `coord_mutex`, so a newcomer can't slip into a
  cycle in progress.
- **Per-mutator snapshot roots (MM-5).** Before parking, each mutator
  publishes its live `Word`s into a snapshot the coordinator visits and
  **updates in place**; on resume the mutator copies the (possibly
  forwarded) values back. This is what makes multi-mutator STW *sound* —
  every thread's roots are seen, and none runs while the heap moves.
- **Native-call convention (MM-6).** `enter_native`/`leave_native`
  bracket a foreign call that may block. While `IN_NATIVE` a thread is
  skipped by the driver's wait loop (it touches no managed heap, so the
  collector runs *around* it) but its published roots are still forwarded.
- **Conservative stack pins across mutators (MM-7).** With the
  `conservative-pin` feature, each mutator publishes a stack window
  (`set_stack_range`); the driver unions all active windows for one
  `pin_pointers_in_ranges` pass, pinning pointer-shaped stack words so
  stack-resident copies the collector can't rewrite stay valid.
- **Explicit FFI pin/unpin (MM-0).** `Mutator::pin(w) -> PinHandle` /
  `unpin` keep an object's address fixed across any number of cycles —
  for addresses that have escaped into foreign code. Independent of the
  `conservative-pin` feature.

**Poll-site contract (§4.2).** The `roots` slice passed to
`poll_safepoint` / `enter_native` must be the mutator's *complete,
consistent* live-root set at that point. A poll with a half-built root
set lets the collector move an object the mutator still holds. The
frontend owns this guarantee (precise stack maps, or `push_root`/`pop_root`).

### Concurrent lock-free reads

`committed_pages()`, `page_count()`, `reserved_bytes()`, and `base_ptr()`
use atomic loads and can be called from any thread without a lock,
including while another thread holds the heap mutex or a collection runs.

### Build configurations

- **Default** (`conservative-pin` on): both precise snapshot roots and
  conservative stack-window pins are available.
- **Precise-roots-only** (`--no-default-features`): the conservative scan
  and the per-mutator stack-window machinery compile out entirely. Suited
  to a statepoint-emitting frontend (e.g. OpenDylan) that supplies precise
  roots. The full workspace test suite passes under both configurations.

## What is still single-threaded

**The collector itself.** Marking and evacuation run single-threaded on
the driver's thread under stop-the-world. There is no concurrent marking,
no parallel evacuation, and no work-stealing. The multi-mutator work made
*allocation* and *root enumeration* concurrent and the *pause*
cooperative; it did not parallelize the GC pause.

**Card-table writes** already use relaxed atomic stores, so concurrent
mutator card-marks are safe with no extra coordination.

## Validation

- A cooperative-safepoint suite (`tests/safepoint.rs`) covers the
  driver/worker handshake, including the cross-cycle straggler and
  lost-wakeup cases.
- `tests/native_call.rs`, `tests/conservative_mt.rs` cover MM-6 / MM-7.
- `tests/stress_mt.rs` is a torture test: N workers concurrently
  allocate, poll, take native excursions, and pin/unpin across collections
  while a driver runs minor + full cycles, asserting no corruption. Tune
  with `NEWGC_STRESS_ITERS` (e.g. `NEWGC_STRESS_ITERS=500000 cargo test
  --release -p newgc-core --test stress_mt`).

The phased design — TLAB retirement, safepoint coordination,
happens-before analysis, the `coord_mutex` protocol, and the native-call
and conservative-pin conventions — is in
[MULTI_MUTATOR_DESIGN.md](MULTI_MUTATOR_DESIGN.md); the sprint breakdown
and shipped-state notes are in
[MULTI_MUTATOR_SPRINTS.md](MULTI_MUTATOR_SPRINTS.md).

---

Back to [Home](index.md).
