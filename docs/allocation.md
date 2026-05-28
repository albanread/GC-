# Allocation

## AllocRegion

The heap maintains six open allocation regions, one per `(Generation, PageKind)` pair:

```
alloc_regions[gen][kind]

gen  0 = G0        kind 0 = Cons
     1 = G1              1 = Boxed
     2 = Tenured
```

Each `AllocRegion` tracks one currently-open page:

| Field | Type | Purpose |
|-------|------|---------|
| `current_page` | `usize` | Index of the open page (`usize::MAX` = none open) |
| `offset` | `usize` | Next free cell within the page (0..=8192) |

Allocation and deallocation of regions is the hot path; everything else is a slow-path page acquisition.

## Fast path

To allocate `n` cells from `(gen, kind)`:

1. Check `AllocRegion::remaining_cells()` — `PAGE_SIZE_CELLS - offset`.
2. If `n <= remaining`: return `base + offset * 8`, advance `offset += n`.
3. If the page is full: call `acquire_free_page()`.

The fast path is a single comparison and an integer increment — no locks, no atomics, no system calls.

## Page acquisition

When the open page is exhausted, `acquire_free_page()`:

1. Scans `descs` linearly for a page with `generation == Free`.
2. Commits the page if not already committed (OS call).
3. **Clears start bits** for the new page — any `01`/`11` markers from a prior tenant must be erased before the new tenant writes its own.
4. **Zeroes heap cells** — forwarding markers left by a prior collection cycle must not be visible to a fresh allocator.
5. Sets `PageDesc::fresh(gen, kind)` on the descriptor.
6. Opens the `AllocRegion` pointing at the new page with `offset = 0`.

Steps 3 and 4 are correctness-critical bug fixes (start-bit contamination and stale forward markers were real bugs in earlier versions).

## Start-bit bitmap

Every allocated object has a 2-bit entry in the global start-bit bitmap. The bitmap covers the entire reservation — one `Arc<[AtomicU64]>` shared by all allocation paths.

**Encoding** — 2 bits per cell, 32 cells per `u64` word:

| Pair | Meaning |
|------|---------|
| `00` | Not an object start |
| `01` | Start of a boxed (header-bearing) object |
| `11` | Start of a cons cell |

The allocator sets bits via `fetch_or` on the appropriate word — this is atomic and requires no lock.

**Why two bits instead of one?** The GC must distinguish a cons start from a boxed start without reading the tag on the pointer. When following a cons-tagged pointer, the GC checks the start bit is `11`; for a header-tagged pointer it checks for `01`. A single bit can only say "is a start" — it cannot say "what kind".

**Clearing** — when a page is reclaimed or reused, its start-bit range is zeroed. This is done word-by-word covering the page's cell range. Without this step, stale `01`/`11` bits from old tenants would cause the mark/evac gates to accept or reject objects at wrong addresses.

## Cons allocation

Cons cells are always 2 cells wide. The allocator returns a pointer to cell 0 (the `car`); cell 1 (the `cdr`) is the adjacent cell. The start bit for cell 0 is set to `11`; cell 1 has no start bit (`00`).

The tag on a cons pointer is `Tag::Cons` (`001`). The GC's consistency gate verifies that a `Cons`-tagged pointer lands on a `11` start-bit cell.

## Boxed object allocation

For a boxed object of `n` payload cells, `n + 1` cells are allocated (header + payload). Cell 0 receives the `HeapHeader`; payload cells are either zero-filled or filled with `FILL_WORD`. The start bit for cell 0 is set to `01`.

## Allocation counter

Every successful allocation increments `bytes_alloc_since_gc`. This counter is checked by `should_collect()` against `auto_gc_trigger_bytes` to decide when a collection is due. See [GC Cycles — Trigger Policy](gc-cycles.md) for details.

---

Back to [Home](index.md).
