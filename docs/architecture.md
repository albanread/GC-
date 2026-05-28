# Architecture

## Overview

NewGC separates three concerns that most embedded GCs tangle together:

1. **Memory management** — reserving and committing virtual address space, tracking which pages belong to which generation, bumping allocation pointers
2. **Collection algorithms** — marking live objects, evacuating them to fresh pages, reclaiming emptied pages
3. **Language semantics** — what a tagged pointer looks like, how to decode an object header, which payload cells hold live pointers

Concern (3) is entirely behind the `HeapLayout` trait. The GC engine never inspects a tag or a header directly; it always calls through the trait. The result is a single codebase that correctly collects Lisp cons cells, Dylan instances, or any other tagged-pointer language, with zero runtime overhead — the trait is monomorphised, not `dyn`.

## Data flow

```
Mutator allocates
  └─ AllocRegion::alloc()       bump pointer in open page
       └─ acquire_free_page()   if page full: find/commit a free page
            └─ start_bits       set 2-bit start marker for new object

Trigger fires (should_collect() or explicit call)
  └─ collect_minor / collect_major / collect_full
       │
       ├─ evacuate_with_roots(from_gen, dest_gen, visit_roots)
       │    ├─ scan_dirty_cards_as_roots()    find cross-gen pointers
       │    ├─ mark BFS                        find all live objects
       │    └─ Cheney evacuation BFS
       │         ├─ copy object to dest page
       │         ├─ write forward marker at source
       │         └─ rewrite all pointer slots in copied object
       │
       └─ page reclamation
            ├─ pages with no pins → Generation::Free
            └─ pages with pins   → flip generation in place
```

## Module map

| Module | Responsibility |
|--------|----------------|
| `traits.rs` | `HeapLayout` trait, `WordKind`, `PointerKind`, `ObjectLayout` |
| `word.rs` | `Word` — 64-bit tagged value, all constructors and accessors |
| `heap_common.rs` | `HeapHeader`, `HeapType`, `GcBit`, `CardTable`, `StartBits` alias |
| `lisp_layout.rs` | Reference binding — NCL-style 3-bit Lisp tag scheme |
| `tiny_layout.rs` | Reference binding — minimal 2-bit tag scheme |
| `page_heap/space.rs` | `PageHeap<L>` struct; OS-level backing; commit/decommit; public APIs |
| `page_heap/page_desc.rs` | `PageDesc`, `Generation`, `PageKind` — 12-byte per-page metadata |
| `page_heap/alloc.rs` | `AllocRegion` — bump pointer, page acquisition, start-bit bitmap writes |
| `page_heap/mark.rs` | BFS mark pass — roots → object graph traversal using `HeapLayout::classify` |
| `page_heap/evac.rs` | `PageEvacuator` — Cheney BFS, forwarding pointers, pin handling, OOM |
| `page_heap/cycle.rs` | `collect_minor`, `collect_major`, `collect_full` — cohort promotion policy |
| `page_heap/pin.rs` | `pin_pointers_in_ranges` — conservative stack/register scan |
| `page_heap/scanner.rs` | `scan_dirty_cards_as_roots` — card-table extraction |
| `page_heap/coordinator_api.rs` | Legacy accessors; scheduled for removal |

## Key design principles

**Monomorphisation over dynamic dispatch.** `PageHeap<L>` is generic over `L: HeapLayout`. Every call to `L::classify`, `L::header_layout`, and `L::rewrite_pointer_addr` inlines at the call site. There is no vtable, no `dyn`, no allocation from the language binding.

**Stop-the-world.** All collection passes run with the mutator suspended. There is no concurrent marking or incremental update. This simplifies the algorithm significantly and eliminates the need for read barriers.

**Immovable pinning, not write protection.** Objects that cannot be moved (conservative stack roots) are left in place and their pages are flipped to the destination generation rather than freed. This avoids SIGSEGV-based remembered sets and the associated OS overhead.

**Cards persist between cycles.** The card table is never bulk-cleared. Cards are only cleared lazily during collection when the GC verifies the old-to-young pointer no longer exists. This means a major GC can reuse cards written during previous minor cycles without requiring the mutator to re-mark them.

**Pages are the unit of everything.** Allocation, collection, and reclamation all operate on whole 64 KB pages. There is no sub-page compaction; individual dead objects within a page do not reduce the page's generation-occupancy count (that is handled by evacuation copying live objects out).

## Dependencies

| Crate | Purpose | Feature-gated |
|-------|---------|---------------|
| `windows 0.62` | `VirtualAlloc` / `VirtualFree` for OS-backed reservation | `cfg(windows)` |
| `libc 0.2` | `mmap` / `mprotect` / `madvise` for OS-backed reservation | `cfg(unix)` |

No other runtime dependencies. The `newgc-test-lisp` driver adds no GC dependencies of its own.

## `PageHeap<L>` — top-level struct fields

The complete heap state lives in a single `PageHeap<L>` value. Key fields:

| Field | Type | Purpose |
|-------|------|---------|
| `storage` | `Backing` | OS reservation (Windows/Unix/Box fallback) |
| `n_pages` | `usize` | Total pages in reservation |
| `committed_bits` | `Vec<AtomicU64>` | Per-page commit state, lock-free reads |
| `descs` | `Vec<PageDesc>` | Parallel metadata table, 12 bytes per page |
| `alloc_regions` | `[[AllocRegion; 2]; 3]` | 6 open bump-pointer regions (3 gens × 2 kinds) |
| `start_bits` | `StartBits` (= `Arc<[AtomicU64]>`) | 2-bit start marker, global, 32 cells per word |
| `mark_bits` | `Box<[u64]>` | 1-bit mark, 64 cells per word |
| `cards` | `Arc<CardTable>` | Soft write-barrier card table |
| `pinned_cells` | `HashSet<usize>` | Conservative pin targets (cell indices) |
| `minors_since_g0_promote` | `u32` | Cohort promotion counter for G0→G1 |
| `g0_promotes_since_g1_promote` | `u32` | Cohort promotion counter for G1→Tenured |
| `bytes_alloc_since_gc` | `usize` | Allocation counter for trigger policy |
| `auto_gc_trigger_bytes` | `usize` | Threshold for `should_collect()` |
| `poisoned` | `bool` | Set after mid-evacuation OOM; prevents further collections |

---

Back to [Home](index.md).
