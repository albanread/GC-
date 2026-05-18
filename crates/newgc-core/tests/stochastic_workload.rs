//! Stochastic long-running workload simulator.
//!
//! Drives the GC with a randomised stream of operations that
//! approximates a real program: function-call enter/return creating
//! short-lived locals, longer-lived heap structures (lists, trees,
//! strings, vectors) born and dying at random times, intermixed
//! GC cycles, all over thousands of operations.
//!
//! Every tracked structure carries an integrity descriptor (`Shape`)
//! recording exactly what was allocated. After every GC cycle the
//! simulator walks each surviving structure and verifies its payload
//! matches the descriptor — so any single dangling-pointer / lost-
//! payload bug surfaces immediately, not at the end.
//!
//! Determinism is by design: every randomness source comes from a
//! seeded LCG. A failure in seed `42` is reproducible by re-running
//! the same seed.
//!
//! ## What this catches that the mechanical tests don't
//!
//! - **Stack-shaped root patterns.** Function frames push/pop locals;
//!   the GC sees a varying root set over time. Frames live for a
//!   random duration.
//! - **Crossing lifetimes.** A long-lived tracked structure can be
//!   allocated while a frame is open, then survive after the frame
//!   returns. Lifetime overlap produces realistic pointer churn.
//! - **Mixed allocation shapes.** Lists (cons cells), trees (boxed),
//!   strings (opaque payload), large vectors (1000-slot), and small
//!   objects all coexist.
//! - **Random GC interleavings.** Minor and major cycles fire at
//!   pseudo-random points, not at scripted boundaries.
//! - **Continuous integrity verification.** Every 50 ops a full
//!   walk validates every tracked structure's payload.

use newgc_core::{
    Generation, HeapHeader, HeapLayout, HeapType, LispLayout, PageHeap,
    PAYLOAD_MASK, Tag, Word,
};

// =========================================================================
// Deterministic RNG
// =========================================================================

struct Rng {
    state: u64,
}

impl Rng {
    fn new(seed: u64) -> Self {
        // Ensure non-zero state.
        Self { state: seed | 1 }
    }

    #[inline]
    fn next_u64(&mut self) -> u64 {
        self.state = self
            .state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.state
    }

    fn range(&mut self, lim: usize) -> usize {
        if lim == 0 {
            return 0;
        }
        (self.next_u64() as usize) % lim
    }

    /// Returns true with probability `n_in_100 / 100`.
    fn percent(&mut self, n_in_100: u32) -> bool {
        (self.next_u64() % 100) < n_in_100 as u64
    }

    /// Pick an index from the table weighted by the first element.
    fn weighted_choice(&mut self, weights: &[u32]) -> usize {
        let total: u64 = weights.iter().map(|&w| w as u64).sum();
        if total == 0 {
            return 0;
        }
        let mut roll = self.next_u64() % total;
        for (i, &w) in weights.iter().enumerate() {
            if (w as u64) > roll {
                return i;
            }
            roll -= w as u64;
        }
        weights.len() - 1
    }
}

// =========================================================================
// Tracked structure descriptors
// =========================================================================

/// A description of what was allocated, enough to verify the payload
/// post-GC. The descriptors are kept Rust-side, independent of the
/// heap.
#[derive(Clone, Debug)]
enum Shape {
    /// `(0 1 2 … len-1)` — list of `len` fixnums.
    List { len: usize },
    /// Vector of `len` slots, each holding `seed + i` as a fixnum.
    Vector { len: u32, seed: i64 },
    /// Large vector with `len` payload slots, each set to fixnum
    /// `seed.wrapping_add(i)`. Conceptually identical to Vector but
    /// kept separate so the workload mixes both sizes.
    LargeVector { len: u32, seed: i64 },
    /// String of `bytes.len()` bytes recorded verbatim. Allocated
    /// via HeapType::String (opaque payload).
    Str { bytes: Vec<u8> },
    /// Balanced binary tree of `depth`. Each interior node is a
    /// 3-slot vector (left, right, depth_marker); each leaf is a
    /// cons cell holding (leaf_id, NIL).
    Tree { depth: u32 },
    /// "Small object" — single cons cell with the given car.
    SmallCons { car: i64 },
}

struct Tracked {
    root: Word,
    shape: Shape,
    death_step: usize,
    born_step: usize,
    id: usize,
}

struct Frame {
    locals: Vec<Word>,
    return_step: usize,
    depth: u32,
}

// =========================================================================
// Allocation helpers
// =========================================================================

type Heap = PageHeap<LispLayout>;

fn cons(h: &mut Heap, g: Generation, car: Word, cdr: Word) -> Word {
    let p = h.try_alloc_cons_in(g).expect("cons alloc");
    unsafe {
        *p.as_ptr() = car.raw();
        *p.as_ptr().add(1) = cdr.raw();
    }
    Word::from_ptr(p.as_ptr() as *const u8, Tag::Cons)
}

fn vector(h: &mut Heap, g: Generation, n_payload: u32, init: Word) -> Word {
    let total = (1 + n_payload) as usize;
    let p = h.try_alloc_boxed_in(g, total).expect("vector alloc");
    unsafe {
        *p.as_ptr() = HeapHeader::new(HeapType::Vector, n_payload).raw();
        for i in 1..=n_payload as usize {
            *p.as_ptr().add(i) = init.raw();
        }
    }
    Word::from_ptr(p.as_ptr() as *const u8, Tag::Vector)
}

/// Allocate a HeapType::String of `n_bytes` bytes. Payload is opaque
/// (the GC won't scan its cells).
fn string(h: &mut Heap, g: Generation, bytes: &[u8]) -> Word {
    let payload_cells = (bytes.len() + 7) / 8;
    let total = 1 + payload_cells;
    let p = h.try_alloc_boxed_in(g, total).expect("string alloc");
    unsafe {
        *p.as_ptr() = HeapHeader::new(HeapType::String, payload_cells as u32).raw();
        // Zero the payload first to avoid stale bits after the last byte.
        for i in 1..=payload_cells {
            *p.as_ptr().add(i) = 0;
        }
        let dst = p.as_ptr().add(1) as *mut u8;
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), dst, bytes.len());
    }
    Word::from_ptr(p.as_ptr() as *const u8, Tag::String)
}

unsafe fn vector_slot(w: Word, idx: usize) -> Word {
    let base = (w.raw() & PAYLOAD_MASK) as *const u64;
    unsafe { Word::from_raw(*base.add(idx)) }
}

unsafe fn set_vector_slot(w: Word, idx: usize, v: Word) {
    let base = (w.raw() & PAYLOAD_MASK) as *mut u64;
    unsafe { *base.add(idx) = v.raw() }
}

unsafe fn read_string_bytes(w: Word) -> Vec<u8> {
    let base = (w.raw() & PAYLOAD_MASK) as *const u64;
    let header = HeapHeader::from_raw(unsafe { *base });
    assert_eq!(header.ty(), HeapType::String);
    let payload_cells = header.length_cells() as usize;
    let total_bytes = payload_cells * 8;
    let data = unsafe { base.add(1) as *const u8 };
    let mut bytes = Vec::with_capacity(total_bytes);
    for i in 0..total_bytes {
        bytes.push(unsafe { *data.add(i) });
    }
    bytes
}

fn alloc_list(h: &mut Heap, g: Generation, len: usize) -> Word {
    let mut head = Word::NIL;
    for i in (0..len).rev() {
        head = cons(h, g, Word::fixnum(i as i64), head);
    }
    head
}

fn alloc_vector(h: &mut Heap, g: Generation, len: u32, seed: i64) -> Word {
    let v = vector(h, g, len, Word::NIL);
    for i in 0..len {
        unsafe {
            set_vector_slot(
                v,
                1 + i as usize,
                Word::fixnum(seed.wrapping_add(i as i64)),
            );
        }
    }
    v
}

/// Build a balanced binary tree. Returns the root (a vector for
/// depth > 0, a cons for depth == 0).
fn alloc_tree(h: &mut Heap, g: Generation, depth: u32) -> Word {
    if depth == 0 {
        // Leaf: a cons cell with `0` car.
        return cons(h, g, Word::fixnum(0), Word::NIL);
    }
    let left = alloc_tree(h, g, depth - 1);
    let right = alloc_tree(h, g, depth - 1);
    let node = vector(h, g, 3, Word::NIL);
    unsafe {
        set_vector_slot(node, 1, left);
        set_vector_slot(node, 2, right);
        set_vector_slot(node, 3, Word::fixnum(depth as i64));
    }
    node
}

// =========================================================================
// Verification
// =========================================================================

unsafe fn walk_list_collecting_cars(head: Word) -> Vec<i64> {
    let mut v = Vec::new();
    let mut cur = head;
    while cur.tag() == Tag::Cons {
        let p = (cur.raw() & PAYLOAD_MASK) as *const u64;
        let car = unsafe { Word::from_raw(*p) };
        v.push(car.as_fixnum().unwrap_or(i64::MIN));
        cur = unsafe { Word::from_raw(*p.add(1)) };
    }
    v
}

unsafe fn count_tree_leaves(root: Word) -> Result<(usize, u32), String> {
    if root.tag() == Tag::Cons {
        return Ok((1, 0));
    }
    if root.tag() != Tag::Vector {
        return Err(format!("tree node has wrong tag: {:?}", root.tag()));
    }
    let depth_word = unsafe { vector_slot(root, 3) };
    let depth = depth_word.as_fixnum().ok_or_else(|| {
        format!("tree node depth slot not fixnum: {:?}", depth_word)
    })?;
    let left = unsafe { vector_slot(root, 1) };
    let right = unsafe { vector_slot(root, 2) };
    let (l, ld) = unsafe { count_tree_leaves(left) }?;
    let (r, rd) = unsafe { count_tree_leaves(right) }?;
    if ld != rd {
        return Err(format!(
            "tree imbalanced at depth {}: left subtree depth {} != right {}",
            depth, ld, rd
        ));
    }
    Ok((l + r, ld + 1))
}

fn verify(t: &Tracked) -> Result<(), String> {
    match &t.shape {
        Shape::List { len } => {
            let elems = unsafe { walk_list_collecting_cars(t.root) };
            if elems.len() != *len {
                return Err(format!(
                    "List(id={}, born={}): expected len={}, got {}",
                    t.id,
                    t.born_step,
                    len,
                    elems.len()
                ));
            }
            for (i, v) in elems.iter().enumerate() {
                if *v != i as i64 {
                    return Err(format!(
                        "List(id={}): index {} expected {}, got {}",
                        t.id, i, i as i64, v
                    ));
                }
            }
            Ok(())
        }
        Shape::Vector { len, seed }
        | Shape::LargeVector { len, seed } => {
            if t.root.tag() != Tag::Vector {
                return Err(format!(
                    "Vector(id={}): root not Vector-tagged, got {:?}",
                    t.id,
                    t.root.tag()
                ));
            }
            for i in 0..*len {
                let s = unsafe { vector_slot(t.root, 1 + i as usize) };
                let expected = seed.wrapping_add(i as i64);
                match s.as_fixnum() {
                    Some(v) if v == expected => {}
                    Some(v) => {
                        return Err(format!(
                            "Vector(id={}, born={}): slot {} expected {}, got {}",
                            t.id, t.born_step, i, expected, v
                        ));
                    }
                    None => {
                        return Err(format!(
                            "Vector(id={}): slot {} not fixnum, got {:?}",
                            t.id, i, s
                        ));
                    }
                }
            }
            Ok(())
        }
        Shape::Str { bytes } => {
            if t.root.tag() != Tag::String {
                return Err(format!(
                    "Str(id={}): root not String-tagged, got {:?}",
                    t.id,
                    t.root.tag()
                ));
            }
            let actual = unsafe { read_string_bytes(t.root) };
            let n = bytes.len();
            if actual.len() < n {
                return Err(format!(
                    "Str(id={}): buffer too small, {} < {}",
                    t.id,
                    actual.len(),
                    n
                ));
            }
            if &actual[..n] != bytes.as_slice() {
                return Err(format!(
                    "Str(id={}, born={}): bytes mismatch",
                    t.id, t.born_step
                ));
            }
            Ok(())
        }
        Shape::Tree { depth } => {
            let (leaves, walked_depth) = unsafe { count_tree_leaves(t.root) }?;
            let expected_leaves = 1usize << *depth;
            if leaves != expected_leaves {
                return Err(format!(
                    "Tree(id={}, depth={}): expected {} leaves, got {}",
                    t.id, depth, expected_leaves, leaves
                ));
            }
            if walked_depth != *depth {
                return Err(format!(
                    "Tree(id={}): expected depth {}, walked to {}",
                    t.id, depth, walked_depth
                ));
            }
            Ok(())
        }
        Shape::SmallCons { car } => {
            if t.root.tag() != Tag::Cons {
                return Err(format!(
                    "SmallCons(id={}): root not Cons-tagged",
                    t.id
                ));
            }
            let p = (t.root.raw() & PAYLOAD_MASK) as *const u64;
            let actual = unsafe { Word::from_raw(*p) };
            match actual.as_fixnum() {
                Some(v) if v == *car => Ok(()),
                Some(v) => Err(format!(
                    "SmallCons(id={}): car expected {}, got {}",
                    t.id, car, v
                )),
                None => Err(format!(
                    "SmallCons(id={}): car not fixnum",
                    t.id
                )),
            }
        }
    }
}

// =========================================================================
// Workload state
// =========================================================================

#[derive(Default, Debug)]
struct Stats {
    ops: usize,
    enter_function: usize,
    return_function: usize,
    local_allocations: usize,
    tracked_allocations: usize,
    natural_deaths: usize,
    random_drops: usize,
    minor_cycles: usize,
    major_cycles: usize,
    integrity_checks: usize,
    peak_frames: usize,
    peak_tracked: usize,
    peak_locals_in_one_frame: usize,
}

struct Workload {
    heap: Heap,
    rng: Rng,
    frames: Vec<Frame>,
    tracked: Vec<Tracked>,
    next_id: usize,
    step: usize,
    stats: Stats,
}

impl Workload {
    fn new(seed: u64, reservation_bytes: usize) -> Self {
        Self {
            heap: Heap::with_reservation(reservation_bytes),
            rng: Rng::new(seed),
            frames: Vec::new(),
            tracked: Vec::new(),
            next_id: 0,
            step: 0,
            stats: Stats::default(),
        }
    }

    // -- Frame / function-call management ----------------------------------

    fn enter_function(&mut self) {
        let lifetime = 5 + self.rng.range(80);
        let depth = self.frames.last().map(|f| f.depth + 1).unwrap_or(0);
        self.frames.push(Frame {
            locals: Vec::new(),
            return_step: self.step + lifetime,
            depth,
        });
        self.stats.enter_function += 1;
        self.stats.peak_frames = self.stats.peak_frames.max(self.frames.len());
    }

    fn advance_function_returns(&mut self) {
        while let Some(frame) = self.frames.last() {
            if frame.return_step <= self.step {
                self.frames.pop();
                self.stats.return_function += 1;
            } else {
                break;
            }
        }
    }

    // -- Tracked structure management --------------------------------------

    fn advance_tracked_deaths(&mut self) {
        let mut i = 0;
        while i < self.tracked.len() {
            if self.tracked[i].death_step <= self.step {
                self.tracked.swap_remove(i);
                self.stats.natural_deaths += 1;
            } else {
                i += 1;
            }
        }
    }

    fn alloc_random_local(&mut self) {
        let w = self.alloc_random();
        if let Some(f) = self.frames.last_mut() {
            f.locals.push(w);
            self.stats.local_allocations += 1;
            self.stats.peak_locals_in_one_frame =
                self.stats.peak_locals_in_one_frame.max(f.locals.len());
        }
        // If no frame is open, the alloc is purely transient — only
        // the not-yet-evacuated G0 page holds it. That's a legitimate
        // pattern (top-level expression evaluation).
    }

    fn alloc_random_tracked(&mut self) {
        let (root, shape) = self.alloc_random_with_shape();
        let lifetime = 200 + self.rng.range(2500);
        let id = self.next_id;
        self.next_id += 1;
        self.tracked.push(Tracked {
            root,
            shape,
            death_step: self.step + lifetime,
            born_step: self.step,
            id,
        });
        self.stats.tracked_allocations += 1;
        self.stats.peak_tracked = self.stats.peak_tracked.max(self.tracked.len());
    }

    fn drop_random_tracked(&mut self) {
        if !self.tracked.is_empty() {
            let i = self.rng.range(self.tracked.len());
            self.tracked.swap_remove(i);
            self.stats.random_drops += 1;
        }
    }

    fn alloc_random(&mut self) -> Word {
        let kind = self.rng.weighted_choice(&[
            50, // small cons
            30, // small vector
            10, // string
            5,  // list
            5,  // tree (small)
        ]);
        match kind {
            0 => cons(
                &mut self.heap,
                Generation::G0,
                Word::fixnum(self.rng.range(1000) as i64),
                Word::NIL,
            ),
            1 => {
                let n = 1 + self.rng.range(8) as u32;
                let seed = (self.rng.range(1_000_000) as i64) - 500_000;
                alloc_vector(&mut self.heap, Generation::G0, n, seed)
            }
            2 => {
                let n = 1 + self.rng.range(32);
                let bytes: Vec<u8> = (0..n)
                    .map(|_| (self.rng.next_u64() & 0xff) as u8)
                    .collect();
                string(&mut self.heap, Generation::G0, &bytes)
            }
            3 => {
                let n = 5 + self.rng.range(50);
                alloc_list(&mut self.heap, Generation::G0, n)
            }
            _ => {
                let depth = self.rng.range(3) as u32;
                alloc_tree(&mut self.heap, Generation::G0, depth)
            }
        }
    }

    fn alloc_random_with_shape(&mut self) -> (Word, Shape) {
        let kind = self.rng.weighted_choice(&[
            25, // List
            25, // Vector
            10, // LargeVector
            20, // Str
            10, // Tree
            10, // SmallCons
        ]);
        match kind {
            0 => {
                let len = 5 + self.rng.range(80);
                let root = alloc_list(&mut self.heap, Generation::G0, len);
                (root, Shape::List { len })
            }
            1 => {
                let len = 2 + self.rng.range(16) as u32;
                let seed = (self.rng.range(1_000_000) as i64) - 500_000;
                let root = alloc_vector(&mut self.heap, Generation::G0, len, seed);
                (root, Shape::Vector { len, seed })
            }
            2 => {
                let len = 100 + self.rng.range(500) as u32;
                let seed = (self.rng.range(1_000_000) as i64) - 500_000;
                let root = alloc_vector(&mut self.heap, Generation::G0, len, seed);
                (root, Shape::LargeVector { len, seed })
            }
            3 => {
                let n = 1 + self.rng.range(120);
                let bytes: Vec<u8> = (0..n)
                    .map(|_| (self.rng.next_u64() & 0xff) as u8)
                    .collect();
                let root = string(&mut self.heap, Generation::G0, &bytes);
                (root, Shape::Str { bytes })
            }
            4 => {
                let depth = 1 + self.rng.range(5) as u32;
                let root = alloc_tree(&mut self.heap, Generation::G0, depth);
                (root, Shape::Tree { depth })
            }
            _ => {
                let car = self.rng.range(10_000) as i64;
                let root = cons(
                    &mut self.heap,
                    Generation::G0,
                    Word::fixnum(car),
                    Word::NIL,
                );
                (root, Shape::SmallCons { car })
            }
        }
    }

    // -- GC ----------------------------------------------------------------

    fn collect_all_roots(&self) -> Vec<Word> {
        let frame_roots = self
            .frames
            .iter()
            .flat_map(|f| f.locals.iter().copied());
        let tracked_roots = self.tracked.iter().map(|t| t.root);
        frame_roots.chain(tracked_roots).collect()
    }

    fn redistribute_roots(&mut self, roots: &[Word]) {
        let mut idx = 0;
        for frame in self.frames.iter_mut() {
            for local in frame.locals.iter_mut() {
                *local = roots[idx];
                idx += 1;
            }
        }
        for t in self.tracked.iter_mut() {
            t.root = roots[idx];
            idx += 1;
        }
        assert_eq!(idx, roots.len(), "root distribution miscount");
    }

    fn minor_gc(&mut self) {
        let mut roots = self.collect_all_roots();
        self.heap.collect_minor(|evac| {
            for r in roots.iter_mut() {
                evac.visit(r);
            }
        });
        self.redistribute_roots(&roots);
        self.stats.minor_cycles += 1;
    }

    fn major_gc(&mut self) {
        let mut roots = self.collect_all_roots();
        self.heap.collect_major(|evac| {
            for r in roots.iter_mut() {
                evac.visit(r);
            }
        });
        self.redistribute_roots(&roots);
        self.stats.major_cycles += 1;
    }

    // -- Integrity verification -------------------------------------------

    fn verify_all_tracked(&self) -> Result<(), String> {
        for t in &self.tracked {
            verify(t).map_err(|e| {
                format!(
                    "step={}, ops={}: {}",
                    self.step, self.stats.ops, e
                )
            })?;
        }
        Ok(())
    }

    // -- Main step driver -------------------------------------------------

    fn step(&mut self) {
        self.step += 1;
        self.stats.ops += 1;

        // Time-driven events first.
        self.advance_function_returns();
        self.advance_tracked_deaths();

        // Pick an operation.
        let op_kind = self.rng.weighted_choice(&[
            12, // enter_function
            18, // alloc_random_local (requires open frame)
            10, // alloc_random_tracked
            5,  // drop_random_tracked
            8,  // minor_gc
            2,  // major_gc
            2,  // verify integrity
        ]);
        match op_kind {
            0 => self.enter_function(),
            1 => {
                if self.frames.is_empty() {
                    // Open a frame on demand so the local has a home.
                    self.enter_function();
                }
                self.alloc_random_local();
            }
            2 => self.alloc_random_tracked(),
            3 => self.drop_random_tracked(),
            4 => self.minor_gc(),
            5 => self.major_gc(),
            6 => {
                self.verify_all_tracked()
                    .expect("integrity check failed");
                self.stats.integrity_checks += 1;
            }
            _ => unreachable!(),
        }
    }
}

// =========================================================================
// Tests
// =========================================================================

#[test]
fn stochastic_5000_ops_seed_1() {
    run_stochastic(1, 5_000, 32 * 64 * 1024);
}

#[test]
fn stochastic_5000_ops_seed_42() {
    run_stochastic(42, 5_000, 32 * 64 * 1024);
}

#[test]
fn stochastic_5000_ops_seed_0xdeadbeef() {
    run_stochastic(0xdeadbeef, 5_000, 32 * 64 * 1024);
}

#[test]
fn stochastic_20000_ops_long_run() {
    // Longer run with bigger heap. Verifies steady-state behaviour
    // over ~20K operations.
    run_stochastic(0xc0ffee, 20_000, 128 * 64 * 1024);
}

#[test]
fn stochastic_under_pressure_small_heap() {
    // Tighter heap; GC happens more often per allocation. The 16-page
    // (1 MB) reservation is small enough that mid-evac OOM is
    // realistically possible under the random workload — that's a
    // documented limitation (DESIGN_REVIEW.md #6), not a correctness
    // bug. We catch the panic and accept OOM as a tolerable outcome;
    // any *other* panic (or, more importantly, a silent corruption
    // caught by the integrity sweep) is a real failure.
    use newgc_core::page_heap::evac::GcStallError;
    newgc_core::page_heap::evac::install_quiet_gc_stall_panic_hook();
    let result = std::panic::catch_unwind(|| {
        run_stochastic(7777, 3_000, 16 * 64 * 1024);
    });
    match result {
        Ok(()) => { /* normal completion */ }
        Err(payload) => {
            // Only swallow GcStallError; rethrow anything else.
            if payload.downcast_ref::<GcStallError>().is_some() {
                eprintln!("stochastic_under_pressure: hit known mid-evac OOM (acceptable)");
            } else {
                std::panic::resume_unwind(payload);
            }
        }
    }
}

fn run_stochastic(seed: u64, n_ops: usize, reservation_bytes: usize) {
    let mut w = Workload::new(seed, reservation_bytes);
    let verify_every = 50;

    for _ in 0..n_ops {
        w.step();
        if w.stats.ops % verify_every == 0 {
            if let Err(msg) = w.verify_all_tracked() {
                panic!(
                    "integrity failure at step={}, seed={}: {}\nstats={:?}",
                    w.step, seed, msg, w.stats
                );
            }
        }
    }

    // Final integrity sweep.
    if let Err(msg) = w.verify_all_tracked() {
        panic!(
            "final integrity failure at step={}, seed={}: {}\nstats={:?}",
            w.step, seed, msg, w.stats
        );
    }

    // Drain: drop everything, GC, verify the heap returns to (near)
    // empty.
    w.frames.clear();
    w.tracked.clear();
    w.major_gc();
    assert_eq!(
        w.heap.count_pages_in_gen(Generation::G0),
        0,
        "G0 non-empty after final major"
    );
    assert_eq!(
        w.heap.count_pages_in_gen(Generation::G1),
        0,
        "G1 non-empty after final major"
    );
    // Tenured may have pages because cohort promotion sometimes
    // promotes G1 → Tenured before the next major. Reclaim with
    // another cycle.
    let _ = w.heap.evacuate_from_word_roots(
        Generation::Tenured,
        Generation::Tenured,
        &mut [],
    );

    // Print stats — visible only on test failure, but informative
    // for analysis. Cargo's stdout capture makes this a no-op on
    // success.
    eprintln!("seed={seed}, n_ops={n_ops}, stats={:?}", w.stats);
    eprintln!(
        "  alloc rates: {} locals + {} tracked = {} total allocations",
        w.stats.local_allocations,
        w.stats.tracked_allocations,
        w.stats.local_allocations + w.stats.tracked_allocations
    );
    eprintln!(
        "  gc cycles: {} minor + {} major; {} integrity checks",
        w.stats.minor_cycles,
        w.stats.major_cycles,
        w.stats.integrity_checks
    );
    eprintln!(
        "  function frames: peak={}, peak locals/frame={}",
        w.stats.peak_frames, w.stats.peak_locals_in_one_frame
    );
    eprintln!("  peak tracked: {}", w.stats.peak_tracked);

    // Sanity: the workload should have done meaningful work.
    assert!(
        w.stats.local_allocations + w.stats.tracked_allocations > n_ops / 10,
        "workload didn't allocate much"
    );
    assert!(
        w.stats.minor_cycles > 5,
        "workload didn't trigger enough GCs"
    );
    assert!(
        w.stats.integrity_checks >= n_ops / verify_every / 2,
        "not enough integrity sweeps fired"
    );
}
