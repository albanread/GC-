//! Runtime values for the mini-Lisp test driver.
//!
//! Heap-resident shapes (Pair, Vector, Str) hold a `Word` that
//! tags a pointer into the GC heap. Immediate shapes (Number,
//! Bool, Nil, Symbol) live entirely in Rust memory.
//!
//! The GC sees only the `Word` slots. When the evaluator's
//! environment is walked at a safepoint, every `Word` inside every
//! `Value` is fed through `PageEvacuator::visit`.

use std::rc::Rc;

use newgc_core::page_heap::space::PageHeap;
use newgc_core::{
    Generation, HeapHeader, HeapType, LispLayout, PAYLOAD_MASK, Tag, Word,
};

pub type Heap = PageHeap<LispLayout>;

/// Runtime value. Heap-residence is per-variant.
///
/// Note: we keep no `Function` / `Closure` value variants. Functions
/// are looked up by name in a separate table; this keeps GC walking
/// trivially acyclic.
#[derive(Clone, Debug)]
pub enum Value {
    Number(i64),
    Bool(bool),
    Nil,
    Symbol(Rc<String>),
    /// Tagged pointer to a heap cons cell.
    Pair(Word),
    /// Tagged pointer to a heap Vector.
    Vector(Word),
    /// Tagged pointer to a heap String (opaque payload).
    Str(Word),
}

impl Value {
    /// Apply `f` to every `Word` slot held by this value. Used by
    /// the GC root walk to rewrite slots after evacuation.
    pub fn for_each_word_mut(&mut self, f: &mut dyn FnMut(&mut Word)) {
        match self {
            Value::Pair(w) | Value::Vector(w) | Value::Str(w) => f(w),
            _ => {}
        }
    }

    pub fn is_truthy(&self) -> bool {
        !matches!(self, Value::Bool(false) | Value::Nil)
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Number(_) => "number",
            Value::Bool(_) => "bool",
            Value::Nil => "nil",
            Value::Symbol(_) => "symbol",
            Value::Pair(_) => "pair",
            Value::Vector(_) => "vector",
            Value::Str(_) => "string",
        }
    }
}

// =========================================================================
// Heap-allocation helpers (call out to newgc-core).
// =========================================================================

pub fn alloc_pair(heap: &mut Heap, car: Value, cdr: Value) -> Word {
    let p = heap
        .try_alloc_cons_in(Generation::G0)
        .expect("cons alloc failed");
    let car_word = value_to_word(car);
    let cdr_word = value_to_word(cdr);
    unsafe {
        *p.as_ptr() = car_word.raw();
        *p.as_ptr().add(1) = cdr_word.raw();
    }
    // Card barrier: mark the new cell's card. The cdr in particular
    // may be a heap pointer to a previously-promoted G1 cell (when
    // the mutator's chain build straddled a promoting minor cycle).
    // Without this mark, the major GC's G1→Tenured pass can't find
    // the G1 target via the G0 cell — only via this card.
    heap.mark_card_at(p.as_ptr() as *const u8);
    Word::from_ptr(p.as_ptr() as *const u8, Tag::Cons)
}

/// Allocate an `n`-slot vector with every payload slot initialised
/// to `init`'s heap encoding.
pub fn alloc_vector(heap: &mut Heap, n_payload: u32, init: Value) -> Word {
    let total = (1 + n_payload) as usize;
    let p = heap
        .try_alloc_boxed_in(Generation::G0, total)
        .expect("vector alloc failed");
    let init_word = value_to_word(init);
    unsafe {
        *p.as_ptr() = HeapHeader::new(HeapType::Vector, n_payload).raw();
        for i in 1..=n_payload as usize {
            *p.as_ptr().add(i) = init_word.raw();
        }
    }
    Word::from_ptr(p.as_ptr() as *const u8, Tag::Vector)
}

pub fn alloc_string(heap: &mut Heap, bytes: &[u8]) -> Word {
    let payload_cells = bytes.len().div_ceil(8);
    let total = 1 + payload_cells;
    let p = heap
        .try_alloc_boxed_in(Generation::G0, total)
        .expect("string alloc failed");
    unsafe {
        *p.as_ptr() = HeapHeader::new(HeapType::String, payload_cells as u32).raw();
        // Zero the trailing bytes so the unused tail isn't garbage.
        for i in 1..=payload_cells {
            *p.as_ptr().add(i) = 0;
        }
        let dst = p.as_ptr().add(1) as *mut u8;
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), dst, bytes.len());
    }
    Word::from_ptr(p.as_ptr() as *const u8, Tag::String)
}

// =========================================================================
// Heap-pointer to-and-from Value conversions.
// =========================================================================

/// Encode an immediate `Value` as a tagged `Word` for storage in
/// a heap cell. Heap-resident `Value`s (Pair/Vector/Str) already
/// carry a Word; for them this is the identity.
pub fn value_to_word(v: Value) -> Word {
    match v {
        Value::Number(n) => Word::fixnum(n),
        Value::Bool(true) => Word::T,
        Value::Bool(false) => Word::NIL,
        Value::Nil => Word::NIL,
        Value::Symbol(_) => {
            // Symbols are immediate in Rust memory; we never store
            // them inside heap cells. If a Lisp program tries to
            // do that we panic — there's no symbol-to-Word path.
            panic!("symbols cannot be stored in heap cells in this mini-Lisp")
        }
        Value::Pair(w) | Value::Vector(w) | Value::Str(w) => w,
    }
}

/// Decode a `Word` read from a heap cell into a `Value`. The tag
/// bits drive the dispatch.
///
/// Note: this is irreversible for symbols (we never store them in
/// heap cells), and Bool / Nil are conflated under the immediate
/// tags. The convention: Word::T → Bool(true); Word::NIL →
/// Nil (the Lisp value), since these are the only two immediates
/// the evaluator stores in heap cells.
pub fn word_to_value(w: Word) -> Value {
    match w.tag() {
        Tag::Fixnum => Value::Number(w.as_fixnum().unwrap()),
        Tag::Cons => Value::Pair(w),
        Tag::Vector => Value::Vector(w),
        Tag::String => Value::Str(w),
        Tag::Immediate => {
            if w.is_t() {
                Value::Bool(true)
            } else if w.is_nil() {
                Value::Nil
            } else {
                // chars, unbound markers, etc.
                Value::Nil
            }
        }
        // We don't allocate Symbols or Functions into heap cells.
        Tag::Symbol | Tag::Function => Value::Nil,
        Tag::Forward => {
            panic!("read a forwarding marker outside the GC — impossible")
        }
    }
}

/// Read the car of a Pair value.
pub fn pair_car(w: Word) -> Value {
    debug_assert_eq!(w.tag(), Tag::Cons);
    let p = (w.raw() & PAYLOAD_MASK) as *const u64;
    word_to_value(unsafe { Word::from_raw(*p) })
}

pub fn pair_cdr(w: Word) -> Value {
    debug_assert_eq!(w.tag(), Tag::Cons);
    let p = (w.raw() & PAYLOAD_MASK) as *const u64;
    word_to_value(unsafe { Word::from_raw(*p.add(1)) })
}

pub fn vector_len(w: Word) -> usize {
    debug_assert_eq!(w.tag(), Tag::Vector);
    let p = (w.raw() & PAYLOAD_MASK) as *const u64;
    let header = HeapHeader::from_raw(unsafe { *p });
    header.length_cells() as usize
}

pub fn vector_ref(w: Word, idx: usize) -> Value {
    let n = vector_len(w);
    if idx >= n {
        panic!("vector-ref: index {idx} out of bounds (len {n})");
    }
    let p = (w.raw() & PAYLOAD_MASK) as *const u64;
    word_to_value(unsafe { Word::from_raw(*p.add(1 + idx)) })
}

pub fn vector_set(heap: &Heap, w: Word, idx: usize, v: Value) {
    let n = vector_len(w);
    if idx >= n {
        panic!("vector-set!: index {idx} out of bounds (len {n})");
    }
    let p = (w.raw() & PAYLOAD_MASK) as *mut u64;
    let raw = value_to_word(v).raw();
    let slot_addr = unsafe { p.add(1 + idx) };
    unsafe { *slot_addr = raw };
    // Soft card barrier (sub-phase 9): mark the card containing the
    // slot we just wrote. Unconditional — if the vector is young, the
    // dirty card costs one extra card-scan visit during the next
    // minor; if it's old, the mark is what keeps the new pointer
    // findable.
    heap.mark_card_at(slot_addr as *const u8);
}

pub fn string_bytes(w: Word) -> Vec<u8> {
    debug_assert_eq!(w.tag(), Tag::String);
    let p = (w.raw() & PAYLOAD_MASK) as *const u64;
    let header = HeapHeader::from_raw(unsafe { *p });
    let payload_cells = header.length_cells() as usize;
    let total_bytes = payload_cells * 8;
    let data = unsafe { p.add(1) as *const u8 };
    let mut bytes = Vec::with_capacity(total_bytes);
    for i in 0..total_bytes {
        let b = unsafe { *data.add(i) };
        bytes.push(b);
    }
    // Trim trailing zeros to recover the canonical string length.
    while bytes.last() == Some(&0) {
        bytes.pop();
    }
    bytes
}

pub fn string_len(w: Word) -> usize {
    string_bytes(w).len()
}

/// Walk a heap pair, gathering all elements into a Rust Vec. Errors
/// out on improper lists (cdr that isn't Pair or Nil).
pub fn pair_to_vec(w: Word) -> Vec<Value> {
    let mut out = Vec::new();
    let mut cur = Value::Pair(w);
    loop {
        match cur {
            Value::Pair(p) => {
                out.push(pair_car(p));
                cur = pair_cdr(p);
            }
            Value::Nil => break,
            _ => panic!("improper list: tail = {:?}", cur),
        }
    }
    out
}

/// Walk a list-shaped Value, counting length. Returns `None` for
/// non-list values.
pub fn list_length(v: &Value) -> Option<usize> {
    let mut n = 0;
    let mut cur = v.clone();
    loop {
        match cur {
            Value::Pair(p) => {
                n += 1;
                cur = pair_cdr(p);
            }
            Value::Nil => return Some(n),
            _ => return None,
        }
    }
}
