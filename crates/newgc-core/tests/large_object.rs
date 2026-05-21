//! Sprint VM-1: large-object allocation and GC tests.
//!
//! Large objects span one or more whole 64 KB pages. They are allocated
//! via `try_alloc_large`, never copied during evacuation, and released
//! when they become unreachable.

use newgc_core::{
    Generation, HeapHeader, HeapType, LispLayout, PageHeap, PageKind,
    PAYLOAD_MASK, Tag, Word, PAGE_SIZE_CELLS,
    G0_PROMOTION_THRESHOLD, G1_PROMOTION_THRESHOLD,
};

type Heap = PageHeap<LispLayout>;

/// Allocate a large object: write a Vector header at cell 0 and fill
/// payload cells with `fixnum(42)`. Returns a tagged `Word` pointing
/// at the object.
///
/// `n_cells` is the TOTAL cells to allocate (1 header + (n_cells-1) payload).
fn alloc_large_object(
    h: &mut Heap,
    generation: Generation,
    n_cells: usize,
) -> Word {
    assert!(n_cells >= 1);
    let n_payload = (n_cells - 1) as u32;
    let ptr = h
        .try_alloc_large(n_cells, generation)
        .expect("try_alloc_large must succeed");
    unsafe {
        // Write a Vector header covering n_payload payload cells.
        let header = HeapHeader::new(HeapType::Vector, n_payload);
        (ptr.as_ptr() as *mut u64).write(header.raw());
        // Fill payload with fixnum(42).
        for i in 1..n_cells {
            ptr.as_ptr().add(i).write(Word::fixnum(42).raw());
        }
    }
    Word::from_ptr(ptr.as_ptr() as *const u8, Tag::Vector)
}

// ---------------------------------------------------------------------------
// Test 1: Large object spans more than one page
// ---------------------------------------------------------------------------

#[test]
fn vm1_large_object_allocates_across_page_boundary() {
    // n_cells = PAGE_SIZE_CELLS + 1 → needs 2 pages.
    let mut h = Heap::with_reservation(16 * 64 * 1024);
    let n_cells = PAGE_SIZE_CELLS + 1;
    let ptr = h.try_alloc_large(n_cells, Generation::G0);
    assert!(ptr.is_some(), "alloc of PAGE_SIZE_CELLS+1 cells must succeed");
    let ptr = ptr.unwrap();
    let page_idx = h.page_of(ptr.as_ptr() as *const u8).unwrap();
    let head_desc = h.desc(page_idx);
    assert!(head_desc.is_large_head(), "first page must be a large head");
    assert_eq!(head_desc.n_span, 2, "n_span must be 2 for a 2-page object");
    assert_eq!(head_desc.kind, PageKind::Large);
    let cont_desc = h.desc(page_idx + 1);
    assert!(cont_desc.is_large_cont(), "second page must be a large cont");
    assert_eq!(cont_desc.kind, PageKind::Large);
}

// ---------------------------------------------------------------------------
// Test 2: Large object survives minor GC without moving
// ---------------------------------------------------------------------------

#[test]
fn vm1_large_object_survives_minor_gc_as_pinned() {
    let mut h = Heap::with_reservation(64 * 64 * 1024);
    // Allocate large object spanning 3 pages.
    let n_cells = 2 * PAGE_SIZE_CELLS + 100;
    let obj = alloc_large_object(&mut h, Generation::G0, n_cells);
    let addr_before = obj.raw() & PAYLOAD_MASK;
    let mut root = [obj];

    // Run G0_PROMOTION_THRESHOLD minor GC cycles so the object promotes to G1.
    for _ in 0..G0_PROMOTION_THRESHOLD {
        h.collect_minor(|evac| evac.visit(&mut root[0]));
    }

    // Large object must still be at the same address (never moved).
    let addr_after = root[0].raw() & PAYLOAD_MASK;
    assert_eq!(
        addr_before, addr_after,
        "large object address must not change across GC"
    );

    // All 3 pages must now be in G1 (promoted in-place).
    let page_idx = h.page_of(addr_after as *const u8).unwrap();
    for i in 0..3 {
        assert_eq!(
            h.desc(page_idx + i).generation,
            Generation::G1,
            "page {} of large object must be G1 after promotion",
            i
        );
    }
}

// ---------------------------------------------------------------------------
// Test 3: Unrooted large object is reclaimed
// ---------------------------------------------------------------------------

#[test]
fn vm1_large_object_reclaimed_when_unrooted() {
    let mut h = Heap::with_reservation(64 * 64 * 1024);
    // Allocate a 3-page large object.
    let n_cells = 2 * PAGE_SIZE_CELLS + 1;
    let obj = alloc_large_object(&mut h, Generation::G0, n_cells);
    let addr = obj.raw() & PAYLOAD_MASK;
    let mut root = [obj];

    // One minor cycle WITH the root → object survives.
    h.collect_minor(|evac| evac.visit(&mut root[0]));
    let page_idx = h.page_of(addr as *const u8).unwrap();
    assert_ne!(
        h.desc(page_idx).generation,
        Generation::Free,
        "object must be live after first GC with root"
    );

    // Drop the root and run another minor cycle → object is unreachable → reclaimed.
    h.collect_minor(|_| {});

    // All 3 pages of the run must now be Free.
    for i in 0..3 {
        assert_eq!(
            h.desc(page_idx + i).generation,
            Generation::Free,
            "page {} must be reclaimed after large object becomes unreachable",
            i
        );
    }
}

// ---------------------------------------------------------------------------
// Test 4: Large and small objects coexist across GC cycles
// ---------------------------------------------------------------------------

#[test]
fn vm1_large_and_small_objects_coexist() {
    let mut h = Heap::with_reservation(64 * 64 * 1024);
    let n_cells = PAGE_SIZE_CELLS + 500;
    let large_obj = alloc_large_object(&mut h, Generation::G0, n_cells);
    let mut large_root = [large_obj];

    // Also allocate a small cons in G0.
    let small_ptr = h.try_alloc_cons_in(Generation::G0).unwrap();
    unsafe {
        *small_ptr.as_ptr() = Word::fixnum(7).raw();
        *small_ptr.as_ptr().add(1) = Word::NIL.raw();
    }
    let mut small_root = [Word::from_ptr(
        small_ptr.as_ptr() as *const u8,
        Tag::Cons,
    )];

    // Run a minor GC rooting both objects.
    h.collect_minor(|evac| {
        evac.visit(&mut large_root[0]);
        evac.visit(&mut small_root[0]);
    });

    // Both survive.

    // Check large object payload cell 1 still holds fixnum(42).
    let large_val = unsafe {
        let addr = (large_root[0].raw() & PAYLOAD_MASK) as *const u64;
        Word::from_raw(*addr.add(1)).as_fixnum()
    };
    assert_eq!(large_val, Some(42), "large object payload must be intact");

    // Check small cons car is still fixnum(7).
    let small_val = unsafe {
        let addr = (small_root[0].raw() & PAYLOAD_MASK) as *const u64;
        Word::from_raw(*addr).as_fixnum()
    };
    assert_eq!(small_val, Some(7), "small cons car must be intact");
}

// ---------------------------------------------------------------------------
// Test 5: Simulate a Dylan table bucket array (~24000 cells = 3 pages)
// ---------------------------------------------------------------------------

#[test]
fn vm1_table_buckets_size_24000_cells() {
    // Simulate a 5000-key Dylan table bucket array: ~24000 cells = 3 pages.
    let mut h = Heap::with_reservation(64 * 64 * 1024);
    let n_cells = 24_000;
    let obj = alloc_large_object(&mut h, Generation::G0, n_cells);
    let mut root = [obj];

    // Run enough cycles to promote to G1.
    for _ in 0..G0_PROMOTION_THRESHOLD {
        h.collect_minor(|evac| evac.visit(&mut root[0]));
    }

    let addr = root[0].raw() & PAYLOAD_MASK;
    let page = h.page_of(addr as *const u8).unwrap();
    let head = h.desc(page);
    assert_eq!(
        head.generation,
        Generation::G1,
        "large object must promote to G1"
    );
    assert!(head.is_large_head(), "promoted page must still be a large head");
    assert_eq!(head.n_span, 3, "24000 cells spans 3 pages");
}

// ---------------------------------------------------------------------------
// Test 6: collect_full reclaims dead Tenured large object
// ---------------------------------------------------------------------------

#[test]
fn vm1_collect_full_reclaims_dead_large_object_in_tenured() {
    // Promote a large object all the way to Tenured, then reclaim it.
    let mut h = Heap::with_reservation(256 * 64 * 1024);
    let n_cells = PAGE_SIZE_CELLS + 1; // 2 pages
    let obj = alloc_large_object(&mut h, Generation::G0, n_cells);
    let mut root = [obj];

    // Promote to G1 via minor GCs.
    let total_minors =
        G0_PROMOTION_THRESHOLD * G1_PROMOTION_THRESHOLD;
    for _ in 0..total_minors {
        h.collect_minor(|evac| evac.visit(&mut root[0]));
    }

    let addr = root[0].raw() & PAYLOAD_MASK;
    let page = h.page_of(addr as *const u8).unwrap();
    let page_gen = h.desc(page).generation;
    assert_eq!(page_gen, Generation::Tenured, "large object must be Tenured");

    // Now collect_full with no root → object unreachable → reclaimed.
    let result = h.collect_full(|_| {});
    assert!(
        result.tenured_freed_bytes > 0 || result.tenured_evac.pages_freed > 0,
        "collect_full must reclaim the dead large Tenured object: {:?}",
        result
    );
    assert_eq!(
        h.desc(page).generation,
        Generation::Free,
        "large object pages must be Free after collect_full"
    );
}
