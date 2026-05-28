# Conservative Pinning

## Why pinning is needed

A precise GC knows the exact location of every live heap pointer — it can move any object freely. But in practice, pointers also live in:

- Machine registers at the moment of the collection safepoint
- Stack frames of compiled or interpreted functions
- `unsafe` Rust code holding raw `*const T` pointers into the heap

NewGC cannot safely move an object whose address is held in one of these locations — if it did, the raw pointer would become dangling. The solution is **conservative pinning**: treat any word-sized value that looks like it could be a heap pointer as a pin, and leave the pointed-at object in place during evacuation.

Conservative pinning is an optional feature. It is compiled in by default (`conservative-pin` Cargo feature) but can be disabled for embedders that only use precise root enumeration.

## `pin_pointers_in_ranges`

Before a collection cycle, the embedder calls:

```rust
heap.pin_pointers_in_ranges(&[(start1, end1), (start2, end2), ...]);
```

The ranges typically cover the current thread's stack (`RSP..stack_top`) and any saved-register areas. The function scans every `u64`-aligned word in the ranges and applies the same **five-gate check** used by the mark pass:

1. **Tag classification** — must be `PointerCons` or `PointerHeader` (immediates and fixnums rejected).
2. **Reservation boundary** — address must fall in `[base, base + reserved_bytes)`.
3. **Page kind** — page must not be `Free`.
4. **Generation** — the page must be in G0 (only G0 objects need pinning for a minor cycle; Tenured objects never move during minor cycles anyway).
5. **Start-bit consistency** — cons pointer must land on `11`, header pointer on `01`.

Passing all five gates: the cell is added to `pinned_cells` (a `HashSet<usize>` keyed on cell index) and the page's `pin_byte` is updated.

## Dual-level index

The pin check during evacuation uses a two-level lookup to avoid hammering the `HashSet`:

**Level 1 — page-level `pin_byte`:** Eight bits, one per 8 KB sub-region of the 64 KB page. The bit is set if any pinned object falls in that sub-region. If the page's `pin_byte` is zero, no objects on that page are pinned — the HashSet lookup is skipped entirely.

**Level 2 — `pinned_cells` HashSet:** Cell-index → bool. Only consulted when `pin_byte` indicates the page has at least one pin.

For a typical minor cycle where almost no objects are pinned, the vast majority of evacuation decisions are resolved at level 1 with a single byte comparison.

## Evacuation behaviour for pinned objects

When the evacuator encounters a pinned object at address `addr`:

- The object is **not copied** to the destination generation.
- The slot pointing at it is **left unchanged**.
- At the end of the pass, the source page's `PageDesc::generation` is flipped to `dest_gen`.

The pinned object now lives in `dest_gen` at its original address. All pointer slots that referenced it still point to the correct address — no rewrite needed. The cost is that the pinned object's page is promoted as a whole unit, even if the rest of its contents are dead.

This strategy is sometimes called **"pin in place, flip the page"**. The `EvacResult::pages_flipped` counter tracks how many pages were handled this way.

## Clearing pins

Pins are cleared at the start of each collection cycle. The `pinned_cells` HashSet is emptied, and all `pin_byte` fields in `descs` are zeroed. The embedder must call `pin_pointers_in_ranges` again before the next collection to re-establish any pins.

## Interaction with `collect_full`

`collect_full` does **not** preserve pin state across its three internal evacuation passes. Each pass ends with `clear_all_pins`. If the embedder needs Tenured objects to be pinned during the Tenured→Tenured pass (pass 3), it must call `pin_pointers_in_ranges` between pass 2 and pass 3 — which is not possible via the current public API. In practice, `collect_full` callers that rely on conservative pinning must supply every live Tenured object through the explicit-root closure and not depend on pinning for pass 3 correctness.

## Compiling out

If the embedder uses only precise root enumeration and never calls `pin_pointers_in_ranges`, compile with `--no-default-features` to exclude the conservative-pin code entirely. The `pinned_cells` HashSet, `pin_byte` population, and the dual-level pin check in the evacuator are all removed. The `PageDesc::pin_byte` field and `pin_byte`-related `PageDesc` methods remain, but their values are always zero.

---

Back to [Home](index.md).
