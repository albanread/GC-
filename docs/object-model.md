# Object Model

## Two shapes of heap object

NewGC recognises two categories of heap object, distinguished by the tag on the pointer that points at them and by the start-bit marker on the first cell:

**Cons-shaped** — two adjacent 8-byte cells with no header. The first cell is the `car`, the second is the `cdr`. Both cells are `Word`s. The GC follows both. Pointers to cons cells carry the `Tag::Cons` tag and land on a `11` start bit.

**Boxed** — a variable-length sequence of cells beginning with a `HeapHeader` at offset 0. Pointers carry one of the header-bearing tags (`Symbol`, `Vector`, `Function`, `String`) and land on a `01` start bit. The header encodes the object's type, length, and GC flags.

## HeapHeader

```rust
#[repr(transparent)]
pub struct HeapHeader(u64);
```

One 64-bit word. Three fields packed into it:

```
bits 0–4   TYPE   (5 bits)   HeapType enum — 10 variants, values 0–9
bits 5–28  LEN    (24 bits)  payload length in cells (excludes header)
bits 29–36 GC     (8 bits)   GcBit flags
bits 37–63 —      (27 bits)  reserved / zero
```

Maximum object size: `LEN_MAX = (1 << 24) - 1 = 16,777,215 cells = ~128 MB`.

## HeapType

| Variant | Value | Payload shape |
|---------|-------|--------------|
| `Symbol` | 0 | 7 cells, all `Word`s |
| `Vector` | 1 | `len` cells, all `Word`s |
| `Function` | 2 | cells 1–2 opaque (code pointer, arity); cells 3–4 are `Word`s |
| `String` | 3 | opaque UTF-8 bytes; no `Word` cells |
| `FfiBlock` | 4 | opaque foreign data; no `Word` cells |
| `Other` | 5 | treated conservatively — all payload cells are `Word`s |
| `Bignum` | 6 | cells 1–4 are `Word`s; cells 5+ are raw `u64` limbs (little-endian) |
| `Float` | 7 | cell 1 is a `Word` (marker symbol); cell 2 is raw `f64` bits |
| `Ratio` | 8 | 3 cells — marker `Word`, numerator `Word`, denominator `Word` |
| `Complex` | 9 | 3 cells — marker `Word`, real part `Word`, imaginary part `Word` |

The `word_field_range(length_cells)` method returns the inclusive `(first, last)` offsets of pointer-bearing cells for a given type. Cells outside this range are opaque to the GC — it will not classify them and will not follow them, avoiding false-positive pointer chasing on raw floats, bignum limbs, or string bytes.

| Type | Word-cell range | Non-`Word` cells |
|------|----------------|-----------------|
| `Vector`, `Symbol`, `Ratio`, `Complex`, `Other` | `1..=len` | — |
| `Function` | `3..=4` | code pointer (1), arity (2) |
| `Bignum` | `1..=4` | limbs (5..len) |
| `Float` | `1..=1` | f64 bits (2) |
| `String`, `FfiBlock` | none | all cells opaque |

## GcBit flags

Three flags live in the header's GC field. They are set and cleared by the GC, never by user code.

| Flag | Meaning |
|------|---------|
| `GcBit::Mark` | Object was reached during the current mark pass |
| `GcBit::Tenured` | Object is in the Tenured generation (informational) |
| `GcBit::Pinned` | Object is a conservative pin — cannot be moved this cycle |

## ObjectLayout

The `ObjectLayout` struct is returned by `HeapLayout::header_layout` and tells the GC how to walk a specific object instance:

```rust
pub struct ObjectLayout {
    pub total_cells: usize,        // including the header cell
    pub pointer_cells_start: usize, // inclusive offset of first Word cell
    pub pointer_cells_end: usize,   // exclusive offset
}
```

The GC only follows cells in `[pointer_cells_start, pointer_cells_end)`. A range where `start == end` means no pointer cells (used for `String`, `FfiBlock`).

Two convenience constructors:

```rust
ObjectLayout::opaque(total_cells)       // no pointer cells
ObjectLayout::all_pointers(total_cells) // all payload cells are Words (start=1, end=total)
```

## Object lifecycle

1. **Allocation** — `HeapHeader` is written at cell 0; payload cells are zero-filled (or filled with `FILL_WORD`); start bit `01` is set for the header cell.
2. **Mutation** — user code reads/writes payload cells; card table marked if writing a pointer into an old-generation object.
3. **Mark pass** — header's `GcBit::Mark` is set when the object is reached from roots.
4. **Evacuation** — if the object is not pinned, it is copied to a fresh page; `Tag::Forward` is written at the source's first cell pointing to the new location.
5. **Reclamation** — source page start bits are cleared; page is released to Free if no pins remain.

---

Back to [Home](index.md).
