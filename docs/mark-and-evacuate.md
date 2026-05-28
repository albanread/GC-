# Mark and Evacuate

## Overview

NewGC uses a **mark-then-evacuate** strategy. The mark pass finds all live objects reachable from roots. The evacuation pass copies those objects to a new location, rewrites all pointer slots, and releases the original pages.

These two passes are separate because the evacuation BFS needs to know which objects are pinned (cannot be moved). The mark pass produces the live set; the pin scanner produces the pinned set; the evacuator uses both.

## Mark pass

The mark pass is a breadth-first traversal starting from the root set.

**Input:** a set of root `Word`s and the target generation.  
**Output:** `mark_bits` populated for all cells reachable from roots in the target generation.

### Five-gate check

Before marking a cell, five gates are applied in order. Any failure aborts the mark of that cell (it is not followed):

1. **Tag classification** — `HeapLayout::classify(raw)` returns `WordKind`. Immediates (`Fixnum`, `NIL`, `T`, etc.) pass through immediately — nothing to follow. `Forwarded` cells indicate a bug (a forward marker should not appear during a fresh mark pass).
2. **Reservation boundary** — the address must fall within `[base, base + reserved_bytes)`.
3. **Generation match** — the page containing the address must belong to `target_gen`. Pointers out of the target generation are valid live references but are not followed by this pass.
4. **Page kind** — the page's `PageKind` must not be `Free`.
5. **Start-bit consistency** — a `PointerCons` must land on a `11` start bit; a `PointerHeader` must land on a `01` start bit. Mismatches reject the candidate as a non-pointer.

Passing all five gates: the cell is marked in `mark_bits` and pushed onto the BFS queue.

### Traversal

For each cell dequeued:
- If **cons-shaped** (`11` start bit): follow both the `car` cell and the `cdr` cell as Words, applying the five-gate check to each.
- If **boxed** (`01` start bit): call `HeapLayout::header_layout(header_cell)` to decode the `ObjectLayout`, then follow cells in `[pointer_cells_start, pointer_cells_end)` as Words.

Cells outside the pointer range (raw floats, bignum limbs, string bytes) are skipped.

## Evacuation

The evacuation pass is a **Cheney-style BFS copy**. Live objects from `from_gen` are copied to `dest_gen`. The source pages are then released.

### Phase 1 — BFS copy

For each root slot `*slot`:
1. Classify the raw value via `HeapLayout::classify`.
2. If immediate: skip.
3. If `Forwarded(new_addr)`: rewrite `*slot` to `rewrite_pointer_addr(old_raw, new_addr)` — object already moved; just update the pointer.
4. If `PointerCons` or `PointerHeader`:
   a. Apply the five-gate check. Fail → leave slot unchanged.
   b. Check if the cell is pinned (see below). Yes → skip copy, leave in place.
   c. Allocate `n` cells in `dest_gen` of the matching kind.
   d. `memcpy` the object to the new address.
   e. Write `Word::forward(new_addr)` into the first cell of the **source** (overwriting the original header or car).
   f. Rewrite `*slot` to `rewrite_pointer_addr(old_raw, new_addr)`.
   g. Push new address onto BFS queue.

For each copied object dequeued:
- Walk its pointer cells at the **new** address.
- For each pointer slot: apply the same logic as above (follow forwards, copy not-yet-copied objects).

### Phase 2 — Rewrite stale roots

After the BFS completes, rescan the root set. Any slot pointing into `from_gen` now either:
- Points at a pinned object (unchanged, leave it).
- Points at a source cell containing a forward marker (follow and rewrite).

This second pass is necessary because roots can point at objects that were first reached and forwarded by another root's BFS path, not by the direct root scan.

### Pinned objects

An object is pinned if its cell index is in `pinned_cells` (the conservative pin set). The evacuator checks this before deciding to copy:

1. Fast filter: check the source page's `pin_byte`. If the relevant bit is zero, the object cannot be pinned — skip the hashtable lookup.
2. Full check: look up the cell index in `pinned_cells`.

If pinned: the object is not copied. Its page's generation is **flipped** to `dest_gen` at the end of the pass rather than released. This counts as `pages_flipped` in `EvacResult`.

### Forwarding marker encoding

```
Forward tag: 0b111  (Tag::Forward)
Encoding: new_addr | 0b111
Recovery: raw & PAYLOAD_MASK
```

The forward marker is written into the first cell of the moved object at its old address. Any subsequent visitor that reads this cell gets `WordKind::Forwarded(new_addr)` from `HeapLayout::classify` and follows the chain.

Forward markers are cleaned up during page reclamation: when the source page's start bits are zeroed, the markers become invisible (the GC will not find them via the start-bit gates on subsequent cycles).

### Mid-evacuation page recycling

During the BFS, the evacuator tracks a live-object count per source page in `recycle_live_counts`. When a page's count reaches zero during the BFS (all its live objects have been copied out), the page can be reclaimed **immediately** without waiting for the pass to complete. This shortens pause times for heaps with large transient data.

### OOM handling

If `acquire_free_page()` finds no Free pages available during evacuation, a `GcStallError` is raised. In the non-`try_collect_*` variants this is a panic. In the `try_collect_*` variants it is caught and returned as `GcError::MidEvacOom`. After OOM, the heap is **poisoned** — the `poisoned` flag is set and subsequent allocation calls refuse with a panic. Only `Drop` is safe on a poisoned heap.

---

Back to [Home](index.md).
