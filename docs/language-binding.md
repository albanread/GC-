# Language Binding

## The HeapLayout trait

Every language-specific operation in NewGC is hidden behind `HeapLayout`. The GC engine never inspects a tag directly; it always calls through this trait. Because `PageHeap<L>` is generic over `L: HeapLayout`, the compiler monomorphises all trait calls at their call sites — the hot mark and evacuation paths incur zero dynamic dispatch overhead.

```rust
pub trait HeapLayout: Copy + Clone + Debug + Default + 'static {
    const FILL_WORD: u64;
    fn classify(raw: u64) -> WordKind;
    fn make_forward(new_addr: *const u8) -> u64;
    fn make_pointer(addr: *const u8, kind: PointerKind) -> u64;
    fn rewrite_pointer_addr(old_raw: u64, new_addr: *const u8) -> u64;
    unsafe fn header_layout(header_cell: *const u64) -> ObjectLayout;
}
```

Implementors are zero-sized marker types (e.g., `pub struct LispLayout;`). All methods are free functions — the trait holds no instance data.

**Inlining contract:** implementations must be small (5–20 instructions). The GC calls `classify` on every cell read during mark/evac. A 200-branch `classify` would dominate the pause time. If a tag scheme is complex, consider dispatching on the high bits first and grouping rare tags.

## Methods

### `FILL_WORD: u64`

The value written into freshly allocated payload cells. Should be the language's nil/false/null so reads of uninitialised slots return something harmless rather than a stale pointer that might accidentally pass the five-gate check.

### `classify(raw: u64) -> WordKind`

The heart of the trait. Decodes a raw 64-bit cell value and tells the GC what to do with it:

| `WordKind` variant | GC action |
|-------------------|----------|
| `Immediate` | Leave untouched — fixnum, char, boolean, nil, etc. |
| `PointerCons(*const u8)` | Pointer to a headerless 2-cell pair; target page must be `Cons` |
| `PointerHeader(*const u8)` | Pointer to a header-bearing object; target page must be `Boxed` |
| `Forwarded(*const u8)` | GC-internal forwarding marker; follow the chain |

`classify` is called on **every** cell during mark/evac traversal. It is the safety boundary between "leave this alone" and "follow this pointer." It must be correct and fast.

### `make_forward(new_addr) -> u64`

Encode a forwarding marker for the given new address. The GC writes this into the first cell of a moved object. Subsequent calls to `classify` on that cell must return `Forwarded(new_addr)`.

### `make_pointer(addr, kind) -> u64`

Encode a tagged heap pointer. Used in tests and debug paths. `kind` is either `PointerKind::Cons` or `PointerKind::Header`.

### `rewrite_pointer_addr(old_raw, new_addr) -> u64`

The production pointer-rewrite path used during evacuation. Given the original raw bits and the new address, produce the rewritten bits while **preserving all language-specific tag bits** the GC does not know about.

This is subtly different from `make_pointer`. `make_pointer` would collapse a Symbol, Vector, Function, or String pointer all down to `PointerHeader` — losing the specific tag. `rewrite_pointer_addr` keeps the original tag and just swaps the address bits.

Example for the Lisp binding:

```rust
fn rewrite_pointer_addr(old_raw: u64, new_addr: *const u8) -> u64 {
    (old_raw & TAG_MASK) | (new_addr as u64)
}
```

The tag bits are preserved; only the address payload is updated.

### `header_layout(header_cell: *const u64) -> ObjectLayout`

Called when the GC encounters a `01` start bit and needs to know:
- How many cells to skip past (`total_cells`)
- Which of those cells are pointer-bearing (`pointer_cells_start..pointer_cells_end`)

**Safety:** `header_cell` is guaranteed to point at a valid header cell (the start-bit gate has already verified this). The implementation can read `*header_cell` without further checking.

## Reference implementation: LispLayout

`LispLayout` implements the full NCL-style 3-bit Lisp tag scheme:

| Tag bits | `WordKind` returned |
|----------|---------------------|
| `000` Fixnum | `Immediate` |
| `001` Cons | `PointerCons(addr)` |
| `010` Symbol | `PointerHeader(addr)` |
| `011` Vector | `PointerHeader(addr)` |
| `100` Function | `PointerHeader(addr)` |
| `101` String | `PointerHeader(addr)` |
| `110` Immediate (nil, T, char, unbound) | `Immediate` |
| `111` Forward | `Forwarded(addr)` |

`header_layout` decodes the `HeapHeader` at the given address, reads the `HeapType` and `length_cells`, and calls `HeapType::word_field_range(length_cells)` to compute the pointer-cell range. Objects with no pointer cells (String, FfiBlock) return `ObjectLayout::opaque(total)`.

`rewrite_pointer_addr` preserves the original 3-bit tag:

```rust
fn rewrite_pointer_addr(old_raw: u64, new_addr: *const u8) -> u64 {
    (old_raw & 0b111) | (new_addr as u64)
}
```

## Reference implementation: TinyLayout

`TinyLayout` is a minimal 2-bit tag scheme for testing that `HeapLayout` is genuinely polymorphic:

| Tag bits | `WordKind` returned |
|----------|---------------------|
| `00` Immediate | `Immediate` |
| `01` Cons | `PointerCons(addr)` |
| `10` Header | `PointerHeader(addr)` |
| `11` Forward | `Forwarded(addr)` |

All boxed objects are "all-pointers" — no opaque cell ranges. This is the simplest possible binding and serves as a correctness check that the GC algorithms work independently of the specific tag scheme.

## Implementing your own binding

To connect NewGC to a new language:

1. Define a zero-sized marker type: `#[derive(Copy, Clone, Debug, Default)] pub struct MyLayout;`
2. Implement `HeapLayout for MyLayout`:
   - `FILL_WORD` — your language's null/nil/false bit pattern
   - `classify` — map your tag bits to `WordKind`
   - `make_forward` — encode a forwarding marker in your tag space; the `Forward` sentinel must not collide with any valid user tag
   - `make_pointer` — for tests; encode a cons or header pointer
   - `rewrite_pointer_addr` — preserve your tag bits while replacing the address bits
   - `header_layout` — decode your object header and return `total_cells`, `pointer_cells_start`, `pointer_cells_end`
3. Parameterise: `let heap = PageHeap::<MyLayout>::with_reservation(1 << 30);`

The GC will handle everything else: allocation, marking, evacuation, card scanning, pinning.

---

Back to [Home](index.md).
