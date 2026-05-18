# NewGC — Phase 1 Design Review

A fresh read of the page-heap source after extraction. Goals: surface
what we overlooked in the lift, what's load-bearing-but-undocumented,
and what should change before Phase 2 (trait extraction).

Status at review time: **136 tests passing** (112 unit + 24 synthetic),
near-verbatim lift from `ncl-runtime/src/page_heap/` with `crate::heap::`
references rewritten to `crate::heap_common::` and `coordinator_api.rs`
retained (it's GC-internal, not a coordinator binding).

## Top 3 things that need to change before OpenDylan can bind

### 1. `HeapType` is Lisp-shaped, not language-agnostic

[heap_common.rs:107](crates/newgc-core/src/heap_common.rs:107) defines
`HeapType` as a closed enum with 10 variants: `Symbol`, `Vector`,
`Function`, `String`, `FfiBlock`, `Other`, `Bignum`, `Float`, `Ratio`,
`Complex`. `word_field_range` then hard-codes which payload cells are
pointer-typed for each. The mark and evac scanners read the header,
match on `HeapType`, and use the returned range.

This is the **NCL-shape we cannot inherit**. OpenDylan's classes have
their own pointer-slot map (driven by `ClassMetadata::scan` in the
existing nod-runtime), and "Bignum/Ratio/Complex" mean nothing in Dylan.

**Phase 2 trait must replace this with**: a method on the language
binding trait that, given a header cell and a payload range, calls back
into the GC's `visit_slot` for each pointer-bearing offset. The GC stays
out of the layout decision entirely.

### 2. Conservative pin scanner is unconditionally in the hot path — **RESOLVED (Cargo feature)**

**Resolution:** `conservative-pin` Cargo feature (default on). The
public `PageHeap::pin_pointers_in_ranges` method and the
`coordinator_api::collect_minor_with_static` call site that
invokes it are both gated `#[cfg(feature = "conservative-pin")]`.
The pin-related fields and accessors (`pinned_cells`,
`is_pinned_cell`, `pinned_count`, `has_pins`) stay unconditionally —
they're cheap when no pins are added, and the `extend_mark_from_*`
methods short-circuit on `pinned_cells.is_empty()`.

For an OpenDylan-style precise-roots-only client:
```toml
[dependencies]
newgc-core = { path = "...", default-features = false }
```
This compiles the pin scanner out entirely (~280 lines of code
gone, plus ~22 pin-specific tests gated to match).

### 3. The `young_*` / `old_*` API on `PageHeap` is semispace-shaped legacy — **RESOLVED (deprecation)**

All `young_*` / `old_*` methods on `PageHeap` are now
`#[deprecated]` with `note` pointing at the page-heap-native
replacement:

| Deprecated | Replacement |
|---|---|
| `young_used_bytes()` | `stats().g0_used_bytes` |
| `old_used_bytes()` | `stats().g1_used_bytes + stats().tenured_used_bytes` |
| `old_capacity_bytes_per_semi()` | `reserved_bytes()` |
| `young_base_ptr()` | `base_ptr() as *const u64` |
| `young_starts_handle()` | `start_bits_handle()` |
| `young_try_alloc_slab(n)` | `try_alloc_g0_slab(n)` |
| `old_cards()` | `cards()` |
| `old_live_base_ptr()` | `base_ptr() as *const u8` |

The deprecated names still work (compile + run, emit warning on
use) so NCL's existing coordinator code can migrate at its own
pace. Internal callers in our crate are switched to the new
names; zero deprecation warnings on a clean build.

`coordinator_api.rs:69,80,107,114,132,150,157` expose:
- `young_used_bytes` / `old_used_bytes`
- `young_base_ptr` / `young_starts_handle` / `young_try_alloc_slab`
- `old_cards` / `old_live_base_ptr` / `old_capacity_bytes_per_semi`

These names made sense in NCL only because the upstream `gc-semispace`
backend had a young+old two-region geometry and the Cargo-feature switch
demanded matching call-sites. In NewGC there's no second backend with
that geometry — the page-heap has 3 generations and one reservation.

**Fix**: rename to the page-heap's native vocabulary
(`g0_used_bytes`, `cards`, `reservation_base`, `try_alloc_slab(gen, ...)`).
Delete the semispace-aliasing layer; the names are dead weight.

## Specific things that were overlooked

### 4. `coordinator_api.rs` is misnamed

The file holds two distinct kinds of method:
- **GC-internal algorithm**: `prepare_recycle_live_counts_from_marks`,
  `recycle_live_counts_active_for`, `clear_recycle_live_counts`,
  `release_zero_live_unpinned_pages`, `mark_minor_with_static`,
  `collect_minor_with_static`, `extend_mark_from_pinned`,
  `extend_mark_from_cross_gen_pinned`.
- **Bind-layer accessors**: the `young_*` / `old_*` names from (3).

When we keep both, the file name implies the algorithm methods are
"coordinator API" — they aren't. The algorithm methods belong with
`cycle.rs` (or a new `recycle.rs`); the bind-layer goes away with (3).

### 5. The cross-gen extend-mark fix has no synthetic regression test

[coordinator_api.rs:230-236](crates/newgc-core/src/page_heap/coordinator_api.rs:230)
runs `extend_mark_from_pinned` for both G0 and G1, plus
`extend_mark_from_cross_gen_pinned(Generation::G0)`. The comment block
references a real crash in NCL's `demos/life.lisp` at
`minor-gcs=15 / bytes-promoted-total=125 MB`.

The pattern: a pinned-G1 object's field points at a G0 cell. The G0
cell isn't marked by the normal precise-mark pass (because the pinned-G1
object isn't a precise root — it's pinned conservatively). Without
cross-gen extend-mark, the G0 cell gets reclaimed, the pinned G1 field
dangles.

**The crash that drove this fix was real-workload-only**. Our synthetic
tests don't have:
- a pinned G1 object (synthetic tests don't exercise conservative pin)
- with a slot pointing into G0
- across a minor cycle

This is exactly the kind of bug `GC_LESSONS.md` Pattern 2 warns about:
mechanics tests can't reach this. Worth adding a synthetic test that
constructs this shape explicitly via the public API
(`pin_pointers_in_ranges` + a hand-crafted "stack" buffer).

### 6. Mid-evacuation OOM is an unconditional panic — **RESOLVED (try_* variants)**

Four panic sites in [heap.rs](crates/newgc-core/src/page_heap/evac.rs):
- `evac.rs:491` — "page heap exhausted mid-evacuation"
- `evac.rs:878` — pinned-object evacuation OOM
- `evac.rs:958` — old-gen evacuation OOM
- `evac.rs:1154` — full-GC scratch OOM

NCL's design doc (sub-phase 10) says trigger budget will pre-allocate to
avoid this. For a language client, the panic is a process kill —
unrecoverable. A GC crate that can crash the host on OOM is a hard sell.

**Resolution:** added `try_collect_minor` / `try_collect_major` /
`try_collect_auto` / `try_evacuate_with_roots` to `PageHeap`. Each
wraps the panicking version with `catch_unwind` and returns
`Result<_, GcError>`. On `Err(GcError::MidEvacOom(GcStallError))` the
client has full diagnostic info (free-page count, generations,
copied-before-failure counts) and can drop the heap to recover —
no process kill. The existing panicking `collect_*` methods stay
for backward compatibility.

The "fix path 1" (pre-flight reservation) is the cleaner future
direction, but option 2/3 hybrid (catch_unwind wrapping) ships
today and is what `newgc-core/tests/try_collect.rs` (6 tests)
exercises. The heap is documented as "indeterminate state after
`Err`" — clients should drop and recreate.

### 7. Conservative pin gates are duplicated between mark and evac

`mark::PageHeap::try_mark_root` and `evac::PageEvacuator::mark_visit_slot`
have **identical six-gate checks** (tag → page → gen → kind → start-bit
→ tag/start consistency). Right now `mark_visit_slot` is a near-duplicate
of `try_mark_root`.

**Fix**: extract into a single `gate_pointer_slot(heap, slot) -> Option<CellIdx>`
helper that returns the validated cell index. Used by both mark and evac
modes. The gates are safety-critical and load-bearing — duplicating them
risks one path getting tightened and the other staying loose.

### 8. `PageHeap` is one struct with 17 fields, all `pub(super)`

[space.rs:83-214](crates/newgc-core/src/page_heap/space.rs:83). Holding
allocator state, page table, two bitmaps, alloc regions, recycle counts,
generation counters, card table, pin summary, page caps. Sibling modules
(`alloc`, `evac`, `mark`, `pin`, `coordinator_api`) reach in directly via
`pub(super)` visibility.

**Costs**:
- Borrow-checker fights: any method on `PageHeap` that touches more than
  one field locks out concurrent reads of unrelated fields.
- Refactoring risk: a change to one field's invariants requires auditing
  every sibling module.
- Lifetime opacity at the public API: callers see one mega-struct and
  can't ask "what's STW-protected vs mutator-callable?"

**Fix (Phase 2)**: split into:
- `PageReservation` — base pointer, commit bits, descs (STW-only mutate
  during commit/decommit, read-anytime via atomics for commit bits).
- `BumpAllocator` — alloc regions + start-bit Arc (mutator-callable, with
  the start-bit bitmap as the lock-free shared part).
- `Collector` — mark bits, recycle counts, generation counters, pin sets
  (STW-only).
- `Telemetry` — `last_mark_live_*`, `last_pin_summary` (read-anytime
  atomic, cleared at cycle end).

Then `PageHeap` is the composer that mediates access. Sibling modules
borrow the specific sub-struct they need.

### 9. The card barrier is allocated but not wired — **RESOLVED**

**Sub-phase 9 landed.** Implementation:

- **`PageHeap::mark_card_at(slot_addr)`** — the mutator API.
- **Cons + vector-set! mark cards** on every heap-pointer write
  (mini-Lisp's `alloc_pair` and `value::vector_set`).
- **`collect_minor` and `collect_major`** both run
  `scan_dirty_cards_as_roots` inside their evacuator closures.
- **Page filter "any gen except `from_gen` and Free"** — broader
  than strictly-older. Necessary because a major's G1→Tenured pass
  needs to scan G0 cards too, in order to find G0→G1 cross-gen
  pointers when a chain straddles G0 and G1 (a transient state
  created when a promoting minor fires mid-mutator-build).
- **Copy marks dest cards unconditionally** (any dest_gen,
  including G0). When the GC moves an object, the dest page's
  cards inherit the may-contain-heap-pointer status.
- **`rebuild_cards_for_old_gens`** at end-of-cycle scans every
  live page (G0, G1, Tenured) — not just G1/Tenured. G0 cards
  persist between cycles so the next major's G0-card scan finds
  previously-marked content.

**Test coverage:** 4 Rust card_barrier tests + the entire
mini-Lisp suite. The script 04 mutation pattern (build cross-gen
chains, mutate slots, mix minor and major cycles) passes with
default GC settings — no workaround needed.

**Bugs found during this work** (all fixed):
1. `c4_collect_major_clears_unrooted_old` — initial filter was
   too permissive and treated intra-gen pointers as roots, keeping
   unrooted G1 alive forever after a major.
2. `script_04_pattern_minor_and_major_mixed` — the 95-cell list
   bug. Root cause: G0 cards weren't being scanned during the
   major's G1→Tenured pass, so cross-gen G0→G1 references via a
   GC-copied G0 chain were invisible. Fix: scan G0 cards too;
   mark dest cards for G0 copies; persist G0 cards between cycles.

### 10. Trigger policy + auto-full-GC — **RESOLVED (sub-phase 10)**

`PageHeap::should_collect()` and `PageHeap::collect_auto()` are the
auto-trigger API. The allocator bumps an internal byte counter on
every cell handed out; `should_collect()` returns true once the
counter crosses `auto_gc_trigger_bytes`. `collect_auto` chooses
minor or major based on Tenured occupancy (default: major when
Tenured > 75% of reservation, configurable via
`set_tenured_full_threshold_bps`).

After each cycle the threshold recomputes to
`max(gc_budget_min_bytes, 0.5 * tenured_used)` — SBCL's GENCGC
trigger heuristic, so cycles get longer as the live set grows.

Mini-Lisp's `maybe_gc` now delegates to `heap.should_collect()` and
`collect_auto`. The `(set-gc-threshold! n)` builtin sets the byte
budget; `set-majors-every` translates an old "every Nth minor"
intent into a Tenured-fraction-bps setting.

15 trigger-policy tests in [`tests/trigger_policy.rs`](crates/newgc-core/tests/trigger_policy.rs).

### 11. No safepoint / poll-word API

A JIT-compiled mutator needs:
- A poll word the GC can flip to request a safepoint.
- An entry point the mutator calls when the poll word is hot (`gc_pitstop`).
- A way to enumerate roots from JIT-emitted stack maps at the safepoint.

NCL's design doc calls this Phase 4. It's not in the crate. Without it,
multi-threaded GC is structurally impossible — single-thread STW is the
only safe model.

**Phase 2 trait surface should include**:
```rust
trait GcPolicy {
    fn at_safepoint(&self) -> bool;
    fn enumerate_roots(&self, visit: &mut dyn FnMut(&mut Word));
}
```

The crate provides a `Heap::request_safepoint()` that sets a flag; the
binding calls `at_safepoint()` from the JIT-emitted poll path; the
binding implements `enumerate_roots` via statepoint stack maps (Dylan)
or via the conservative scan + static area walk (NCL).

### 11. Boxed object header has 24-bit length, capping at ~16M cells

[heap_common.rs:96-103](crates/newgc-core/src/heap_common.rs:96).
`LEN_BITS = 24`, `MAX_OBJECT_CELLS = 16_777_215`. At 8 bytes per cell,
**max single object size is 128 MiB**. Fine for Lisp; tight for
"Large" pages — sub-phase 7 deferred large objects, and `PageKind::Large`
exists but is unimplemented.

When large-object support lands, either the length field grows (4 bytes
header → 8 bytes header — a cell width change), or large pages use
a different layout (header stores byte count instead).

### 12. Identity of `PAGE_SIZE_BYTES = 64 KB` is implicit

Hard-coded as 64 KB to match Windows `VirtualAlloc` allocation
granularity. On Linux where mmap granularity is 4 KB, this is wasteful
(192 KB descs table for a 1 GB reservation is fine, but small-allocation-
intensive workloads pay for full 64 KB commits when 4 KB would do).

**Phase 2 trait**: make `PAGE_SIZE_BYTES` a const generic or a runtime
parameter, defaulting to OS granularity. Re-derive `PAGE_SIZE_CELLS`,
`PIN_SLOTS_PER_PAGE`, `CELLS_PER_PIN_SLOT` accordingly.

### 13. No `GcStats` / introspection surface — **RESOLVED**

`PageHeap::stats() -> GcStats` returns a one-shot snapshot. The
struct exposes ~50 fields organised in five groups:

- **Capacity**: `reserved_bytes`, `committed_bytes`,
  `committed_pages`, `total_pages`, `free_pages`,
  `page_size_bytes`.
- **Generation occupancy**: per-gen `used_bytes`, `used_pages`,
  `free_pages` for G0, G1, Tenured, plus rolled-up totals.
- **Trigger policy**: `bytes_alloc_since_gc`,
  `auto_gc_trigger_bytes`, `gc_budget_min_bytes`,
  `tenured_full_threshold_bps`.
- **Last-cycle telemetry**: `last_mark_live_bytes`,
  `last_mark_live_pages`, `last_zero_live_pages_released`,
  `last_pin_summary` (cohort-bucketed pin counts).
- **Cohort counters**: `minors_since_g0_promote`,
  `g0_promotes_since_g1_promote`, `total_minors`, `total_majors`.

Mini-Lisp exposes the subset most workloads need via the
`(heap-used-bytes)`, `(heap-g0-used-bytes)`,
`(heap-tenured-used-bytes)`, `(heap-free-pages)`,
`(heap-bytes-alloc-since-gc)`, `(heap-auto-gc-trigger-bytes)`
builtins; `scripts/07-gc-stats.lisp` exercises them.

9 tests in [`tests/gc_stats.rs`](crates/newgc-core/tests/gc_stats.rs).

### 14. Backing::Boxed warning is real on Windows — **RESOLVED (mmap landed)**

`Backing::Mmap { base, reserved_bytes }` ships behind
`#[cfg(unix)]` in `space.rs`. The constructor calls
`mmap(NULL, total_bytes, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS,
-1, 0)` to reserve address space without committing; commit pages
do `mprotect(PROT_READ | PROT_WRITE)`; decommit pages do
`madvise(MADV_DONTNEED) + mprotect(PROT_NONE)` to release physical
pages and re-trap accesses. `Drop` calls `munmap` on the full
reservation.

The Windows path is unchanged (`VirtualAlloc` reserve +
`VirtualAlloc(MEM_COMMIT)` / `VirtualFree(MEM_DECOMMIT)`). The
`Backing::Boxed` variant is kept for exotic platforms (neither
Windows nor Unix); the "never constructed" warning is now only
visible on that hypothetical third target — both shipping
platforms construct one of `Virtual` / `Mmap`.

Cross-platform claim now holds: every shipping target exercises a
real OS-virtual-memory backing.

### 15. The `recycle_live_counts: Vec<u16>` array is `n_pages` long

64 KB pages → 8192 cells per page → 16-bit count overflows at 65535
cells. A page can hold at most 8192 cells, so `u16` is fine. Document
the invariant — a future bump to 1 MB pages would silently overflow.

### 16. `Send + Sync` for `Backing::Virtual` is asserted by `unsafe impl`

[space.rs:235-236](crates/newgc-core/src/page_heap/space.rs:235).
Justified by the comment "process-lifetime stable + mediated by
commit-bit bitmap + commit_lock". Fine. But the **same Send+Sync claim
implicitly propagates to `PageHeap`**, which holds the `commit_lock`
and atomic commit bits AND the non-atomic `descs: Vec<PageDesc>` and
`mark_bits: Box<[u64]>`.

The contract is: descs/mark_bits are mutated only under STW (exclusive
`&mut PageHeap`). That's enforced by Rust's borrow checker today
because all mutation goes through `&mut self` methods. But the
`unsafe impl Send + Sync for Backing` is misleading — it makes it look
like `Backing` is fully thread-safe when actually `PageHeap` as a whole
needs STW discipline.

**Fix**: either add a `pub struct GcLock(...)` token type that callers
must hold to mutate, or strengthen the doc comment that says "all
non-atomic mutation requires exclusive access."

## What we got right

- `coordinator_api.rs` came over cleanly — its imports were already on
  the right side of the bind line (`heap_common`, not `crate::heap` /
  `crate::word` / NCL coordinator types).
- The five-gate `maybe_copy` in evac.rs explicitly rejects
  payload-shaped-pointer false positives. This is the kind of safety
  rail that pays for itself in real workloads.
- The cohort promotion model (`minors_since_g0_promote`,
  `g0_promotes_since_g1_promote`) cleanly separates "which cycle is
  this?" from "what generation are we copying out of?"
- The forwarding-pointer encoding lives in the `Word` type (`Tag::Forward`),
  not in the heap header, so a moved object's first cell unambiguously
  signals "moved" via the tag check.
- Tests carried over with **only path rewrites** — none of the test logic
  depended on NCL-coordinator types.

## Recommended next steps

In priority order:

1. **Phase 2 trait extraction.** Define `HeapWord`, `ObjectShape`,
   `RootPolicy` traits. Refactor page_heap to be generic. Migrate the
   embedded NCL Word/HeapHeader into a `default-lisp` Cargo feature for
   one synthetic test client.
2. **Split `coordinator_api.rs`** along the line from (3)+(4) — drop the
   semispace-aliasing names, move algorithm methods to `cycle.rs` or a
   new `recycle.rs`.
3. **Feature-flag conservative pinning.** `default-features = []` for the
   pure-precise-root case (target: OpenDylan + statepoints); `features =
   ["conservative-pin"]` for NCL.
4. **Fix the OOM panics.** Pre-flight reservation check; `Result` return
   from the cycle entry points; client-decides recovery.
5. **Add the missing regression test** for the cross-gen pinned-field
   extend-mark. Construct a fake stack with a Word pointing at a G1
   object that owns a slot to a G0 cell; pin it; minor; verify the G0
   target survives.
6. **Sub-phase 9 (soft cards)**: define the mutator-side `mark_card`
   trait method; have the binding call it on old-gen pointer stores; add
   GC-side card-scan in `collect_minor` for `G1` and `Tenured` pages.
7. **Sub-phase 10 (trigger policy)**: budget-based auto-GC.
8. **`GcStats` snapshot API** consolidating the 6+ getters.
9. **Linux `mmap`-based Backing.** Re-establish cross-platform.
10. **Safepoint / poll-word API** when multi-threading becomes a real
    concern.

## What this review can't see

`GC_LESSONS.md` Pattern 2 still applies. None of the 24 synthetic tests
exercise real workloads — they're mechanic-shaped probes against
hand-built object graphs. Specifically, the bugs in NCL's history that
this review can't probe:

- Macroexpand-all spirals (recursive form-tree allocators).
- `life.lisp` minor-gcs=15 dangling pointer (the cross-gen
  pinned-field bug from (5)).
- Heap-monitor steady-state under randomly chosen workloads.

The way to find those bugs is to bind NewGC into NCL and re-run NCL's
demo suite, or to bind it into NewOpenDylan and run Richards/macros/
conditions. **Both bindings are Phase 2 work.** Until at least one of
them runs a real load against this crate clean for a week, the test
count is reassurance, not evidence.
