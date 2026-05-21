# VMDylanSprint — Windows VM-assisted NewGC sprints for Dylan integration

**Date:** 2026-05-20
**Audience:** NewGC maintainers + NewOpenDylan team
**Status:** Plan — not yet implemented

---

## Background

The Dylan team reviewed NewGC against Dylan-the-language's GC requirements and identified three
blocking gaps (ordered by impact):

1. **Large-object allocation** — `<table-buckets>` and `<simple-object-vector>` exceed the 8192-cell
   single-page cap. Blocks the first moderate-size `<table>`.
2. **Tenured-generation collection** — `collect_full` never reclaims Tenured objects. REPL and IDE
   sessions leak without bound.
3. **Heap growth on OOM** — fixed 1 GB reservation exhausted by non-trivial compilation runs.

A parallel review of the Windows virtual-memory API surface found that two VM primitives —
**lazy-committed VA reservations** and **`MEM_WRITE_WATCH`** — restructure the solution space
significantly:

- A 32 GB lazy VA reservation (no physical cost until committed) makes large-object
  contiguity guaranteed and eliminates the heap-growth problem entirely.
- `MEM_WRITE_WATCH` can replace the per-store software write barrier with a hardware-tracked
  dirty-page list, zero mutator overhead.

This document describes the resulting four sprints, their technical contracts, and their test plans.

---

## Sprint map

| Sprint | Title | Resolves gap |
|--------|-------|-------------|
| **VM-0** | VM infrastructure — lazy metadata + 32 GB reservation | Prerequisite for all |
| **VM-1** | Large-object allocation (`PageKind::Large`) | Gap 1 |
| **VM-2** | Tenured-generation collection (`collect_full`) | Gap 2 |
| **VM-3** | `MEM_WRITE_WATCH` write barrier (perf) | Future perf, not blocking |

Gap 3 (heap growth) is **eliminated** by VM-0. The 32 GB reservation is larger than any Dylan
workload we anticipate; the only remaining OOM scenario is a runaway leak, which VM-2 fixes.

---

## Sprint VM-0 — Lazy-backed metadata via separate VA reservations

### Problem

`PageHeap` currently allocates `start_bits` (`Arc<[AtomicU64]>`, 32 MB) and `mark_bits`
(`Box<[u64]>`, 16 MB) for a 1 GB reservation. Scaling to 32 GB would require 1 GB of
`start_bits` and 512 MB of `mark_bits` — all committed upfront in RAM. That is untenable.

### Solution

Allocate the heap backing, `start_bits`, and `mark_bits` from **three separate
`VirtualAlloc(MEM_RESERVE)` ranges** that are committed lazily, page-by-page, as heap pages are
opened. On a 64-bit process, VA space is 128 TB; reserving 33.5 GB of it costs a handful of
kernel VAD entries and nothing in RAM.

#### Reservation layout

```
Heap backing:       32 GB  (= DEFAULT_MAX_PAGES × PAGE_SIZE_BYTES)
start_bits mirror:   1 GB  (= 32 GB / 8 bytes/cell × 2 bits/cell / 8 = 1 GB)
mark_bits mirror:  512 MB  (= 32 GB / 8 bytes/cell × 1 bit/cell / 8 = 512 MB)
descs Vec:           6 MB  (= DEFAULT_MAX_PAGES × 12 bytes — normal Rust allocation, pre-sized)
```

Physical RAM usage is proportional only to committed pages. A 16 MB live heap (256 committed
pages) consumes 256 × (64 KB heap + 2 KB start_bits + 1 KB mark_bits) ≈ 16.75 MB RAM.

#### Commit/decommit contract

Every call to `commit_page(idx)` commits three contiguous OS pages in a single batch:

```
heap page:        heap_base  + idx × 64 KB           (64 KB, PAGE_READWRITE)
start_bits slab:  sb_base    + idx × 256 × 8 bytes   (2 KB, PAGE_READWRITE)
mark_bits slab:   mb_base    + idx × 128 × 8 bytes   (1 KB, PAGE_READWRITE)
```

The OS zero-fills all three on commit. This replaces the explicit zero-fill loop in `acquire_free_page`
(Bug #4 fix) for start_bits and mark_bits — those slices are already zero. The heap-cell
zero-fill remains (forwarding markers from prior tenants must be cleared).

`decommit_page(idx)` calls `DiscardVirtualMemory(addr, PAGE_SIZE_BYTES)` on each of the three
slices. `DiscardVirtualMemory` tells the OS the contents are garbage; the physical pages are
returned under memory pressure but the committed state is preserved, avoiding a decommit +
recommit syscall roundtrip for pages that cycle through Free frequently. On platforms that don't
have `DiscardVirtualMemory`, fall back to `VirtualFree(MEM_DECOMMIT)` as before.

#### `Backing` change

```rust
enum Backing {
    Boxed(Box<[u8]>),                     // unchanged; test / non-Windows path

    #[cfg(windows)]
    Virtual {
        heap_base:       *mut u8,
        heap_bytes:      usize,
        sb_base:         *mut u8,         // start_bits VA reservation
        sb_bytes:        usize,
        mb_base:         *mut u8,         // mark_bits VA reservation
        mb_bytes:        usize,
    },

    #[cfg(unix)]
    Mmap { ... },                         // unchanged; future: add sb/mb mirrors
}
```

`Drop` calls `VirtualFree(MEM_RELEASE)` on all three VA ranges.

#### `start_bits` and `mark_bits` field types

`start_bits: PageStartBits` changes from `Arc<[AtomicU64]>` to a thin wrapper around a raw
pointer into the VA reservation:

```rust
pub struct VmStartBits {
    base: *mut AtomicU64,   // points into sb_base (always aligned to 8 bytes)
    n_words: usize,
}
unsafe impl Send for VmStartBits {}
unsafe impl Sync for VmStartBits {}
```

The `Arc` clone pattern used in `collect_minor` / `collect_major` to pass `cards` +
`start_bits` into closures is replaced with a plain copy of the `VmStartBits` struct
(it is `Copy`). The pointer stability guarantee comes from the VA reservation's lifetime
(tied to `PageHeap` lifetime, which is `'static` in practice).

`mark_bits: Box<[u64]>` changes to `VmMarkBits { base: *mut u64, n_words: usize }` similarly.

#### `committed_bits` bitmap

With lazy metadata, `committed_bits` remains as-is for the fast-path `is_committed` check.
The bitmap tracks our own state, not the OS's — this is correct because we always commit/decommit
through our own functions. `VirtualQuery` as a fallback is too expensive for the hot path.

The `commit_lock: Mutex<()>` is retained for STW correctness (sub-phase 11 multi-thread work
will revisit this).

#### Default constants

```rust
pub const DEFAULT_MAX_PAGES:    usize = 32 * 1024 * 1024 * 1024 / PAGE_SIZE_BYTES; // 524 288 pages
pub const DEFAULT_RESERVATION_BYTES: usize = DEFAULT_MAX_PAGES * PAGE_SIZE_BYTES;  // 32 GB
```

`with_reservation(n)` now accepts a `max_bytes` ceiling and reserves that much VA upfront,
regardless of how many pages are actually committed. For tests, `with_reservation(8 * 64 * 1024)`
passes `n` as both the committed cap and the VA ceiling so the test heap stays small.

#### Platform notes

- **Windows (primary):** Three `VirtualAlloc(MEM_RESERVE, PAGE_NOACCESS)` calls on construction;
  per-page `VirtualAlloc(MEM_COMMIT, PAGE_READWRITE)` + `DiscardVirtualMemory` on open/close.
- **Unix (secondary):** `mmap(PROT_NONE, MAP_NORESERVE)` for the three ranges;
  `mprotect(PROT_READ|WRITE)` for commit; `madvise(MADV_DONTNEED) + mprotect(PROT_NONE)` for decommit.
  Behaviour is equivalent; `DiscardVirtualMemory` equivalent is `madvise(MADV_FREE)` before
  `mprotect(PROT_NONE)`.
- **Boxed (tests without OS primitives):** Unchanged — all "pages" stay permanently committed;
  the max reservation is whatever was passed; no growth.

#### Tests (VM-0, 7 tests)

| Test | What it checks |
|------|---------------|
| `vm0_commit_zeroes_heap_and_metadata` | After `commit_page(i)`, heap cells + start_bits + mark_bits slices are all zero |
| `vm0_decommit_clears_committed_bit` | After `decommit_page(i)`, `is_committed(i)` is false |
| `vm0_recommit_after_discard_is_zero` | After decommit + recommit, heap cells are zero (no forwarding marker from prior tenant) |
| `vm0_reservation_fits_32gb_va` | `with_reservation(32 * 1024 * 1024 * 1024)` succeeds on Windows without committing RAM |
| `vm0_start_bits_index_matches_heap_page` | start_bits word for cell `page_idx × PAGE_SIZE_CELLS` is in the committed slab |
| `vm0_mark_bits_index_matches_heap_page` | Same for mark_bits |
| `vm0_boxed_backing_unchanged` | Box-backed path still passes all existing space.rs tests |

#### Relationship to other sprints

VM-0 is a **prerequisite** for VM-1 (large objects need guaranteed-contiguous VA) and implicitly
**eliminates the need for a heap-growth sprint** (32 GB is sufficient; adding another range would
be a one-function change if ever needed).

---

## Sprint VM-1 — Large-object allocation (`PageKind::Large`)

### Problem

`<simple-object-vector>` and `<table-buckets>` can exceed 8192 cells (one 64 KB page). A 5,000-key
Dylan `<table>` needs ~24,000 cells for its bucket array. NewGC currently rejects any allocation
larger than `PAGE_SIZE_CELLS`.

### Solution

Implement a multi-page *large-object run*: a contiguous sequence of pages all marked
`PageKind::Large`, where the head page records the run length and the GC treats the entire run
as a single, immovable object.

#### `PageDesc` change

Repurpose the unused `_pad: u16` field:

```rust
pub struct PageDesc {
    pub scan_start_offset: u32,
    pub words_used:        u16,
    pub generation:        Generation,
    pub kind:              PageKind,
    pub pin_byte:          u8,
    pub age:               u8,
    pub n_span:            u16,   // was _pad; see below
}
```

`n_span` semantics:

| `kind` | `n_span` | Meaning |
|--------|---------|---------|
| `Large` | `N >= 1` | Head of an N-page run |
| `Large` | `0`      | Continuation page of a run |
| `Cons` / `Boxed` | `1` | Normal single-page allocation |
| `Free` | `0` | Free page |

`PageDesc::fresh(gen, kind)` sets `n_span = 1` for Cons/Boxed, `0` for Free. Large head pages
set `n_span` explicitly. `PageDesc::release()` resets `n_span = 0`.

New predicates:

```rust
pub fn is_large_head(&self) -> bool  { self.kind == PageKind::Large && self.n_span >= 1 }
pub fn is_large_cont(&self) -> bool  { self.kind == PageKind::Large && self.n_span == 0 }
```

#### Allocation

New method in `alloc.rs`:

```rust
impl<L: HeapLayout> PageHeap<L> {
    /// Allocate one large object of `n_cells` cells in `generation`.
    /// Returns a pointer to the first cell of the object, or `None`
    /// if no contiguous run of sufficient pages is available.
    ///
    /// Large objects always start at cell 0 of their head page.
    /// They are never bump-allocated alongside small objects.
    pub fn try_alloc_large(
        &mut self,
        n_cells: usize,
        generation: Generation,
    ) -> Option<NonNull<u64>>
```

Algorithm:

1. `n_pages = n_cells.div_ceil(PAGE_SIZE_CELLS)`.  
   Panic if `n_pages > DEFAULT_MAX_PAGES` (request is structurally impossible).
2. Linear scan of `descs` for the first `n_pages` contiguous `Free` pages.
   (With the 32 GB VA reservation from VM-0, this always succeeds for reasonable requests.)
3. Commit all `n_pages` pages via `commit_page`.
4. Stamp head page: `kind = Large, generation, n_span = n_pages as u16, words_used = n_cells as u16`.
5. Stamp each continuation page: `kind = Large, generation, n_span = 0, words_used = PAGE_SIZE_CELLS as u16`.
6. Set one start bit at the head page's cell 0 (boxed-header start, `01` encoding).
7. Return `NonNull::new_unchecked(head_page_ptr as *mut u64)`.

The caller writes the `HeapHeader` at cell 0 (class wrapper pointer, exact slot count) exactly
as for a boxed object. The GC finds the object boundary from `n_span × PAGE_SIZE_CELLS`.

#### Evacuation — large objects are pinned in place

Large objects are **never copied**. During `evacuate_with_roots`, when `visit` encounters a
pointer into a `PageKind::Large` page:

1. Read the cell at the pointer address. If it already holds a `Tag::Forward`, follow it
   (the object was already processed this cycle — shouldn't happen for Large, but defensive).
2. Otherwise: the object stays at its current address. Write no forwarding marker. Do not push
   onto the BFS queue. Rewrite the calling slot to the same pointer value (no-op, but keeps the
   code path uniform).

In the page-reclaim loop after BFS completes, for each `from_gen` page:

- **Unpinned Cons/Boxed page:** `PageDesc::release()` → Free.
- **Large head page:** check reachability via `pages_recycled_mid_evac` or a mark bit.
  If the object has zero live references (not visited by BFS), release the entire run:
  `for i in 0..n_span { release(head_page_idx + i) }`.
  If the object is live, flip the entire run's generation:
  `for i in 0..n_span { desc(head_page_idx + i).generation = dest_gen }`.
- **Large continuation page:** skip — it will be handled when the head is processed.

Determining liveness for large objects: since large objects are never forwarded, the BFS
does not naturally track whether a large object was visited. Add a lightweight per-large-
object visited flag using the mark bitmap: when `visit` encounters a live large object, set
the mark bit at its head cell. The page-reclaim loop checks this bit.

#### Mark / scan

In `mark.rs`, when the mark BFS encounters a pointer to a `PageKind::Large` head page:

1. Check `is_large_head()` on the desc of the pointed-to page.
2. Compute `total_cells = desc.n_span as usize * PAGE_SIZE_CELLS`. (The `header_layout` call
   already gives `total_cells` from the wrapper header for span-1 objects; for multi-page runs
   we use the page descriptor instead to avoid reading past the object boundary.)
3. Walk `pointer_cells_start..pointer_cells_end` as reported by `header_layout`. If those cells
   span a page boundary, the VA reservation from VM-0 guarantees the next page is contiguous —
   no special boundary handling needed.

In scanner/coordinator: the dirty-card scan must not stop at a page boundary for large objects.
The card range covers `n_span × PAGE_SIZE_BYTES` bytes starting at the head page.

#### Caller API addition

```rust
impl<L: HeapLayout> PageHeap<L> {
    /// True if the cell at `ptr` is the start of a large-object run.
    pub fn is_large_object(&self, ptr: *const u8) -> bool;

    /// For a large object at `ptr`, returns the number of pages in its run.
    pub fn large_object_span(&self, ptr: *const u8) -> usize;
}
```

#### Tests (VM-1, 8 tests)

| Test | What it checks |
|------|---------------|
| `vm1_alloc_large_single_boundary` | Object of exactly PAGE_SIZE_CELLS + 1 cells allocates across 2 pages |
| `vm1_alloc_large_multi_page` | 30,000-cell object spans 4 pages; head has n_span=4; continuations have n_span=0 |
| `vm1_large_object_layout_correct` | header_layout + n_span give consistent total_cells for a 5,000-slot vector |
| `vm1_large_object_survives_minor_gc` | Rooted large object in G0 survives 3 minor cycles unchanged |
| `vm1_large_object_generation_flips` | On G0→G1 promotion cycle, large object's entire run flips to G1 |
| `vm1_large_object_reclaimed_when_unrooted` | Unrooted large object's pages return to Free after minor GC |
| `vm1_large_and_small_coexist` | Large object in G0 doesn't interfere with small boxed/cons allocation in same generation |
| `vm1_table_buckets_size_24000_cells` | Simulates a 5,000-key Dylan table bucket array; allocation + 1 minor GC completes without panic |

---

## Sprint VM-2 — Tenured-generation collection (`collect_full`)

### Problem

`collect_major` (and `collect_minor`'s cascade path) both promote objects toward Tenured but
never reclaim Tenured objects. Promoted function IR, DFM, sealing facts, and cached
compilation results accumulate without bound. The `stress_mixed_workload` test already had to
weaken its reclaim assertion because of this.

### Solution

Add `collect_full`, which forces all live objects to Tenured and then compacts Tenured using
only the explicit mutator roots.

#### Key invariant

After force-promoting G0 → G1 → Tenured (ignoring age thresholds), **the entire live heap is
in Tenured**. No G0 or G1 objects remain. Therefore, the only external references to Tenured
objects are the caller's explicit root set. Tenured → Tenured evacuation with those roots is
**correct and complete** — no cross-generation card scan is needed for pass 3.

This invariant is what makes the implementation clean: we do not need to scan G0/G1 as
additional roots because they are empty after the first two passes.

#### New type

```rust
/// Summary of a `collect_full` cycle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FullCollectResult {
    /// Pass 1: G0 → G1 (forced, ignoring minors_since_g0_promote threshold).
    pub g0_evac:      EvacResult,
    /// Pass 2: G1 → Tenured (forced).
    pub g1_evac:      EvacResult,
    /// Pass 3: Tenured → Tenured (compact; explicit roots only).
    pub tenured_evac: EvacResult,
    /// Bytes freed from Tenured (= tenured_evac.pages_freed * PAGE_SIZE_BYTES, approximately).
    pub tenured_freed_bytes: usize,
}
```

#### Implementation

```rust
impl<L: HeapLayout> PageHeap<L> {
    /// Full stop-the-world collection: promote all young objects to
    /// Tenured, then compact Tenured using only the caller's explicit
    /// roots.
    ///
    /// This is the correct "full GC" for long-lived REPL / IDE sessions.
    /// It reclaims dead objects in all three generations, including
    /// Tenured. Use `collect_minor` / `collect_major` for routine
    /// cycle-based collection; call `collect_full` when the session
    /// needs a clean slate or when Tenured occupancy crosses a threshold.
    pub fn collect_full<F>(&mut self, mut visit_roots: F) -> FullCollectResult
    where
        F: FnMut(&mut PageEvacuator<'_, L>),
```

**Pass 1 — G0 → G1 (forced):**

```rust
let cards_arc = Arc::clone(&self.cards);
let heap_base = self.base_ptr() as *mut u64;
let heap_cells = self.reserved_bytes() / 8;
let descs_snap = self.descs().to_vec();

let g0_evac = self.evacuate_with_roots(Generation::G0, Generation::G1, |e| {
    visit_roots(e);
    scan_dirty_cards_as_roots(e, &cards_arc, heap_base, heap_cells, Some(&descs_snap));
});
```

Forced means `dest = G1` unconditionally, regardless of `minors_since_g0_promote`. The
counter is reset to 0 after `collect_full` completes.

**Pass 2 — G1 → Tenured (forced):**

```rust
let descs_after_g0: Vec<PageDesc> = self.descs().to_vec();
let g1_evac = self.evacuate_with_roots(Generation::G1, Generation::Tenured, |e| {
    visit_roots(e);
    scan_dirty_cards_as_roots(e, &cards_arc, heap_base, heap_cells, Some(&descs_after_g0));
});
```

After pass 2, G0 and G1 are empty.

**Pass 3 — Tenured → Tenured (compact):**

```rust
let tenured_evac = self.evacuate_with_roots(Generation::Tenured, Generation::Tenured, |e| {
    visit_roots(e);
    // No card scan needed: G0 and G1 are empty after passes 1 and 2.
});
```

No dirty-card scan in pass 3. The card table is rebuilt from the post-pass-3 heap state.

**Counter reset and card rebuild:**

```rust
self.rebuild_cards_for_old_gens();
self.minors_since_g0_promote = 0;
self.g0_promotes_since_g1_promote = 0;

let tenured_freed_bytes = tenured_evac.pages_freed * PAGE_SIZE_BYTES;
FullCollectResult { g0_evac, g1_evac, tenured_evac, tenured_freed_bytes }
```

#### Existing `collect_major` is unchanged

`collect_major` remains as the "promote everything young, clean up G0" hammer. Its docstring
is updated to note explicitly: *"Tenured objects are not reclaimed. Call `collect_full` for
full-heap reclamation."* Both are valid call sites; the Dylan runtime uses `collect_major` for
its routine threshold-triggered cycles and `collect_full` for explicit full-GC requests and
end-of-compilation cleanup.

#### `GcStats` addition

```rust
pub struct GcStats {
    // ... existing fields ...
    pub last_full_collect_tenured_freed_bytes: usize,
    pub last_full_collect_tenured_evac_objects: usize,
}
```

Updated by `collect_full` before returning.

#### Trigger guidance for Dylan runtime

Call `collect_full` when:
- `stats.tenured_used_bytes / stats.reserved_bytes > 0.70` (Tenured fill threshold, default 70%)
- Explicit user request (`(gc)` in the REPL, `force-gc()` in the IDE)
- End of a compilation unit (before starting the next, to reclaim dead IR)

The existing `should_collect_major()` heuristic (Tenured-full threshold in basis points) is
a reasonable proxy for triggering `collect_full` in the absence of explicit requests.

#### Tests (VM-2, 7 tests)

| Test | What it checks |
|------|---------------|
| `vm2_tenured_garbage_reclaimed_on_full_collect` | Objects promoted to Tenured, then dropped; `collect_full` reclaims their pages |
| `vm2_rooted_tenured_objects_survive_full_collect` | Rooted Tenured objects keep their addresses and values through a `collect_full` |
| `vm2_full_collect_resets_both_promotion_counters` | `minors_since_g0_promote` and `g0_promotes_since_g1_promote` are both 0 after `collect_full` |
| `vm2_full_collect_on_empty_heap_is_noop` | No pages, no roots → `FullCollectResult` all-zero, no panic |
| `vm2_g0_and_g1_empty_after_full_collect` | After `collect_full`, `count_pages_in_gen(G0)` and `count_pages_in_gen(G1)` are both 0 |
| `vm2_repl_session_no_tenured_leak` | 10 rounds of: allocate 100 boxed objects → promote to Tenured → drop all → `collect_full`; Tenured page count stays bounded |
| `vm2_stress_mixed_workload_reclaim_assertion_restored` | The workload from the existing `stress_mixed_workload` test but with the weakened assertion restored to ≥ 95% reclaim |

---

## Sprint VM-3 — `MEM_WRITE_WATCH` write barrier (future perf)

**Status:** Planned, not blocking Dylan correctness. Target: post-VM-2.

### Problem

The current write barrier is a per-store software call:

```rust
// In the Dylan JIT, every pointer store into old-gen:
heap.mark_card_at(slot_addr);  // → AtomicU8::store(Relaxed)
```

For workloads with many cross-generational writes (inserting into a large `<table>` in Tenured,
updating old-gen vector slots), this adds one atomic store per pointer write. At 200 K writes/s,
that's 200 K atomic stores/s on the write barrier path alone.

### Solution

Allocate the heap with `MEM_WRITE_WATCH`. The Windows kernel hardware-tracks which 4 KB pages
have been written since the last `GetWriteWatch(WRITE_WATCH_FLAG_RESET)` call. At the start
of each minor GC, the GC queries the dirty pages in one batch call — zero mutator overhead.

#### Construction change

```rust
#[cfg(windows)]
let heap_base = VirtualAlloc(
    None,
    heap_bytes,
    MEM_RESERVE | MEM_WRITE_WATCH,
    PAGE_NOACCESS,
);
```

#### Minor GC integration

Replace `scan_dirty_cards_as_roots` with `scan_write_watch_as_roots`:

```rust
fn scan_write_watch_as_roots<L: HeapLayout>(
    evac:        &mut PageEvacuator<'_, L>,
    heap_base:   *mut u8,
    heap_bytes:  usize,
    descs:       &[PageDesc],
    from_gen:    Generation,
) {
    let mut pages: Vec<*mut u8> = vec![null_mut(); 65536];
    let mut count = pages.len() as u64;
    let mut page_granularity: u32 = 0;
    unsafe {
        GetWriteWatch(
            WRITE_WATCH_FLAG_RESET,
            heap_base as *mut _,
            heap_bytes,
            pages.as_mut_ptr() as *mut *mut _,
            &mut count,
            &mut page_granularity,
        );
    }
    for &page_addr in &pages[..count as usize] {
        let page_idx = (page_addr as usize - heap_base as usize) / PAGE_SIZE_BYTES;
        if descs[page_idx].generation > from_gen {
            // This 4 KB OS page (may span multiple GC pages) is dirty and old-gen.
            // Scan all cells in the 4 KB window for cross-gen pointers.
            scan_4kb_window_as_roots(evac, page_addr, from_gen);
        }
    }
}
```

The `CardTable` field and `mark_card_at` method remain in `PageHeap` as stubs (they become
no-ops under `MEM_WRITE_WATCH`). This preserves ABI compatibility with the coordinator API.

#### Granularity tradeoff

| Dimension | Card table | `MEM_WRITE_WATCH` |
|---|---|---|
| Granularity | 512 bytes (64 cells) | 4 KB (512 cells) — 8× coarser |
| Mutator cost | 1 atomic store per pointer write | 0 |
| GC scan cost | O(dirty cards × 64 cells) | O(dirty pages × 512 cells) |
| False positives | Low (software tracking) | Higher (any write to the page, not just pointer writes) |

For Dylan: the primary write pattern is sequential fills of fresh vectors and occasional
table insertions into old-gen. False positives are low (most writes are into G0, which
`GetWriteWatch` still returns but the gen-filter discards). The 8× coarser scan is
offset by removing the per-store barrier.

Whether `MEM_WRITE_WATCH` is a net win depends on workload. Gate it behind a feature flag:

```toml
[features]
write-watch = []   # Windows MEM_WRITE_WATCH; zero-cost write barrier + coarser GC scan
```

#### Tests (VM-3, 5 tests)

| Test | What it checks |
|------|---------------|
| `vm3_write_watch_detects_old_gen_write` | Writing a G0 pointer into a G1 slot; `GetWriteWatch` reports the G1 page dirty |
| `vm3_write_watch_reset_clears_between_cycles` | After one minor GC, `GetWriteWatch` returns empty for unmodified pages |
| `vm3_write_watch_cross_gen_pointer_not_lost` | G1 table with G0 value; minor GC without explicit root still keeps the G0 value alive |
| `vm3_write_watch_false_positive_is_safe` | Writing a fixnum (non-pointer) into old-gen; false-positive scan does not corrupt |
| `vm3_write_watch_vs_card_table_equivalent` | Same workload under both implementations; rooted-object survival count matches |

---

## Dependency graph

```
Dylan requirements
        │
        ▼
  ┌─────────────────────────────────────────────┐
  │  VM-0: Lazy metadata + 32 GB VA reservation │  ← prerequisite for VM-1; eliminates heap-growth gap
  └──────────────────────────┬──────────────────┘
                             │
             ┌───────────────┼───────────────┐
             ▼               ▼               ▼
       ┌───────────┐   ┌───────────┐   ┌───────────┐
       │   VM-1    │   │   VM-2    │   │   VM-3    │
       │ Large-obj │   │ collect_  │   │ WriteWatch│
       │ alloc     │   │ full      │   │ (perf)    │
       └───────────┘   └───────────┘   └───────────┘
       Gap 1 fixed     Gap 2 fixed     Future perf
```

VM-1 and VM-2 are independent once VM-0 lands; they can be developed in parallel.
VM-3 requires VM-0 (needs `MEM_WRITE_WATCH` on the heap backing).

---

## Dylan gap resolution summary

| Gap | Description | Resolution |
|-----|-------------|-----------|
| Gap 1 | Large vectors / tables exceed 8192-cell cap | VM-1 |
| Gap 2 | Tenured objects accumulate forever | VM-2 |
| Gap 3 | Heap growth on OOM | **Eliminated by VM-0** (32 GB lazy reservation) |
| Future | Multi-mutator threading | THREADING.md roadmap, Sprint 28+ |
| Future | `MEM_WRITE_WATCH` write barrier perf | VM-3 |
| Future | Weak refs, finalizers | Sprint 30+ |

---

## Files affected per sprint

| Sprint | Files changed |
|--------|--------------|
| VM-0 | `space.rs` (Backing, commit_page, decommit_page, with_reservation), `alloc.rs` (VmStartBits, VmMarkBits) |
| VM-1 | `page_desc.rs` (n_span field), `alloc.rs` (try_alloc_large), `evac.rs` (large-object pinning + run reclaim), `mark.rs` (cross-page walk), `scanner.rs` (large-object card range) |
| VM-2 | `cycle.rs` (collect_full, FullCollectResult), `space.rs` (GcStats additions) |
| VM-3 | `space.rs` (MEM_WRITE_WATCH in Backing, scan_write_watch_as_roots), `coordinator_api.rs` (mark_card_at stub), `Cargo.toml` (write-watch feature) |

---

*End of VMDylanSprint.md*
