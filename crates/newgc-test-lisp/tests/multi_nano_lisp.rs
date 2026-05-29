//! multi-nano-lisp: N real interpreters evaluating concurrently.
//!
//! Each thread registers its own `Mutator` on a shared `GcCoordinator`
//! heap and runs a full nano-lisp interpreter (`Interpreter::from_mutator`).
//! This is the most realistic multi-mutator test there is: real
//! allocation patterns from real Lisp evaluation, real env roots gathered
//! at each safepoint, real card barriers (`alloc_pair` / `vector-set!`),
//! and concurrent stop-the-world collections driven by whichever
//! interpreter trips its drive cadence — exercising the same
//! concurrent-driver path the synthetic torture covers, but through an
//! actual language runtime.
//!
//! Integrity: each interpreter keeps a rooted "golden" 50-element list
//! whose sum is fixed at 1225 regardless of the random churn around it.
//! Every collection (driven by *any* interpreter) must forward every
//! interpreter's live golden list; if a peer's list is lost, mis-
//! forwarded, or torn, its sum != 1225 and the assert fires.
//! `newgc_core::crash::install()` turns a segfault into a localized report.
//!
//! Tunable: `NEWGC_MULTILISP_ROUNDS` (default 3), `NEWGC_MULTILISP_CHURN`
//! (default 120). Each round spins up a fresh coordinator + N interpreters.

use std::sync::{Arc, Barrier};
use std::thread;

use newgc_core::{GcCoordinator, LispLayout};
use newgc_test_lisp::{Interpreter, Value};

type Coord = GcCoordinator<LispLayout>;
const N_WORKERS: usize = 4;

fn expect_number(v: Value) -> i64 {
    match v {
        Value::Number(n) => n,
        other => panic!("expected number, got {other:?}"),
    }
}

fn run_round(round: u64, churn: usize) {
    // Generous shared reservation: N interpreters churn garbage into one
    // heap, and value.rs's allocators `.expect()` (a true OOM would panic),
    // so give plenty of headroom; frequent collections keep it mostly free.
    let coord = Coord::with_reservation(1024 * 64 * 1024);
    let ready = Arc::new(Barrier::new(N_WORKERS));

    let workers: Vec<_> = (0..N_WORKERS)
        .map(|t| {
            let c = coord.clone();
            let ready = Arc::clone(&ready);
            thread::spawn(move || {
                let m = c.register_mutator();
                let mut interp = Interpreter::from_mutator(m);
                let seed = (round.wrapping_mul(131).wrapping_add(t as u64 + 1)) as i64;
                let src = format!(
                    r#"
                    (random-seed! {seed})
                    ; golden: a rooted 50-element list 0..49, sum = 1225.
                    (define (mk-golden i) (if (= i 50) nil (cons i (mk-golden (+ i 1)))))
                    (define golden (mk-golden 0))
                    ; churn: random garbage lists + vectors, forcing frequent
                    ; safepoints (each alloc polls; peers drive collections).
                    (define (build-list n)
                      (if (= n 0) nil (cons (random 100) (build-list (- n 1)))))
                    (define (churn k)
                      (if (= k 0)
                          nil
                          (begin
                            (length (build-list (random 20)))
                            (vector-length (make-vector (random 8) (random 100)))
                            (churn (- k 1)))))
                    (define (sum xs) (if (null? xs) 0 (+ (car xs) (sum (cdr xs)))))
                    (churn {churn})
                    (sum golden)
                "#
                );
                // All interpreters begin evaluating together, so collections
                // fire while several are mid-allocation.
                ready.wait();
                let v = interp.run_source(&src).expect("lisp run errored");
                assert_eq!(
                    expect_number(v),
                    1225,
                    "worker {t} (round {round}): golden list corrupted under concurrent GC"
                );
            })
        })
        .collect();

    for h in workers {
        h.join().expect("interpreter thread panicked");
    }
}

#[test]
fn multi_nano_lisp_golden_survival() {
    newgc_core::crash::install();
    let rounds: u64 = std::env::var("NEWGC_MULTILISP_ROUNDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3);
    let churn: usize = std::env::var("NEWGC_MULTILISP_CHURN")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(120);
    for r in 0..rounds {
        run_round(r, churn);
    }
    eprintln!("multi_nano_lisp_golden_survival: {rounds} rounds x {N_WORKERS} interpreters OK");
}
