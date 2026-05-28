# Write Barrier

## Purpose

A generational GC only collects young objects during a minor cycle. But young objects can be pointed to by old objects — if the GC only scans G0 pages, it would miss those cross-generation pointers and incorrectly reclaim live young objects.

The write barrier's job is to track every store of a young pointer into an old object, so the GC can find those cross-generation references without scanning the entire old-generation heap.

## Card table

NewGC uses a **software card table** for the write barrier.

The reservation is divided into cards of 512 bytes each (`CARD_SIZE_BYTES = 512`, which is 64 cells). The card table is a flat array of bytes — one byte per card, sized to cover the full reservation:

```rust
pub struct CardTable {
    bytes: Box<[AtomicU8]>,
}
```

For a 2 GB reservation: `2 GB / 512 = 4,194,304` cards → 4 MB of card table storage.

Card entries:
- `0` = clean (no known young pointer in this card)
- `1` = dirty (may contain a young pointer)

## Marking a card

The mutator calls `mark_card_at(slot_addr)` after **every** write of a heap pointer into a heap slot:

```rust
pub fn mark_card_at(&self, slot_addr: *const u8) {
    // computes byte_offset = slot_addr - base_ptr
    // card = byte_offset / CARD_SIZE_BYTES
    bytes[card].store(1, Ordering::Relaxed)
}
```

This is a single `u8` atomic store — the cheapest possible barrier. No CAS, no lock, no fence.

**False positives are safe.** A dirty card that turns out to contain no young pointer is harmless — the GC just rescans a card unnecessarily. **False negatives are fatal.** An unmarked card that actually contains a young pointer means the GC will miss a live young object and corrupt the heap.

## Mutator discipline

Every write of a heap pointer into a heap cell in a generation older than G0 must mark the card. In the mini-Lisp driver:

- `alloc_pair(car, cdr)` marks the card after writing the car and cdr cells.
- `vector_set!(v, i, val)` marks the card after writing the element.

Pure stack references (root slots in the call stack) do not need card marks — they are passed directly to the evacuator's root closure.

## Card persistence across cycles

Cards are **not** cleared after each minor cycle. A dirty card that survives a minor cycle remains dirty for the next cycle. This property is exploited by the major GC: cards written during any previous cycle are still available for the G0 and G1 passes without requiring the mutator to re-mark them.

After each collection pass, `rebuild_cards_for_old_gens()` reconstructs the card table from the actual post-evacuation heap state — because evacuation moves objects between pages, and the dirty bit on the old page does not transfer to the new page automatically.

## GC-side card scanning

During collection, `scan_dirty_cards_as_roots()` extracts cross-generation pointers from dirty cards and adds them to the evacuator's root set:

1. Snapshot page descriptors and the card table **before** the evacuation pass.
2. For each dirty card: check whether the card's page was in a generation **older** than the current from-generation at snapshot time.
3. For each cell in a qualifying dirty card: classify the cell with `HeapLayout::classify`. If it's a `PointerCons` or `PointerHeader` into `from_gen`, add its address as a root.
4. These roots are passed to `PageEvacuator::visit` just like explicit stack roots.

The snapshot ensures the filter targets pages that were in their correct generation at the start of the cycle — pages freshly promoted during the current pass are excluded.

## Integration with collection cycles

| Cycle | Card scan in `visit_roots` | After cycle |
|-------|--------------------------|-------------|
| `collect_minor` | Yes — snapshot before evacuation; scans Tenured/G1 cards for G0 pointers | `rebuild_cards_for_old_gens()` |
| `collect_major` pass 1 (G1→Tenured) | Yes — Tenured cards for G1 pointers | — |
| `collect_major` pass 2 (G0→G0) | Yes — Tenured+G1 cards for G0 pointers | `rebuild_cards_for_old_gens()` |
| `collect_full` passes 1 and 2 | Yes | — |
| `collect_full` pass 3 (Tenured→Tenured) | **No** — G0 and G1 are empty; explicit roots are sufficient | `rebuild_cards_for_old_gens()` |

---

Back to [Home](index.md).
