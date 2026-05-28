# NewGC

NewGC is a page-based, generational, mark-evacuate garbage collector written in Rust. It is extracted from the NewCormanLisp page-heap module and redesigned as a language-agnostic engine: the core algorithms — allocation, marking, evacuation, card-barrier scanning — are parameterised by a single `HeapLayout` trait. Any language that implements the trait gets the full GC for free.

## What it does

- **Generational collection** — three generations (G0 nursery, G1 intermediate, Tenured) with configurable promotion thresholds
- **Cheney-style evacuation** — live objects are copied to fresh pages; no fragmentation over time
- **Soft write barrier** — a 512-byte card table tracks old-to-young pointer stores at low cost
- **Conservative pinning** — an optional stack scanner pins objects that cannot be moved safely
- **Language-agnostic** — every tag-dispatch, header-decode, and pointer-rewrite operation is delegated to a zero-overhead trait; two reference implementations (`LispLayout`, `TinyLayout`) ship with the crate
- **Cross-platform** — native virtual memory on Windows (`VirtualAlloc`) and Unix (`mmap`), plus a boxed fallback for other targets
- **311+ tests** — unit, integration, stochastic, threading, and trigger-policy test suites

## Status

Phase 1 (standalone extraction) is complete. Phase 2 (language-binding trait extraction) is in progress.

## Documentation

| Page | What it covers |
|------|----------------|
| [Architecture](architecture.md) | Module map, data flow, crate structure |
| [Word and Tags](word-and-tags.md) | The 64-bit `Word` type, 3-bit tag scheme, immediates |
| [Heap Layout](heap-layout.md) | 64 KB pages, PageDesc, generations, page kinds, memory overhead |
| [Object Model](object-model.md) | HeapHeader, HeapType, GcBits, ObjectLayout |
| [Allocation](allocation.md) | Bump-pointer regions, start-bit bitmap, page acquisition |
| [GC Cycles](gc-cycles.md) | Minor, major, full collection; cohort promotion; trigger policy |
| [Mark and Evacuate](mark-and-evacuate.md) | BFS mark pass, Cheney evacuation, forwarding pointers |
| [Write Barrier](write-barrier.md) | Card table structure, mutator discipline, dirty-card scanning |
| [Language Binding](language-binding.md) | HeapLayout trait, WordKind, LispLayout, TinyLayout |
| [Conservative Pinning](conservative-pinning.md) | Stack scanner, five-gate validation, dual-level pin index |
| [Configuration and API](configuration.md) | Heap construction, tuning knobs, collection APIs, stats, errors |
| [Threading](threading.md) | What works today, roadmap to concurrent mutators |
| [Test Driver](test-driver.md) | Mini-Lisp evaluator: syntax, GC integration, workload scripts |

## Quick start

```
cargo build --release
cargo test
```

No system dependencies beyond Rust stable. On Windows the `windows` crate links against in-box DLLs (`VirtualAlloc` etc.). On Linux `libc` is used for `mmap`. No third-party native libraries are required.

## Crate layout

```
NewGC/
├── Cargo.toml                       workspace root
└── crates/
    ├── newgc-core/                  the GC engine
    │   ├── src/
    │   │   ├── lib.rs               public re-exports
    │   │   ├── traits.rs            HeapLayout, WordKind, ObjectLayout
    │   │   ├── word.rs              Word, Tag, fixnum encoding
    │   │   ├── heap_common.rs       HeapHeader, HeapType, CardTable, GcBit
    │   │   ├── lisp_layout.rs       reference binding (3-bit Lisp tags)
    │   │   ├── tiny_layout.rs       reference binding (2-bit minimal tags)
    │   │   └── page_heap/
    │   │       ├── space.rs         PageHeap<L> — core struct, OS backing
    │   │       ├── page_desc.rs     PageDesc, Generation, PageKind
    │   │       ├── alloc.rs         AllocRegion, start-bit bitmap
    │   │       ├── mark.rs          BFS mark pass
    │   │       ├── evac.rs          Cheney evacuation, forwarding pointers
    │   │       ├── cycle.rs         minor / major / full GC drivers
    │   │       ├── pin.rs           conservative stack scanner
    │   │       ├── scanner.rs       card-table dirty-card extraction
    │   │       └── coordinator_api.rs  legacy semispace accessors
    │   └── tests/                   integration and stress tests
    └── newgc-test-lisp/             mini-Lisp workload driver
        ├── src/
        │   ├── reader.rs            S-expression parser
        │   ├── value.rs             Value enum, heap allocation helpers
        │   ├── eval.rs              tree-walking interpreter
        │   └── lib.rs
        ├── tests/                   Lisp-level GC tests
        └── scripts/                 .lisp workload files
```
