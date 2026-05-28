# Configuration and API

## Constructing a heap

```rust
// Explicitly-sized reservation
let heap = PageHeap::<LispLayout>::with_reservation(2 * 1024 * 1024 * 1024); // 2 GB

// Two-argument constructor (young_bytes hint, old_bytes hint — reserves the sum)
let heap = PageHeap::<LispLayout>::new(256 * 1024 * 1024, 768 * 1024 * 1024);
```

The reservation is virtual address space. Physical RAM is committed only when a page is first written. You can safely over-provision on 64-bit systems.

## Allocation

```rust
// Allocate a cons cell in G0 — returns NonNull<u64> pointing to car cell
let cell: NonNull<u64> = heap.try_alloc_cons_in(Generation::G0)?;

// Allocate a boxed object (header + n payload cells) in G0
let cell: NonNull<u64> = heap.try_alloc_boxed_in(total_cells, Generation::G0)?;
```

These return `Option<NonNull<u64>>` — `None` means no free pages available. In normal operation the trigger policy fires before the heap is full, so allocation failures only occur if the caller ignores `should_collect()` signals.

After allocation, write the `HeapHeader` (for boxed objects) and payload cells, then set the start bits if your allocator does not use the built-in bump-pointer path.

## Collection — manual

```rust
// Minor cycle: collect G0 → G0 or G0 → G1
let result: CollectResult = heap.collect_minor(|evac| {
    evac.visit(&mut root1);
    evac.visit(&mut root2);
});

// Major cycle: G1 → Tenured, then G0 → G0
let result: CollectResult = heap.collect_major(|evac| {
    evac.visit(&mut root1);
});

// Full cycle: G0 → G1 → Tenured → Tenured (compact)
let result: FullCollectResult = heap.collect_full(|evac| {
    evac.visit(&mut root1);
});

// Low-level: evacuate a specific generation pair
let result: EvacResult = heap.evacuate_with_roots(Generation::G0, Generation::G1, |evac| {
    evac.visit(&mut root1);
});
```

The `visit_roots` closure receives a `&mut PageEvacuator`. Call `evac.visit(&mut slot)` for every live pointer slot. The slot is updated in place if the object moves. Visit every root — missing a root means the GC may reclaim a live object.

## Collection — automatic

```rust
// Returns true when bytes_alloc_since_gc >= auto_gc_trigger_bytes
if heap.should_collect() {
    heap.collect_auto(|evac| { evac.visit(&mut root); });
}
```

`collect_auto` calls `collect_major` when Tenured occupancy exceeds `tenured_full_threshold_bps`, otherwise `collect_minor`.

## Trigger policy configuration

```rust
// Minimum allocation budget before triggering (default 8 MB)
heap.set_gc_budget_min_bytes(16 * 1024 * 1024);

// Tenured occupancy threshold for switching minor → major (default 7500 = 75%)
heap.set_tenured_full_threshold_bps(8000); // 80%
```

The trigger threshold after each cycle is `max(budget_min, 0.5 × tenured_used_bytes)`. Larger Tenured sets grow the threshold proportionally.

## OOM-safe collection

The `try_collect_*` variants catch mid-evacuation OOM and return a `Result`:

```rust
match heap.try_collect_minor(|evac| { evac.visit(&mut root); }) {
    Ok(result) => { /* result: CollectResult */ }
    Err(GcError::MidEvacOom(e)) => {
        eprintln!("OOM during minor GC: {e:?}");
        // heap is now poisoned; drop it
    }
    Err(GcError::HeapPoisoned) => {
        eprintln!("heap was already failed");
    }
}
```

After `GcError::MidEvacOom` the heap is **poisoned** — the `poisoned` flag is set. All subsequent allocation and collection calls will panic. The only safe operation on a poisoned heap is `Drop`.

## Conservative pinning

```rust
// Pin objects reachable from stack ranges before a collection
let stack_top: *const u8 = /* get from OS or thread local */;
let stack_bot: *const u8 = /* current RSP */;
heap.pin_pointers_in_ranges(&[(stack_bot, stack_top)]);

// Then collect
heap.collect_minor(|evac| { evac.visit(&mut root); });
```

Pins are cleared automatically at the start of each collection. Re-pin before each cycle if needed.

```rust
// Diagnostic
let n = heap.pinned_count();
let is_pinned = heap.is_pinned_cell(cell_idx);
```

## Statistics

```rust
let stats: GcStats = heap.stats();
```

`GcStats` fields:

| Category | Fields |
|----------|--------|
| Capacity | `reserved_bytes`, `committed_bytes`, `committed_pages`, `total_pages`, `free_pages`, `page_size_bytes` |
| G0 | `g0_used_bytes`, `g0_used_pages` |
| G1 | `g1_used_bytes`, `g1_used_pages` |
| Tenured | `tenured_used_bytes`, `tenured_used_pages` |
| Totals | `total_used_bytes`, `total_used_pages` |
| Trigger | `bytes_alloc_since_gc`, `auto_gc_trigger_bytes`, `gc_budget_min_bytes`, `tenured_full_threshold_bps` |
| Last cycle | `last_mark_live_bytes`, `last_mark_live_pages`, `last_zero_live_pages_released`, `last_pin_summary` |
| Cohort counters | `minors_since_g0_promote`, `g0_promotes_since_g1_promote`, `total_minors`, `total_majors` |

Lock-free reads: `committed_pages()`, `page_count()`, `reserved_bytes()`, and `base_ptr()` all use atomic reads and do not require exclusive access to the heap.

## Platform notes

| Platform | Memory backend | Commit | Decommit |
|----------|---------------|--------|---------|
| Windows | `VirtualAlloc(MEM_RESERVE)` | `VirtualAlloc(MEM_COMMIT)` | `VirtualFree(MEM_DECOMMIT)` |
| Linux/macOS | `mmap(PROT_NONE)` | `mprotect(PROT_READ\|WRITE)` | `madvise(MADV_DONTNEED)` + `mprotect(PROT_NONE)` |
| Other | `Box<[u8]>` (fully committed) | (all pages always committed) | (not supported) |

The boxed fallback never decommits memory. All pages are always physically resident. Use it only for targets without virtual-memory primitives.

---

Back to [Home](index.md).
