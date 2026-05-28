# Test Driver: Mini-Lisp

## Purpose

`newgc-test-lisp` is a workload generator for the GC, not a usable Lisp. Its sole purpose is to create realistic allocation patterns — building lists and trees, mutating vectors, building and dropping data structures of various sizes — so that the GC can be exercised against something closer to real usage than synthetic `alloc_cons / collect_minor` sequences.

## Architecture

```
reader.rs   S-expression parser → Expr AST
eval.rs     Tree-walking interpreter — evaluates Expr against a heap
value.rs    Value enum — heap-resident shapes, allocation helpers
lib.rs      Public API: run_script(src, heap) -> Result<Value>
```

The interpreter is intentionally simple. There are no closures, no tail-call optimisation, no macros. The goal is allocation patterns, not language completeness.

## Syntax reference

### Literals

```lisp
42          ; fixnum
-7          ; negative fixnum
"hello"     ; string (allocates a boxed String object)
nil         ; empty list / false
t           ; true
```

### Definitions and bindings

```lisp
(define (name arg1 arg2) body)   ; top-level function (no closures)
(let ((x 1) (y 2)) body)        ; local bindings
```

Functions are not first-class — only top-level defines are supported. Recursive functions are allowed.

### Control flow

```lisp
(if condition then-expr else-expr)
```

### Arithmetic

```lisp
(+ a b)  (- a b)  (* a b)  (/ a b)
(= a b)  (< a b)  (> a b)
```

All arithmetic operates on fixnums. Division truncates toward zero.

### Pairs and lists

```lisp
(cons a b)          ; allocate a 2-cell cons pair
(car pair)          ; first element
(cdr pair)          ; second element
(null? x)           ; true if x is nil
```

Build a list: `(cons 1 (cons 2 (cons 3 nil)))` → `(1 2 3)`.

### Vectors

```lisp
(vector n init)         ; allocate a vector of n elements, each initialised to init
(vector-ref v i)        ; read element i (0-based)
(vector-set! v i val)   ; write val to element i — marks the write-barrier card
(vector-length v)       ; number of elements
```

`vector-set!` marks the write-barrier card after every store, which is required for correct minor-cycle collection of vectors that may be promoted to old generations while still receiving young-pointer writes.

### Strings

```lisp
(string "literal")     ; allocate a boxed String object
(string-length s)      ; byte length
```

### GC control

```lisp
(gc-major)    ; trigger a major GC cycle explicitly
(gc-now)      ; same as gc-major; alias for readability in scripts
```

These are primarily used in test scripts to force a collection at a known point and then verify that still-reachable objects have not been corrupted.

### Assertions

```lisp
(assert (eq a b))          ; panics if not equal
(check-equal a b)          ; same, with a better error message
```

### Random numbers

```lisp
(random n)    ; pseudo-random integer in [0, n); seeded deterministically
```

Used in stochastic workload scripts to vary allocation sizes and access patterns.

### Introspection

```lisp
(gc-stats)    ; returns a string summary of heap stats
```

## GC integration

The interpreter integrates with the GC at two points:

**Allocation safepoint.** The evaluator fires a safepoint every ~1,000 allocated cells. At a safepoint, it walks its entire `Value` environment — every live binding in every stack frame — and calls `evac.visit` on each `Word` held by a `Value`. After the safepoint, every Word in the environment points to the correct post-collection address.

**Explicit GC calls.** `(gc-major)` and `(gc-now)` trigger a collection immediately, using the same root-walking closure as the safepoint.

The root walk does not require separate bookkeeping — the interpreter environment is the root set. This is intentional: the mini-Lisp is designed to make root enumeration trivially correct.

## Workload scripts

Scripts live in `crates/newgc-test-lisp/scripts/`:

| Script | What it tests |
|--------|--------------|
| `01-arithmetic.lisp` | Fixnum arithmetic, basic control flow |
| `02-functions.lisp` | Recursion — factorial, Fibonacci |
| `03-pairs.lisp` | Cons-cell list construction, `car`/`cdr`, `null?` |
| `04-mutation.lisp` | Mixed minor/major patterns via `vector-set!` and explicit `(gc-major)` |
| `05-vectors.lisp` | Vector allocation and element mutation |
| `06-strings.lisp` | String construction |
| `07-gc-stats.lisp` | Heap introspection via `(gc-stats)` |

## Test structure

### Smoke tests (`tests/smoke.rs`)

Fourteen tests covering basic evaluation: arithmetic, definitions, recursion, pairs. These verify the interpreter is correct before testing GC behaviour.

### Lisp workload tests (`tests/lisp_workloads.rs`)

Seventeen tests that build data structures of specific shapes to exercise distinct GC patterns:

- Deep cons-cell lists that survive many minor cycles
- Binary trees of various depths
- Vectors with mutation across GC boundaries
- Mixed live/dead allocation (the classic "weak-generation hypothesis" workload)

Each test ends with `(assert ...)` or `(check-equal ...)` to verify object values are intact after collection.

### Stochastic Lisp tests (`tests/lisp_stochastic.rs`)

Five tests using `(random n)` to vary allocation sizes, recursion depths, and mutation targets. Seed is fixed for reproducibility. The goal is to hit allocation patterns that deterministic tests might miss.

### Script runner tests (`tests/run_scripts.rs`)

One test per `.lisp` file in `scripts/`. Each test loads and runs the script; any `(assert ...)` failure propagates as a test failure.

## Writing a new workload

A workload script should:

1. Define helper functions at the top
2. Build data structures representing your target allocation pattern
3. Optionally call `(gc-major)` at points where you want to force a collection
4. Call `(check-equal ...)` to verify values are intact after collection

```lisp
; Example: build a list of 100 elements, survive a GC cycle, verify
(define (build-list n)
  (if (= n 0)
      nil
      (cons n (build-list (- n 1)))))

(define lst (build-list 100))
(gc-major)
(check-equal (car lst) 100)
(check-equal (car (cdr lst)) 99)
```

---

Back to [Home](index.md).
