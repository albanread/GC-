# NewGC

Page-based mark-evacuate generational garbage collector, written in Rust.

> **⚠️ Experimental — do not depend on this.**
>
> This crate is a research vehicle for a Rust-native, language-agnostic
> page-heap GC. It has not been run against a real workload outside its
> own test suite. The API is unstable, the OOM story is incomplete
> (see DESIGN_REVIEW.md), and multi-threaded mutator support is not
> yet designed. Treat every version as pre-0.1.
>
> **The included mini-Lisp is a GC test driver, not a Lisp.** It exists
> solely to push allocation and mutation patterns at `PageHeap`. It has
> no closures, no tail calls, no macros, no module system, no error
> handling worth the name, and a value stack that doubles as the GC
> root set. Do not build anything on top of it.

Lifted from NewCormanLisp's [page_heap module](https://github.com/.../page_heap) as the starting point. The design follows SBCL's `gencgc.c` (page-based, generational, conservative pinning) with adaptations for memory-safe Rust.

## Status

**Phase 1 — extraction from NCL: DONE.** ≈6,673 lines of page-heap code
copied near-verbatim into [crates/newgc-core/](crates/newgc-core/). 112
unit tests carried over; 24 synthetic stress tests added.

**Phase 2 — language-binding trait: DONE.** The `HeapLayout` trait in
[traits.rs](crates/newgc-core/src/traits.rs) abstracts every
language-specific operation — tag classification, forwarding-marker
encoding, header decoding, fill-word choice. `PageHeap<L>` is generic
over `L: HeapLayout`. The production code paths (mark, evac, pin,
coordinator-api) call `L::classify`, `L::header_layout`,
`L::make_forward`, `L::rewrite_pointer_addr` exclusively — no
language-specific calls in the hot path.

Two reference bindings ship:
- **[`LispLayout`](crates/newgc-core/src/lisp_layout.rs)** — NCL's
  3-bit tag + typed-`HeapHeader` runtime. 12 unit tests.
- **[`TinyLayout`](crates/newgc-core/src/tiny_layout.rs)** — a
  deliberately different 2-bit tag, length-only header layout. 10
  unit tests + 10 end-to-end `PageHeap<TinyLayout>` integration tests.

Total: **311 tests passing** with default features (238 with
`--no-default-features` — the 73-test gap is the pin-scanner
tests + the dependent integration tests, all gated behind the
`conservative-pin` feature).

### `newgc-core` (the GC engine) — 226 tests

- 134 unit tests
- 24 LispLayout synthetic (`tests/synthetic.rs`)
- 10 TinyLayout end-to-end (`tests/tiny_layout_endtoend.rs`)
- 46 categorised workload tests across 12 sections
  (`tests/workloads.rs`) — allocator stress, working-set patterns,
  generational shapes, object graphs, mutation, pathological,
  realistic, pin scanner, long-running stress, cross-gen pinning,
  stats consistency, TinyLayout parallels
- 5 stochastic workload simulators (`tests/stochastic_workload.rs`) —
  5K–20K randomised operations per run with function frames,
  random lifetimes, mixed object types (lists/strings/trees/large
  vectors/small objects), continuous integrity verification
- 7 threading tests (`tests/threading.rs`) — `Send + Sync`,
  independent heaps in parallel, shared heap via `Mutex`,
  concurrent reads. See [THREADING.md](THREADING.md) for the analysis.
- 4 card-barrier tests (`tests/card_barrier.rs`) — old→young
  writes survive minor cycles; mixed minor/major patterns work
  without manual GC scheduling.
- 15 trigger-policy tests (`tests/trigger_policy.rs`) —
  sub-phase 10: `should_collect()` / `collect_auto()` auto-decide
  when to GC and whether to do minor or major based on Tenured
  fill. Allocation budget recomputes each cycle as
  `max(min, 0.5 × tenured_used)`.
- 6 try-collect tests (`tests/try_collect.rs`) — sub-phase 10
  follow-up: `try_collect_*` methods wrap the panic-on-OOM
  evacuator with `catch_unwind` + `Result<_, GcError>`. Clients
  can recover from mid-evacuation OOM by dropping the heap
  instead of taking a process kill.
- 9 GC-stats tests (`tests/gc_stats.rs`) — `PageHeap::stats() ->
  GcStats` returns a one-shot snapshot of capacity, generation
  occupancy, trigger policy, last-cycle telemetry, and cohort
  counters. Replaces the dozen scattered getters.

### `newgc-test-lisp` (mini-Lisp test driver) — 50 tests

A tree-walking Lisp evaluator that drives the GC through actual Lisp
programs — `define`, `let`, `if`, recursion, `cons`/`car`/`cdr`,
vectors, strings, mutation via `set!`, and a deterministic
`(random n)` builtin for stochastic-feeling workloads.

- 7 reader unit tests
- 14 smoke tests (`tests/smoke.rs`) — arithmetic, control flow,
  user-defined functions, mutual recursion
- 17 Lisp workload tests (`tests/lisp_workloads.rs`) — list/tree/
  vector/string builders, fibonacci tree recursion, mixed-shape
  programs, mutation patterns, all with explicit `(gc-major)`
  interspersed
- 5 stochastic Lisp programs (`tests/lisp_stochastic.rs`) — random
  allocation choice, random recursion depth, random mutation
  targets, 300-iteration mixed-workload long run
- 7 script-file runners (`tests/run_scripts.rs`) — workload programs
  in plain `.lisp` files under [`scripts/`](crates/newgc-test-lisp/scripts/).
  Add a new `.lisp` file, drop in a `#[test]` that calls
  `run_script_file`, and you have a new workload — no Rust edit
  required for the program logic. Each script self-verifies via
  `assert` / `check-equal`.

The Lisp evaluator exists as a test driver for the GC, **not** as a
usable Lisp implementation. There are no closures (functions are
defined at top level and looked up by name); the value stack is the
GC root set; safepoints fire on a configurable allocation threshold
plus explicit `(gc-now)` and `(gc-major)`.

**Phase 3 (planned):** real-workload soak across NCL and NewOpenDylan;
upstream sub-phases 9 (soft cards), 10 (trigger policy), 12 (delete
semispace) land in the shared crate.

## Design lineage

- SBCL `gencgc.c` — page-based mark-evacuate, sub-page pin bitmap,
  soft card marks, generation thresholds.
- CCL `lisp-kernel/gc-common.c` — mark-compact-in-place ideas (not
  adopted here, but informed the choice of mark-evacuate over
  copy-to-newspace).
- NewCormanLisp `docs/GC_DESIGN.md` — the synthesis that picked the
  shape currently in this crate.
- NewCormanLisp `docs/GC_LESSONS.md` — the field report from building
  it. Read this before changing anything load-bearing.

## Status of NCL's page-heap at extraction time

Sub-phases landed in upstream NCL:
- 1–8: backend abstraction, page reservation, descriptors, allocation,
  mark, conservative pin scan, evacuation, three-generation policy.
- 11a–11c: Cargo-feature switch, real `collect_minor_with_static`,
  workspace feature propagation.

Sub-phases **not** landed (so the crate is incomplete in the same ways):
- 9: soft card marks + IR-emitted write barrier.
- 10: trigger policy + auto-full-GC budget.
- 12: deletion of upstream semispace (NCL still defaults to semispace).

The 312+ unit tests from upstream all carry over and pass here.

## Building

```
cargo build
cargo test
```

Cross-platform: Windows uses `VirtualAlloc` (reserve + commit),
Unix targets use `mmap(PROT_NONE) + mprotect` + `madvise(MADV_DONTNEED)`.
The `Backing::Boxed` Rust-allocator fallback is kept for exotic
platforms but isn't exercised on Windows or Unix.
