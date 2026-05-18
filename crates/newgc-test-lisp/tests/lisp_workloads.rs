//! Lisp-driven workloads — actual programs that exercise the GC
//! through allocation patterns the test author writes once in
//! Lisp instead of dozens of times in Rust.
//!
//! Each test is a small Lisp program that:
//!   1. Allocates substantial heap data (lists, trees, strings,
//!      vectors).
//!   2. Triggers GCs at meaningful points (`gc-now`, `gc-major`).
//!   3. Verifies the result post-GC — any corruption surfaces as
//!      a wrong final answer or a panic from a downstream
//!      builtin (`car` of a non-pair, etc.).
//!
//! The interpreter's auto-trigger threshold is set low so multiple
//! GCs fire even within a single test.

use newgc_test_lisp::{Interpreter, Value};

fn small_interp() -> Interpreter {
    let mut i = Interpreter::new(8 * 64 * 1024);
    i.set_minor_threshold(200);
    i.set_majors_every(5);
    i
}

fn medium_interp() -> Interpreter {
    let mut i = Interpreter::new(32 * 64 * 1024);
    i.set_minor_threshold(500);
    i.set_majors_every(8);
    i
}

fn expect_number(v: Value) -> i64 {
    match v {
        Value::Number(n) => n,
        other => panic!("expected number, got {other:?}"),
    }
}

// =========================================================================
// List allocation patterns
// =========================================================================

#[test]
fn build_long_list_via_recursion() {
    let mut i = small_interp();  // lower threshold so auto-trigger fires
    // Depth 200 stays well clear of the Rust 1 MB stack limit.
    let v = i.run_source(r#"
        (define (count-down n)
          (if (= n 0)
              nil
              (cons n (count-down (- n 1)))))
        (define xs (count-down 200))
        (length xs)
    "#).unwrap();
    assert_eq!(expect_number(v), 200);
    let n_gcs = i.stats.minor_gcs + i.stats.major_gcs;
    assert!(n_gcs > 0, "no GC fired during 200-cell list build");
    // Force a few more GCs, list must still walk.
    i.run_source("(gc-major)").unwrap();
    i.run_source("(gc-major)").unwrap();
    let v = i.run_source("(length xs)").unwrap();
    assert_eq!(expect_number(v), 200);
}

#[test]
fn fold_a_long_list_verifies_each_element() {
    let mut i = medium_interp();
    let v = i.run_source(r#"
        (define (build n)
          (if (= n 0)
              nil
              (cons n (build (- n 1)))))
        (define (sum xs)
          (if (null? xs)
              0
              (+ (car xs) (sum (cdr xs)))))
        (sum (build 100))
    "#).unwrap();
    // sum of 1..100 = 5050
    assert_eq!(expect_number(v), 5050);
}

#[test]
fn nested_list_construction() {
    let mut i = medium_interp();
    // Build a list whose elements are themselves lists.
    let v = i.run_source(r#"
        (define (build-row k)
          (if (= k 0) nil (cons k (build-row (- k 1)))))
        (define (build-table n)
          (if (= n 0) nil (cons (build-row 5) (build-table (- n 1)))))
        (define table (build-table 20))
        ; sum across all rows
        (define (sum-row xs)
          (if (null? xs) 0 (+ (car xs) (sum-row (cdr xs)))))
        (define (sum-all xss)
          (if (null? xss) 0 (+ (sum-row (car xss)) (sum-all (cdr xss)))))
        (sum-all table)
    "#).unwrap();
    // 20 rows of sum(1..5)=15 each = 300
    assert_eq!(expect_number(v), 300);
}

// =========================================================================
// Tree-shaped allocation
// =========================================================================

#[test]
fn balanced_tree_construction_and_walk() {
    let mut i = medium_interp();
    let v = i.run_source(r#"
        ; Each node is a vector [value, left, right].
        (define (make-leaf v)
          (vector v nil nil))
        (define (make-node v l r)
          (vector v l r))
        (define (build depth value)
          (if (= depth 0)
              (make-leaf value)
              (make-node value
                        (build (- depth 1) (- value 1))
                        (build (- depth 1) (+ value 1)))))
        (define t (build 6 100))
        ; Walk: count nodes.
        (define (count n)
          (if (null? n)
              0
              (+ 1
                 (count (vector-ref n 1))
                 (count (vector-ref n 2)))))
        (count t)
    "#).unwrap();
    // Depth 6 → 2^7 - 1 = 127 nodes.
    assert_eq!(expect_number(v), 127);
    let v = i.run_source("(vector-ref t 0)").unwrap();
    assert_eq!(expect_number(v), 100);
}

#[test]
fn tree_walk_after_explicit_gc() {
    let mut i = medium_interp();
    i.run_source(r#"
        (define (build d)
          (if (= d 0)
              (vector 1 nil nil)
              (vector d (build (- d 1)) (build (- d 1)))))
        (define t (build 5))
    "#).unwrap();
    // Force several major GCs.
    for _ in 0..5 {
        i.run_source("(gc-major)").unwrap();
    }
    let v = i.run_source(r#"
        (define (count n)
          (if (null? n)
              0
              (+ 1
                 (count (vector-ref n 1))
                 (count (vector-ref n 2)))))
        (count t)
    "#).unwrap();
    assert_eq!(expect_number(v), 63);  // 2^6 - 1
}

// =========================================================================
// String-heavy
// =========================================================================

#[test]
fn many_strings_in_a_list() {
    let mut i = medium_interp();
    let v = i.run_source(r#"
        ; Build a list of 50 strings; sum the lengths.
        (define (build n)
          (if (= n 0)
              nil
              (cons "hello world" (build (- n 1)))))
        (define ss (build 50))
        (define (total-length xs)
          (if (null? xs)
              0
              (+ (string-length (car xs))
                 (total-length (cdr xs)))))
        (total-length ss)
    "#).unwrap();
    // 50 × 11 = 550
    assert_eq!(expect_number(v), 550);
}

// =========================================================================
// Recursion depth / call frame stress
// =========================================================================

#[test]
fn deep_recursion_holds_intermediate_values() {
    let mut i = medium_interp();
    let v = i.run_source(r#"
        ; A recursive function that holds a list partially-built in
        ; every frame. Tests root scanning over many active frames.
        (define (loop n acc)
          (if (= n 0)
              (length acc)
              (loop (- n 1) (cons n acc))))
        (loop 300 nil)
    "#).unwrap();
    assert_eq!(expect_number(v), 300);
}

#[test]
fn mutual_recursion_with_allocations() {
    let mut i = medium_interp();
    let v = i.run_source(r#"
        (define (build-even n)
          (if (= n 0)
              nil
              (cons n (build-odd (- n 1)))))
        (define (build-odd n)
          (if (= n 0)
              nil
              (cons n (build-even (- n 1)))))
        (length (build-even 100))
    "#).unwrap();
    assert_eq!(expect_number(v), 100);
}

// =========================================================================
// Mutation patterns
// =========================================================================

#[test]
fn vector_mutate_in_loop_then_walk() {
    let mut i = medium_interp();
    let v = i.run_source(r#"
        (define v (make-vector 100 0))
        ; Fill via repeated set!
        (define (fill-from i)
          (if (= i 100)
              nil
              (begin
                (vector-set! v i (* i i))
                (fill-from (+ i 1)))))
        (fill-from 0)
        (gc-major)
        ; Sum every slot.
        (define (sum-from i)
          (if (= i 100)
              0
              (+ (vector-ref v i) (sum-from (+ i 1)))))
        (sum-from 0)
    "#).unwrap();
    // sum of i*i for i in 0..100 = 328350
    assert_eq!(expect_number(v), 328350);
}

#[test]
fn store_pointers_into_vector_then_gc() {
    let mut i = medium_interp();
    let v = i.run_source(r#"
        (define v (make-vector 20 nil))
        ; Each slot holds a fresh list of length i.
        (define (build-row n)
          (if (= n 0) nil (cons n (build-row (- n 1)))))
        (define (fill i)
          (if (= i 20)
              nil
              (begin
                (vector-set! v i (build-row (+ i 1)))
                (fill (+ i 1)))))
        (fill 0)
        (gc-major)
        (gc-major)
        ; Sum every list's length.
        (define (sum-lengths i)
          (if (= i 20)
              0
              (+ (length (vector-ref v i)) (sum-lengths (+ i 1)))))
        (sum-lengths 0)
    "#).unwrap();
    // sum of (i+1) for i in 0..20 = 1+2+...+20 = 210
    assert_eq!(expect_number(v), 210);
}

// =========================================================================
// Allocation churn — short-lived data
// =========================================================================

#[test]
fn many_short_lived_allocations_with_gcs() {
    let mut i = small_interp();
    let v = i.run_source(r#"
        ; Compute sum 1..N by repeatedly allocating and discarding a
        ; list. The list is rebuilt every iteration; previous lists
        ; are garbage. Many minor GCs should fire.
        (define (build n)
          (if (= n 0) nil (cons n (build (- n 1)))))
        (define (sum-list xs)
          (if (null? xs) 0 (+ (car xs) (sum-list (cdr xs)))))
        (define (loop k acc)
          (if (= k 0)
              acc
              (loop (- k 1)
                    (+ acc (sum-list (build 20))))))
        (loop 50 0)
    "#).unwrap();
    // Per iter: sum(1..20) = 210. 50 iterations = 10500.
    assert_eq!(expect_number(v), 10500);
    assert!(i.stats.minor_gcs > 1, "expected several minor GCs");
}

// =========================================================================
// Fibonacci as a real algorithm
// =========================================================================

#[test]
fn fibonacci_tree_recursion_stress() {
    let mut i = medium_interp();
    let v = i.run_source(r#"
        (define (fib n)
          (if (< n 2)
              n
              (+ (fib (- n 1)) (fib (- n 2)))))
        (fib 15)
    "#).unwrap();
    assert_eq!(expect_number(v), 610);
    // fib(15) does ~2000 function calls — exercises frame stack.
    assert!(i.stats.function_calls > 1000);
}

#[test]
fn fibonacci_with_intermediate_list_per_call() {
    let mut i = medium_interp();
    let v = i.run_source(r#"
        ; Allocates a small list per call, then discards it. Stresses
        ; GC by guaranteeing per-call allocation.
        (define (fib n)
          (if (< n 2)
              n
              (let ((trace (cons n nil)))
                (+ (fib (- n 1)) (fib (- n 2))))))
        (fib 12)
    "#).unwrap();
    assert_eq!(expect_number(v), 144);
}

// =========================================================================
// Large vector
// =========================================================================

#[test]
fn allocate_large_vector_walk_after_gc() {
    let mut i = medium_interp();
    // Vector size 100 — the walk uses recursive sum-from which would
    // stack-overflow at larger sizes. 100 is plenty for "large
    // vector through major GC".
    let v = i.run_source(r#"
        (define v (make-vector 100 42))
        (gc-major)
        (gc-major)
        (define (sum-from i)
          (if (= i 100)
              0
              (+ (vector-ref v i) (sum-from (+ i 1)))))
        (sum-from 0)
    "#).unwrap();
    assert_eq!(expect_number(v), 100 * 42);
}

// =========================================================================
// Mixed-shape workload — exercising many object types in one program
// =========================================================================

#[test]
fn mixed_shapes_one_program() {
    let mut i = medium_interp();
    let v = i.run_source(r#"
        ; Build a list of vectors of strings.
        (define (make-row k)
          (let ((v (make-vector 5 "x")))
            (vector-set! v 0 "alpha")
            (vector-set! v 1 "beta")
            (vector-set! v 2 "gamma")
            (vector-set! v 3 "delta")
            (vector-set! v 4 "epsilon")
            v))
        (define (build n)
          (if (= n 0)
              nil
              (cons (make-row n) (build (- n 1)))))
        (define table (build 25))
        (gc-major)
        ; Sum total string lengths across all rows.
        (define (sum-row v i acc)
          (if (= i 5)
              acc
              (sum-row v (+ i 1) (+ acc (string-length (vector-ref v i))))))
        (define (sum-table xs)
          (if (null? xs)
              0
              (+ (sum-row (car xs) 0 0)
                 (sum-table (cdr xs)))))
        (sum-table table)
    "#).unwrap();
    // Each row: 5+4+5+5+7 = 26 chars. 25 rows = 650.
    assert_eq!(expect_number(v), 650);
}

// =========================================================================
// Stats sanity
// =========================================================================

#[test]
fn workload_triggers_meaningful_gc_activity() {
    let mut i = small_interp();
    i.set_minor_threshold(100);
    i.run_source(r#"
        (define (build n)
          (if (= n 0) nil (cons n (build (- n 1)))))
        ; Force many short-lived allocations.
        (define (loop k)
          (if (= k 0)
              nil
              (begin
                (length (build 30))
                (loop (- k 1)))))
        (loop 50)
    "#).unwrap();
    let total_gcs = i.stats.minor_gcs + i.stats.major_gcs;
    assert!(total_gcs >= 3,
        "expected ≥3 GCs, got {} minor + {} major = {}",
        i.stats.minor_gcs, i.stats.major_gcs, total_gcs);
    assert!(i.stats.allocations > 500,
        "expected >500 allocations, got {}", i.stats.allocations);
}

#[test]
fn explicit_gc_calls_are_counted() {
    let mut i = small_interp();
    i.run_source("(gc-now)").unwrap();
    i.run_source("(gc-major)").unwrap();
    i.run_source("(gc-now)").unwrap();
    i.run_source("(gc-major)").unwrap();
    assert!(i.stats.minor_gcs + i.stats.major_gcs >= 2);
}
