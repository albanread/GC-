# Heap Layout

## Virtual address space

The heap is a single contiguous virtual address reservation. The default size is 2 GB. All pages are reserved upfront but committed on demand — virtual memory is cheap; physical RAM is not.

On Windows, `VirtualAlloc(MEM_RESERVE)` reserves the address range and `VirtualAlloc(MEM_COMMIT)` commits individual pages when first written. On Unix, `mmap(PROT_NONE | MAP_NORESERVE)` reserves without committing, and `mprotect(PROT_READ | PROT_WRITE)` commits. A boxed fallback (`Box<[u8]>`) is fully committed and is used on targets that support neither.

## Page size

Pages are fixed at 64 KB (65,536 bytes = 8,192 cells of 8 bytes each). This matches the `VirtualAlloc` granularity on Windows, making aligned page boundaries free to enforce.

## Generations

Every live page belongs to one of four `Generation` values:

| Generation | Purpose | Collected by |
|------------|---------|-------------|
| `Free` | Not assigned; may or may not be committed | — |
| `G0` | Nursery — all new allocations land here | `collect_minor` |
| `G1` | Intermediate — survived enough G0 cycles | `collect_minor` (cascade) or `collect_major` |
| `Tenured` | Old — survived enough G1 promotions | `collect_full` only |

The promotion ladder is `Free → G0 → G1 → Tenured`. Tenured is a fixed point; there is no super-tenured generation.

## Page kinds

Every non-free page has a `PageKind` that determines how the GC walks its contents:

| Kind | Object shape | Walking strategy |
|------|-------------|-----------------|
| `Cons` | Headerless 2-cell pairs | Fixed stride of 2 cells; every even cell is an object start |
| `Boxed` | Variable-size objects with a `HeapHeader` at cell 0 | Start-bit bitmap lookup to find object boundaries |
| `Large` | One object spanning one or more whole pages | `n_span` field on head page gives run length |
| `Free` | Unassigned | Not walked |

A Cons page never holds boxed objects and vice versa. This separation is enforced by having distinct `AllocRegion` instances per `(Generation, PageKind)` pair.

## PageDesc — per-page metadata

The heap maintains a parallel `Vec<PageDesc>` with one 12-byte entry per page, indexed by page number. This keeps object-bearing pages free of metadata fragmentation.

```
offset  field               type    bytes   purpose
0       scan_start_offset   u32     4       cell offset for next mark-pass start
4       words_used          u16     2       bump-pointer high-water mark (cells consumed)
6       generation          u8      1       Generation enum
7       kind                u8      1       PageKind enum
8       pin_byte            u8      1       sub-page pin bitmap (8 × 8 KB sub-regions)
9       age                 u8      1       minor cycles survived in current generation
10      n_span              u16     2       large-object run length (1 for Cons/Boxed)
                                    ─────
total                               12
```

`PageDesc` is `#[repr(C)]` and `assert_eq!(size_of::<PageDesc>(), 12)` is a compile-time assertion.

### `words_used`

The bump-pointer high-water mark. Set by the allocator as cells are claimed. Evacuation rewrites it after compaction to reflect the reduced occupancy of surviving objects.

### `scan_start_offset`

Reserved for incremental scanning. Currently zero for fresh pages; updated when the evacuator compacts a page so subsequent mark passes begin after the last surviving object.

### `pin_byte`

An 8-bit bitmap. Bit `i` is set when at least one object in the `i`-th 8 KB sub-region of the page has been identified as a conservative pin. This acts as a coarse first filter: the evacuator checks `pin_byte != 0` before doing a full hashtable lookup for individual pinned cell addresses.

```rust
pub fn set_pin(&mut self, slot: u8) { self.pin_byte |= 1 << slot; }
pub fn has_pins(&self) -> bool      { self.pin_byte != 0 }
```

### `n_span`

For `Cons` and `Boxed` pages: always `1`. For `Large` pages: `>= 1` on the head page (the actual page count in the run), `0` on continuation pages. Free pages have `n_span = 0`.

## Bitmaps

Three bitmaps span the full reservation:

### Start-bit bitmap

2 bits per cell, packed 32 cells per `u64`. Total size for 2 GB: `(2 GB / 8 bytes) × 2 bits / 8 = 64 MB`.

Encoding:
- `00` — not an object start (default for all cells after page reuse)
- `01` — start of a boxed (header-bearing) object
- `11` — start of a cons cell

The allocator writes start bits atomically via `fetch_or`. The consistency gate during mark/evac checks that a cons-tagged pointer lands on a `11` cell and a header-tagged pointer lands on a `01` cell.

### Mark bitmap

1 bit per cell, packed 64 cells per `u64`. Total size for 2 GB: `(2 GB / 8 bytes) / 8 = 32 MB`. Cleared only for the target generation at the start of each collection pass.

### Committed-page bitmap

1 bit per page, packed 64 pages per `u64`. Total size: `32,768 / 64 / 8 = 512 bytes`. Read lock-free via `AtomicU64`.

## Memory overhead

For a 2 GB reservation:

| Component | Size | Notes |
|-----------|------|-------|
| Virtual address space | 2 GB | Not committed until written |
| PageDesc table | 384 KB | 12 bytes × 32,768 pages |
| Start-bit bitmap | 64 MB | 2 bits/cell |
| Mark bitmap | 32 MB | 1 bit/cell |
| Card table | 4 MB | 1 byte per 512-byte card |
| Committed-page bitmap | 512 B | 1 bit/page |
| AllocRegion state | < 1 KB | 6 regions |
| Pinned-cells HashSet | variable | Empty outside collections |

Total fixed overhead: roughly 100 MB — 5% of the reservation. All metadata except the PageDesc table and card table is zero-filled automatically by the OS for committed pages; only metadata pages are touched on startup.

---

Back to [Home](index.md).
