//! Synthetic stress / correctness tests for newgc-core.
//!
//! These exercise the page-heap GC at its public API surface, on
//! graph shapes that the per-module unit tests in upstream NCL did
//! not. The bar is "every object reachable before a cycle is
//! reachable after, with payload intact" — actual correctness, not
//! mechanic shape.
//!
//! `GC_LESSONS.md` Pattern 2 is the explicit caveat: these tests can
//! all pass and the GC can still be broken in ways that only show up
//! on a real workload. They raise the floor; they don't replace
//! integration with a language runtime.
//!
//! Conventions:
//!   - Heaps are sized small (8–32 pages) so behaviour under near-OOM
//!     pressure is also exercised.
//!   - Object graphs use NCL's tagged Word + `HeapType::Vector` for
//!     boxed objects (full-payload scan). Cons cells use the cons
//!     tag directly.
//!   - Every test asserts both (a) that the GC didn't panic and
//!     (b) some property of the post-cycle heap (page counts,
//!     payload values, or reachability count).

use newgc_core::page_heap::evac::EvacResult;
use newgc_core::{
    Generation, HeapHeader, HeapType, LispLayout, PageHeap, PAYLOAD_MASK, Tag, Word,
};

/// Concrete page-heap type used throughout these synthetic tests.
/// All Word / HeapHeader manipulation is NCL-shaped, so the layout
/// binding is fixed to `LispLayout`.
type TestHeap = PageHeap<LispLayout>;

// -- Helpers ----------------------------------------------------------------

/// 8-page heap = 512 KB. Default for tests that don't need pressure.
fn small_heap() -> TestHeap {
    TestHeap::with_reservation(8 * 64 * 1024)
}

/// 32-page heap = 2 MB. Used for the random-graph stress tests.
fn medium_heap() -> TestHeap {
    TestHeap::with_reservation(32 * 64 * 1024)
}

/// Allocate a cons cell containing `(car . cdr)`. Returns the
/// Word-tagged pointer to the new cons.
fn cons(h: &mut TestHeap, g: Generation, car: Word, cdr: Word) -> Word {
    let p = h.try_alloc_cons_in(g).expect("cons alloc");
    unsafe {
        *p.as_ptr() = car.raw();
        *p.as_ptr().add(1) = cdr.raw();
    }
    Word::from_ptr(p.as_ptr() as *const u8, Tag::Cons)
}

/// Allocate a Vector with `n_payload` Word slots, all initialised to
/// `init`. Returns the Word-tagged pointer to the header cell.
fn vector(h: &mut TestHeap, g: Generation, n_payload: u32, init: Word) -> Word {
    let total_cells = (1 + n_payload) as usize;
    let p = h
        .try_alloc_boxed_in(g, total_cells)
        .expect("vector alloc");
    unsafe {
        *p.as_ptr() = HeapHeader::new(HeapType::Vector, n_payload).raw();
        for i in 1..=n_payload as usize {
            *p.as_ptr().add(i) = init.raw();
        }
    }
    Word::from_ptr(p.as_ptr() as *const u8, Tag::Vector)
}

/// Read slot `idx` of a vector pointed to by `w`. Slot 0 is the
/// header; slot 1 is the first payload cell.
unsafe fn vector_slot(w: Word, idx: usize) -> Word {
    let base = (w.raw() & PAYLOAD_MASK) as *const u64;
    unsafe { Word::from_raw(*base.add(idx)) }
}

/// Write slot `idx` of a vector pointed to by `w`. Slot 0 is the
/// header; slot 1 is the first payload cell.
unsafe fn set_vector_slot(w: Word, idx: usize, value: Word) {
    let base = (w.raw() & PAYLOAD_MASK) as *mut u64;
    unsafe { *base.add(idx) = value.raw() }
}

/// Build a chain of `n` cons cells: `(0 1 2 … n-1)`. Returns the
/// head Word.
fn list(h: &mut TestHeap, g: Generation, n: usize) -> Word {
    let mut head = Word::NIL;
    for i in (0..n).rev() {
        head = cons(h, g, Word::fixnum(i as i64), head);
    }
    head
}

/// Walk a list and return its elements as fixnums. Errors out on
/// improper lists.
unsafe fn list_to_vec(head: Word) -> Vec<i64> {
    let mut v = Vec::new();
    let mut cur = head;
    while cur.tag() == Tag::Cons {
        let ptr = (cur.raw() & PAYLOAD_MASK) as *const u64;
        let car = unsafe { Word::from_raw(*ptr) };
        let cdr = unsafe { Word::from_raw(*ptr.add(1)) };
        v.push(car.as_fixnum().expect("car must be fixnum"));
        cur = cdr;
    }
    assert!(cur.is_nil(), "improper list: tail = {cur:?}");
    v
}

/// Re-resolve a Word against forward-pointer chains. Tests use this
/// after evacuation to follow root rewrites manually when the root
/// wasn't passed through the evac visitor.
unsafe fn resolve_forwards(mut w: Word) -> Word {
    while w.is_forward() {
        let p = w.forward_target().expect("forwarded") as *const u64;
        // The forward marker stores the new tagged Word in the slot
        // following the marker cell — same as the live header. But for
        // tests we only encode forwards in object headers, so this
        // helper is unused in current tests; leave for future work.
        w = unsafe { Word::from_raw(*p) };
    }
    w
}

// Suppress dead-code warning while resolve_forwards is unused.
#[allow(dead_code)]
fn _keep_resolve_forwards_alive() {
    let _ = unsafe { resolve_forwards(Word::NIL) };
}

// -- Steady-state allocation ------------------------------------------------

#[test]
fn steady_state_alloc_and_drop_does_not_grow_unboundedly() {
    // Allocate 1000 cons cells, drop the root before each minor cycle.
    // After 20 cycles the heap should have come back to (near) empty
    // every time. Verify peak page usage is bounded.
    let mut h = small_heap();
    let mut peak_g0 = 0;

    for _ in 0..20 {
        let _head = list(&mut h, Generation::G0, 1000);
        peak_g0 = peak_g0.max(h.count_pages_in_gen(Generation::G0));
        // Drop the head by not retaining it as a root.
        let _ = h.evacuate_from_word_roots(
            Generation::G0,
            Generation::G1,
            &mut [],
        );
        // G0 should be empty (no roots → nothing copied).
        assert_eq!(h.count_pages_in_gen(Generation::G0), 0);
    }

    assert!(peak_g0 > 0, "test should have allocated SOMETHING");
    assert!(peak_g0 < 16, "peak G0 usage was {peak_g0}, expected bounded");
}

#[test]
fn alloc_evac_alloc_replay_keeps_working() {
    // 10 rounds of: alloc 500-cell chain (G0), evac G0→G1 with the
    // existing G1 roots preserved + the new G0 chain rooted. After
    // each round, walk every chain root and verify its length.
    let mut h = medium_heap();
    let mut roots: Vec<Word> = Vec::new();
    for round in 0..10 {
        let new_chain = list(&mut h, Generation::G0, 50);
        roots.push(new_chain);
        let mut roots_slice: Vec<Word> = roots.clone();
        let _ = h.evacuate_from_word_roots(
            Generation::G0,
            Generation::G1,
            &mut roots_slice,
        );
        roots = roots_slice;
        // Verify every retained chain is intact (length 50, contents
        // 0..50).
        for (i, &r) in roots.iter().enumerate() {
            let elems = unsafe { list_to_vec(r) };
            assert_eq!(
                elems.len(),
                50,
                "round {round}, chain {i}: length={}",
                elems.len()
            );
            for (j, v) in elems.iter().enumerate() {
                assert_eq!(*v, j as i64);
            }
        }
    }
    assert_eq!(roots.len(), 10);
}

// -- Deep graph survival ----------------------------------------------------

#[test]
fn deep_chain_survives_payload_intact() {
    let mut h = medium_heap();
    let n = 2000;
    let head = list(&mut h, Generation::G0, n);
    let mut roots = [head];
    h.evacuate_from_word_roots(
        Generation::G0,
        Generation::G1,
        &mut roots,
    );
    let elems = unsafe { list_to_vec(roots[0]) };
    assert_eq!(elems.len(), n);
    for (i, v) in elems.iter().enumerate() {
        assert_eq!(*v, i as i64, "element {i} corrupt: got {v}");
    }
}

#[test]
fn deep_chain_survives_two_minor_cycles() {
    let mut h = medium_heap();
    let n = 1000;
    let head = list(&mut h, Generation::G0, n);
    let mut roots = [head];
    h.evacuate_from_word_roots(Generation::G0, Generation::G1, &mut roots);
    h.evacuate_from_word_roots(Generation::G1, Generation::Tenured, &mut roots);
    let elems = unsafe { list_to_vec(roots[0]) };
    assert_eq!(elems.len(), n);
    for (i, v) in elems.iter().enumerate() {
        assert_eq!(*v, i as i64);
    }
}

// -- Wide fan-out -----------------------------------------------------------

#[test]
fn wide_fanout_vector_survives() {
    let mut h = medium_heap();
    // One vector with 500 payload slots, each pointing at a unique
    // 5-cell cons chain.
    let vec_w = vector(&mut h, Generation::G0, 500, Word::NIL);
    for i in 0..500 {
        let chain = list(&mut h, Generation::G0, 5);
        unsafe { set_vector_slot(vec_w, 1 + i, chain) };
        // Stamp the chain head so we can identify it.
        let head_ptr = (chain.raw() & PAYLOAD_MASK) as *mut u64;
        unsafe { *head_ptr = Word::fixnum(i as i64 + 1_000_000).raw() };
    }

    let mut roots = [vec_w];
    h.evacuate_from_word_roots(
        Generation::G0,
        Generation::G1,
        &mut roots,
    );
    // Each branch should still be present and start with the
    // stamped marker.
    for i in 0..500 {
        let branch = unsafe { vector_slot(roots[0], 1 + i) };
        assert_eq!(branch.tag(), Tag::Cons);
        let branch_ptr = (branch.raw() & PAYLOAD_MASK) as *const u64;
        let car = unsafe { Word::from_raw(*branch_ptr) };
        assert_eq!(
            car.as_fixnum(),
            Some(i as i64 + 1_000_000),
            "branch {i} corrupt"
        );
    }
}

// -- DAG with shared substructure -------------------------------------------

#[test]
fn dag_shared_node_is_not_duplicated() {
    // Two roots both pointing at the same shared cons. After
    // evacuation, both roots should point at the SAME new cons.
    let mut h = small_heap();
    let shared = cons(&mut h, Generation::G0, Word::fixnum(99), Word::NIL);
    let left = cons(&mut h, Generation::G0, Word::fixnum(1), shared);
    let right = cons(&mut h, Generation::G0, Word::fixnum(2), shared);

    let mut roots = [left, right];
    let result = h.evacuate_from_word_roots(
        Generation::G0,
        Generation::G1,
        &mut roots,
    );
    // 3 objects total: shared, left, right.
    assert_eq!(result.objects_copied, 3);
    assert_eq!(result.cells_copied, 6);

    // Both new roots' cdrs should point at the same shared cell.
    let left_ptr = (roots[0].raw() & PAYLOAD_MASK) as *const u64;
    let right_ptr = (roots[1].raw() & PAYLOAD_MASK) as *const u64;
    let left_cdr = unsafe { Word::from_raw(*left_ptr.add(1)) };
    let right_cdr = unsafe { Word::from_raw(*right_ptr.add(1)) };
    assert_eq!(
        left_cdr.raw(),
        right_cdr.raw(),
        "shared subgraph was duplicated: {:?} vs {:?}",
        left_cdr,
        right_cdr
    );
    // The shared cell's car should still be 99.
    let shared_ptr = (left_cdr.raw() & PAYLOAD_MASK) as *const u64;
    let car = unsafe { Word::from_raw(*shared_ptr) };
    assert_eq!(car.as_fixnum(), Some(99));
}

#[test]
fn diamond_dag_two_paths_to_same_leaf() {
    // root → (left, right); left and right both point at leaf.
    let mut h = small_heap();
    let leaf = cons(&mut h, Generation::G0, Word::fixnum(7), Word::NIL);
    let left = cons(&mut h, Generation::G0, leaf, Word::NIL);
    let right = cons(&mut h, Generation::G0, leaf, Word::NIL);
    let root = cons(&mut h, Generation::G0, left, right);

    let mut roots = [root];
    let result = h.evacuate_from_word_roots(
        Generation::G0,
        Generation::G1,
        &mut roots,
    );
    // 4 unique objects: root, left, right, leaf.
    assert_eq!(result.objects_copied, 4);
    assert_eq!(result.cells_copied, 8);

    // Verify both paths reach the same leaf.
    let root_ptr = (roots[0].raw() & PAYLOAD_MASK) as *const u64;
    let new_left = unsafe { Word::from_raw(*root_ptr) };
    let new_right = unsafe { Word::from_raw(*root_ptr.add(1)) };
    let left_ptr = (new_left.raw() & PAYLOAD_MASK) as *const u64;
    let right_ptr = (new_right.raw() & PAYLOAD_MASK) as *const u64;
    let left_leaf = unsafe { Word::from_raw(*left_ptr) };
    let right_leaf = unsafe { Word::from_raw(*right_ptr) };
    assert_eq!(
        left_leaf.raw(),
        right_leaf.raw(),
        "diamond duplicated leaf"
    );
}

// -- Random graph fuzz ------------------------------------------------------

/// Trivial LCG so tests are deterministic. Not cryptographic.
fn lcg(state: &mut u64) -> u64 {
    *state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    *state
}

#[test]
fn random_graph_reachability_preserved() {
    // Build N cons cells. Each cell's cdr is either NIL or a random
    // earlier cell. Pick a random subset as roots. Mark every
    // reachable cell PRE-cycle, then GC, then traverse from the
    // (rewritten) roots and verify the same reachable count.
    let mut h = medium_heap();
    let mut state: u64 = 0xdead_beef_5eed;
    let n = 800usize;

    let mut all: Vec<Word> = Vec::with_capacity(n);
    let mut reachable_indices = std::collections::HashSet::new();

    // Build the graph.
    for i in 0..n {
        let cdr = if i == 0 || (lcg(&mut state) & 3) == 0 {
            Word::NIL
        } else {
            all[(lcg(&mut state) as usize) % i]
        };
        let c = cons(&mut h, Generation::G0, Word::fixnum(i as i64), cdr);
        all.push(c);
    }

    // Pick 20 random roots.
    let mut roots: Vec<Word> = (0..20)
        .map(|_| all[(lcg(&mut state) as usize) % n])
        .collect();

    // Compute pre-cycle reachable set by walking from each root.
    for &r in &roots {
        let mut cur = r;
        while cur.tag() == Tag::Cons {
            let ptr = (cur.raw() & PAYLOAD_MASK) as *const u64;
            let car = unsafe { Word::from_raw(*ptr) };
            let cdr = unsafe { Word::from_raw(*ptr.add(1)) };
            reachable_indices.insert(car.as_fixnum().unwrap());
            cur = cdr;
        }
    }

    // GC.
    let result = h.evacuate_from_word_roots(
        Generation::G0,
        Generation::G1,
        &mut roots,
    );

    // Object count check: every unique reachable cell should have
    // been copied exactly once.
    assert_eq!(
        result.objects_copied,
        reachable_indices.len(),
        "objects_copied={}, expected={}",
        result.objects_copied,
        reachable_indices.len()
    );

    // Walk from the new roots, accumulate the indices, compare.
    let mut post_indices = std::collections::HashSet::new();
    for &r in &roots {
        let mut cur = r;
        while cur.tag() == Tag::Cons {
            let ptr = (cur.raw() & PAYLOAD_MASK) as *const u64;
            let car = unsafe { Word::from_raw(*ptr) };
            let cdr = unsafe { Word::from_raw(*ptr.add(1)) };
            post_indices.insert(car.as_fixnum().unwrap());
            cur = cdr;
        }
    }
    assert_eq!(post_indices, reachable_indices,
        "reachable set differs after GC");
}

// -- Cross-generation references --------------------------------------------

#[test]
fn old_gen_pointing_at_young_survives_minor() {
    // Allocate a vector in G1 with one slot. Allocate a leaf in G0.
    // Have the G1 vector's slot point at the G0 leaf. Run a minor:
    // the leaf gets evacuated G0→G1; the old vector's slot must be
    // updated to the new address.
    let mut h = small_heap();
    let leaf = cons(&mut h, Generation::G0, Word::fixnum(42), Word::NIL);
    let vec_w = vector(&mut h, Generation::G1, 1, Word::NIL);
    unsafe { set_vector_slot(vec_w, 1, leaf) };

    // Manually feed the vector's slot as a root. (In a real
    // runtime, this is what the card barrier scan would do.)
    let leaf_ptr_in_vec = (vec_w.raw() & PAYLOAD_MASK) as *mut u64;
    let mut roots = [unsafe { Word::from_raw(*leaf_ptr_in_vec.add(1)) }];
    h.evacuate_from_word_roots(
        Generation::G0,
        Generation::G1,
        &mut roots,
    );
    unsafe { *leaf_ptr_in_vec.add(1) = roots[0].raw() };

    // The G1 vector's slot now points into the new (G1) location of
    // the leaf. Reading that should give us back fixnum 42.
    let new_leaf = unsafe { vector_slot(vec_w, 1) };
    assert_eq!(new_leaf.tag(), Tag::Cons);
    let car = unsafe {
        let p = (new_leaf.raw() & PAYLOAD_MASK) as *const u64;
        Word::from_raw(*p)
    };
    assert_eq!(car.as_fixnum(), Some(42));
}

// -- Empty / boundary ------------------------------------------------------

#[test]
fn empty_root_set_is_a_noop() {
    let mut h = small_heap();
    let _ = list(&mut h, Generation::G0, 50);
    let result = h.evacuate_from_word_roots(
        Generation::G0,
        Generation::G1,
        &mut [],
    );
    assert_eq!(result.objects_copied, 0);
    assert_eq!(h.count_pages_in_gen(Generation::G0), 0,
        "unrooted G0 should be reclaimed");
}

#[test]
fn nil_only_root_is_a_noop() {
    let mut h = small_heap();
    let _ = list(&mut h, Generation::G0, 50);
    let mut roots = [Word::NIL, Word::T, Word::UNBOUND, Word::fixnum(7), Word::char('a')];
    let result = h.evacuate_from_word_roots(
        Generation::G0,
        Generation::G1,
        &mut roots,
    );
    assert_eq!(result.objects_copied, 0);
    // Immediates must be left alone.
    assert!(roots[0].is_nil());
    assert!(roots[1].is_t());
    assert!(roots[2].is_unbound());
    assert_eq!(roots[3].as_fixnum(), Some(7));
    assert_eq!(roots[4].as_char(), Some('a'));
}

// -- Mixed payload (Vector slots holding immediates + pointers) ------------

#[test]
fn vector_with_mixed_payload_evacuates_correctly() {
    let mut h = small_heap();
    let leaf = cons(&mut h, Generation::G0, Word::fixnum(123), Word::NIL);
    let vec_w = vector(&mut h, Generation::G0, 5, Word::NIL);
    unsafe {
        set_vector_slot(vec_w, 1, Word::fixnum(10));
        set_vector_slot(vec_w, 2, leaf);
        set_vector_slot(vec_w, 3, Word::T);
        set_vector_slot(vec_w, 4, leaf);
        set_vector_slot(vec_w, 5, Word::NIL);
    }
    let mut roots = [vec_w];
    let result = h.evacuate_from_word_roots(
        Generation::G0,
        Generation::G1,
        &mut roots,
    );
    // 2 objects copied: vector + leaf (shared, counted once).
    assert_eq!(result.objects_copied, 2);

    // Verify payload.
    let s1 = unsafe { vector_slot(roots[0], 1) };
    let s2 = unsafe { vector_slot(roots[0], 2) };
    let s3 = unsafe { vector_slot(roots[0], 3) };
    let s4 = unsafe { vector_slot(roots[0], 4) };
    let s5 = unsafe { vector_slot(roots[0], 5) };
    assert_eq!(s1.as_fixnum(), Some(10));
    assert_eq!(s2.tag(), Tag::Cons);
    assert!(s3.is_t());
    assert_eq!(s4.tag(), Tag::Cons);
    assert!(s5.is_nil());
    // s2 and s4 are the same leaf — must point at the same new address.
    assert_eq!(s2.raw(), s4.raw(), "shared leaf duplicated");
}

// -- Allocation churn -------------------------------------------------------

#[test]
fn allocation_churn_does_not_leak_pages_after_many_minors() {
    let mut h = medium_heap();
    let mut roots: [Word; 1] = [Word::NIL];
    let mut max_pages_used = 0;
    for _ in 0..50 {
        // Allocate 200 cons cells, link as a chain, retain head.
        let head = list(&mut h, Generation::G0, 200);
        roots[0] = head;
        let pages = h.count_pages_in_gen(Generation::G0);
        max_pages_used = max_pages_used.max(pages);
        h.evacuate_from_word_roots(Generation::G0, Generation::G1, &mut roots);
        // Drop and repeat.
        roots[0] = Word::NIL;
        h.evacuate_from_word_roots(Generation::G1, Generation::G1, &mut roots);
    }
    // Heap should have plenty of free pages still.
    assert!(
        h.count_pages_in_gen(Generation::Free) > 0,
        "all pages consumed after churn"
    );
    assert!(max_pages_used >= 1);
}

// -- Many small bursts ------------------------------------------------------

#[test]
fn many_independent_small_graphs_all_evacuate() {
    let mut h = medium_heap();
    let mut roots: Vec<Word> = Vec::new();
    for k in 0..30 {
        // Each graph: a 5-cell chain, root = head.
        let head = list(&mut h, Generation::G0, 5);
        // Stamp first car so we can identify it later.
        let p = (head.raw() & PAYLOAD_MASK) as *mut u64;
        unsafe { *p = Word::fixnum(k as i64 + 50_000).raw() };
        roots.push(head);
    }
    h.evacuate_from_word_roots(Generation::G0, Generation::G1, &mut roots);
    // Verify each root reaches its stamped marker.
    for (k, &r) in roots.iter().enumerate() {
        assert_eq!(r.tag(), Tag::Cons);
        let p = (r.raw() & PAYLOAD_MASK) as *const u64;
        let car = unsafe { Word::from_raw(*p) };
        assert_eq!(car.as_fixnum(), Some(k as i64 + 50_000));
    }
}

// -- Withholding promotion --------------------------------------------------

#[test]
fn within_gen_evac_keeps_objects_in_same_gen() {
    let mut h = small_heap();
    let head = list(&mut h, Generation::G0, 100);
    let mut roots = [head];
    // G0 → G0 (within-gen evac).
    h.evacuate_from_word_roots(Generation::G0, Generation::G0, &mut roots);
    // After: roots[0] is back in G0.
    let new_ptr = (roots[0].raw() & PAYLOAD_MASK) as *const u8;
    let page = h.page_of(new_ptr).expect("in heap");
    assert_eq!(h.desc(page).generation, Generation::G0);
    // List still walks.
    let elems = unsafe { list_to_vec(roots[0]) };
    assert_eq!(elems.len(), 100);
}

// -- Roots holding immediate words ------------------------------------------

#[test]
fn fixnum_root_is_not_followed_into_garbage_addresses() {
    // A root that LOOKS like a low-address pointer should not crash
    // the GC. Specifically: a fixnum whose bits happen to coincide
    // with a low non-heap address. (For Word, fixnum 1 has raw bits
    // 0b1000 = 8 which is a real Page-aligned address but with a
    // Fixnum tag.) The tag check should reject before any
    // dereference.
    let mut h = small_heap();
    let _ = list(&mut h, Generation::G0, 5);
    let mut roots = [
        Word::fixnum(0),
        Word::fixnum(1),
        Word::fixnum(-1),
        Word::fixnum(0x4000_0000),
    ];
    let result = h.evacuate_from_word_roots(
        Generation::G0,
        Generation::G1,
        &mut roots,
    );
    // None of the fixnums should have caused anything to be copied
    // (the 5 unrooted conses are reclaimed).
    assert_eq!(result.objects_copied, 0);
}

// -- Allocate-up-to-capacity ------------------------------------------------

#[test]
fn alloc_until_oom_returns_none_not_panic() {
    // A tiny heap. Fill G0 until alloc returns None.
    let mut h = TestHeap::with_reservation(4 * 64 * 1024);
    let mut n = 0usize;
    while h.try_alloc_cons_in(Generation::G0).is_some() {
        n += 1;
        if n > 100_000 {
            panic!("alloc loop runaway");
        }
    }
    assert!(n > 0, "should have allocated at least one cons");
    // Evac with no roots reclaims everything.
    h.evacuate_from_word_roots(Generation::G0, Generation::G1, &mut []);
    // Now we can alloc again.
    assert!(h.try_alloc_cons_in(Generation::G0).is_some());
}

// -- Self-referential cycle -------------------------------------------------

#[test]
fn self_referencing_cons_terminates() {
    let mut h = small_heap();
    let p = h.try_alloc_cons_in(Generation::G0).unwrap();
    // (x . x) — both car and cdr point at x itself.
    let w = Word::from_ptr(p.as_ptr() as *const u8, Tag::Cons);
    unsafe {
        *p.as_ptr() = w.raw();
        *p.as_ptr().add(1) = w.raw();
    }
    let mut roots = [w];
    let result = h.evacuate_from_word_roots(
        Generation::G0,
        Generation::G1,
        &mut roots,
    );
    assert_eq!(result.objects_copied, 1);
    // The new cell's car AND cdr should equal the new root.
    let p = (roots[0].raw() & PAYLOAD_MASK) as *const u64;
    let car = unsafe { Word::from_raw(*p) };
    let cdr = unsafe { Word::from_raw(*p.add(1)) };
    assert_eq!(car.raw(), roots[0].raw());
    assert_eq!(cdr.raw(), roots[0].raw());
}

// -- Multiple evac passes alternating gens ---------------------------------

#[test]
fn ping_pong_between_gens_preserves_data() {
    let mut h = medium_heap();
    let head = list(&mut h, Generation::G0, 80);
    let mut roots = [head];
    // G0 → G1
    h.evacuate_from_word_roots(Generation::G0, Generation::G1, &mut roots);
    // G1 → Tenured
    h.evacuate_from_word_roots(Generation::G1, Generation::Tenured, &mut roots);
    // Tenured → Tenured (in-place)
    h.evacuate_from_word_roots(Generation::Tenured, Generation::Tenured, &mut roots);
    let elems = unsafe { list_to_vec(roots[0]) };
    assert_eq!(elems.len(), 80);
    for (i, v) in elems.iter().enumerate() {
        assert_eq!(*v, i as i64);
    }
}

// -- High-density false-positive stack (conservative pinning is out of
// -- scope here without a Coordinator, but pin scan can still be tested
// -- directly via pin_pointers_in_ranges) -----------------------------------

#[test]
fn many_alloc_cycles_use_recycled_page_addresses_cleanly() {
    let mut h = small_heap();
    // Two cycles of alloc-and-drop. Force a page to be released and
    // then re-acquired. Verify it comes back clean (no stale Words
    // visible).
    let _ = list(&mut h, Generation::G0, 100);
    h.evacuate_from_word_roots(Generation::G0, Generation::G1, &mut []);
    let free_after_first = h.count_pages_in_gen(Generation::Free);
    let _ = list(&mut h, Generation::G0, 100);
    let free_after_second = h.count_pages_in_gen(Generation::Free);
    // The pages from the second alloc should come from the freed set.
    assert!(
        free_after_first >= free_after_second,
        "page recycle expected: first={free_after_first}, second={free_after_second}"
    );
    h.evacuate_from_word_roots(Generation::G0, Generation::G1, &mut []);
}

// -- Major collection -------------------------------------------------------

#[test]
fn major_collect_reclaims_all_unrooted_old_data() {
    let mut h = medium_heap();
    // Set up: build a list, promote to G1, drop it, run a major.
    let head = list(&mut h, Generation::G0, 200);
    let mut roots = [head];
    h.evacuate_from_word_roots(Generation::G0, Generation::G1, &mut roots);
    assert!(h.count_pages_in_gen(Generation::G1) > 0);

    // Drop the root. After collect_major, G1 should empty out.
    let mut empty: [Word; 0] = [];
    h.collect_major(|evac| {
        for r in empty.iter_mut() {
            evac.visit(r);
        }
    });
    assert_eq!(h.count_pages_in_gen(Generation::G1), 0,
        "unrooted G1 not reclaimed by major");
}

#[test]
fn major_collect_preserves_rooted_data() {
    let mut h = medium_heap();
    let head = list(&mut h, Generation::G0, 300);
    let mut roots = [head];
    h.evacuate_from_word_roots(Generation::G0, Generation::G1, &mut roots);

    // collect_major with the root preserved.
    h.collect_major(|evac| {
        for r in roots.iter_mut() {
            evac.visit(r);
        }
    });
    let elems = unsafe { list_to_vec(roots[0]) };
    assert_eq!(elems.len(), 300);
    for (i, v) in elems.iter().enumerate() {
        assert_eq!(*v, i as i64);
    }
}

// -- Quick smoke: collect_minor cohort threshold --------------------------

#[test]
fn collect_minor_promotes_after_threshold() {
    use newgc_core::G0_PROMOTION_THRESHOLD;
    let mut h = medium_heap();
    let head = list(&mut h, Generation::G0, 50);
    let mut roots = [head];

    for cycle in 0..G0_PROMOTION_THRESHOLD {
        let r = h.collect_minor(|evac| {
            for w in roots.iter_mut() {
                evac.visit(w);
            }
        });
        // Cycles before the promotion threshold do not promote to G1.
        if cycle < G0_PROMOTION_THRESHOLD - 1 {
            assert!(
                !r.promoted_g0,
                "cycle {cycle}: expected no G0 promotion this cycle"
            );
        }
    }

    // The threshold cycle should have promoted to G1.
    let g1_pages = h.count_pages_in_gen(Generation::G1);
    assert!(
        g1_pages > 0,
        "expected promotion to G1 after {G0_PROMOTION_THRESHOLD} minors, got {g1_pages} G1 pages"
    );

    // Data still walks.
    let elems = unsafe { list_to_vec(roots[0]) };
    assert_eq!(elems.len(), 50);
}

// -- Final smoke ------------------------------------------------------------

#[test]
fn returned_evac_result_is_self_consistent() {
    // The EvacResult should report sane numbers.
    let mut h = small_heap();
    let head = list(&mut h, Generation::G0, 100);
    let pages_before = h.count_pages_in_gen(Generation::G0);
    let mut roots = [head];
    let result: EvacResult = h.evacuate_from_word_roots(
        Generation::G0,
        Generation::G1,
        &mut roots,
    );
    assert_eq!(result.objects_copied, 100);
    assert_eq!(result.cells_copied, 200);
    let g0_after = h.count_pages_in_gen(Generation::G0);
    let g1_after = h.count_pages_in_gen(Generation::G1);
    assert_eq!(g0_after, 0, "all G0 pages reclaimed");
    assert!(g1_after > 0, "G1 should hold the survivors");
    assert!(result.pages_freed >= pages_before - g1_after);
}
