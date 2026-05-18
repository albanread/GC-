//! Stochastic Lisp workloads — programs that use `(random)` to
//! generate non-deterministic-feeling allocation patterns over many
//! iterations. The seed is fixed (via `random-seed!`) so failures
//! are reproducible.
//!
//! These complement the Rust-driven `stochastic_workload.rs` in
//! `newgc-core/tests/` by exercising the GC through actual Lisp
//! programs — which means the allocation pattern emerges from the
//! evaluator's frame/stack/cons-cell discipline, not from
//! hand-coded Rust calls.

use newgc_test_lisp::{Interpreter, Value};

fn expect_number(v: Value) -> i64 {
    match v {
        Value::Number(n) => n,
        other => panic!("expected number, got {other:?}"),
    }
}

fn interp_for_stochastic() -> Interpreter {
    let mut i = Interpreter::new(32 * 64 * 1024);
    i.set_minor_threshold(300);
    i.set_majors_every(7);
    i
}

#[test]
fn random_allocation_of_lists_or_vectors() {
    let mut i = interp_for_stochastic();
    let v = i.run_source(r#"
        (random-seed! 12345)
        ; 500 iterations: each randomly allocates either a 1-cons
        ; or a 5-slot vector and counts it. The accumulator is the
        ; only retained Word per iteration; everything else dies.
        (define (build n)
          (if (= n 0)
              nil
              (cons (random 100) (build (- n 1)))))
        (define (loop k n-lists n-vecs)
          (if (= k 0)
              (+ n-lists n-vecs)
              (if (= (random 2) 0)
                  (begin
                    (length (build 10))
                    (loop (- k 1) (+ n-lists 1) n-vecs))
                  (begin
                    (vector-length (make-vector 5 (random 100)))
                    (loop (- k 1) n-lists (+ n-vecs 1))))))
        (loop 200 0 0)
    "#).unwrap();
    // The total number of operations is 200; the answer = 200.
    assert_eq!(expect_number(v), 200);
    let gcs = i.stats.minor_gcs + i.stats.major_gcs;
    assert!(gcs >= 3, "expected several GC cycles, got {gcs}");
}

#[test]
fn random_tracked_list_with_random_modifications() {
    let mut i = interp_for_stochastic();
    let v = i.run_source(r#"
        (random-seed! 7777)
        ; Build a tracked vector of 20 lists. On each iteration:
        ;   - pick a random slot
        ;   - replace its list with a fresh one of random length
        ;   - sum all list lengths (forces full walk)
        (define table (make-vector 20 nil))
        (define (fill-init i)
          (if (= i 20)
              nil
              (begin
                (vector-set! table i (list 1 2 3))
                (fill-init (+ i 1)))))
        (fill-init 0)
        (define (build-list n)
          (if (= n 0) nil (cons n (build-list (- n 1)))))
        (define (sum-row i acc)
          (if (= i 20)
              acc
              (sum-row (+ i 1) (+ acc (length (vector-ref table i))))))
        (define (loop k)
          (if (= k 0)
              (sum-row 0 0)
              (begin
                (vector-set! table (random 20) (build-list (random 15)))
                (loop (- k 1)))))
        (loop 100)
    "#).unwrap();
    // The result is the sum of all 20 lists' lengths after the 100
    // random mutations. Exact value depends on the RNG; just verify
    // it's a sane number.
    let n = expect_number(v);
    assert!(n >= 0 && n < 400, "implausible sum-row total: {n}");
}

#[test]
fn random_recursion_depth_and_allocation() {
    let mut i = interp_for_stochastic();
    let v = i.run_source(r#"
        (random-seed! 42)
        ; A recursive function whose depth and per-frame allocation
        ; vary by `random`. Stress on the frame stack + value stack.
        (define (recurse d)
          (if (= d 0)
              0
              (let ((side (build-list (random 8))))
                (+ (length side) (recurse (- d 1))))))
        (define (build-list n)
          (if (= n 0) nil (cons n (build-list (- n 1)))))
        ; Loop 50 random-depth recursions.
        (define (top k acc)
          (if (= k 0)
              acc
              (top (- k 1) (+ acc (recurse (random 30))))))
        (top 50 0)
    "#).unwrap();
    // Sanity: result should be the sum of side-list lengths across
    // 50 outer iterations. Random but bounded.
    let n = expect_number(v);
    assert!(n >= 0 && n < 50 * 30 * 8, "implausible total: {n}");
    assert!(i.stats.minor_gcs >= 2);
}

#[test]
fn random_string_mixed_with_lists() {
    let mut i = interp_for_stochastic();
    let v = i.run_source(r#"
        (random-seed! 999)
        ; Workload: random mix of strings and lists, each retained in
        ; a 30-slot table at a random index. Total iterations: 150.
        (define table (make-vector 30 nil))
        (define (build-list n)
          (if (= n 0) nil (cons n (build-list (- n 1)))))
        (define (loop k)
          (if (= k 0)
              nil
              (begin
                (vector-set! table (random 30)
                  (if (= (random 3) 0)
                      "marker-string"
                      (build-list (random 10))))
                (loop (- k 1)))))
        (loop 150)
        ; Count non-nil entries.
        (define (count-from i acc)
          (if (= i 30)
              acc
              (count-from
                (+ i 1)
                (if (null? (vector-ref table i))
                    acc
                    (+ acc 1)))))
        (count-from 0 0)
    "#).unwrap();
    // After 150 random mutations into 30 slots, most slots should
    // be filled.
    let n = expect_number(v);
    assert!(n > 0 && n <= 30);
    assert!(i.stats.minor_gcs + i.stats.major_gcs >= 1,
        "expected at least one GC, got {} minor + {} major",
        i.stats.minor_gcs, i.stats.major_gcs);
}

#[test]
fn stochastic_long_run_300_iterations() {
    let mut i = interp_for_stochastic();
    i.set_minor_threshold(150);
    // Split into smaller defines for paren sanity.
    let v = i.run_source(r#"
        (random-seed! 0)

        (define (build-list n)
          (if (= n 0) nil (cons (random 100) (build-list (- n 1)))))

        (define (sum-list xs)
          (if (null? xs) 0 (+ (car xs) (sum-list (cdr xs)))))

        (define (sum-vector v i acc)
          (if (= i (vector-length v))
              acc
              (sum-vector v (+ i 1) (+ acc (vector-ref v i)))))

        (define junk-vec (make-vector 10 0))

        (define (do-choice c)
          (if (= c 0)
              (sum-list (build-list (random 12)))
              (if (= c 1)
                  (length (build-list (random 8)))
                  (if (= c 2)
                      (begin
                        (vector-set! junk-vec (random 10) (random 100))
                        (sum-vector junk-vec 0 0))
                      (begin (gc-now) 0)))))

        (define (iteration k acc)
          (if (= k 0)
              acc
              (iteration (- k 1) (+ acc (do-choice (random 4))))))

        (iteration 300 0)
    "#).unwrap();
    // Result is some accumulation; just sanity-check.
    let n = expect_number(v);
    assert!(n >= 0);
    // Confirm meaningful GC activity.
    let total = i.stats.minor_gcs + i.stats.major_gcs;
    assert!(total >= 5,
        "expected at least 5 GCs over 300 iter; got {} minor + {} major",
        i.stats.minor_gcs, i.stats.major_gcs);
    assert!(i.stats.allocations > 500,
        "expected >500 allocations, got {}", i.stats.allocations);
    eprintln!("stochastic long run: {} alloc, {} minor, {} major, {} fn-calls",
        i.stats.allocations,
        i.stats.minor_gcs,
        i.stats.major_gcs,
        i.stats.function_calls);
}
