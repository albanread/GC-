//! `newgc-test-lisp` — minimal Lisp evaluator driving `newgc-core`.
//!
//! Built as a GC stress driver, not a usable Lisp implementation.
//! Functions are top-level only (no closures); the evaluator is a
//! tree-walking interpreter over parsed s-expressions; the GC sees
//! every live `Word` through the env walk at each safepoint.
//!
//! The point: write workloads in Lisp instead of Rust. A
//! `(define (fact n) ...)` recursion creates allocation patterns —
//! parameter frames, intermediate cons cells, list-tail recursion —
//! that approximate what real programs do, without coding each
//! pattern by hand.

pub mod eval;
pub mod reader;
pub mod value;

pub use eval::{Interpreter, RunError};
pub use reader::{read_all, Sexp};
pub use value::{Heap, Value};
