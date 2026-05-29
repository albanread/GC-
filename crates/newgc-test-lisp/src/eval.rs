//! Tree-walking evaluator.
//!
//! The interpreter's state is:
//!   - `heap`: the page-heap GC.
//!   - `globals`: top-level define-d bindings.
//!   - `functions`: top-level define-d functions (param names + body sexp).
//!   - `frames`: stack of local bindings, one per active function call.
//!   - `value_stack`: anonymous values mid-evaluation. The GC walker
//!     scans this so intermediate Words don't go missing during a
//!     compound expression's evaluation.
//!
//! GC discipline: an allocation may trigger GC. Before any builtin
//! that allocates, every value currently in use must be on
//! `value_stack`, in a `Frame::bindings`, or in `globals` —
//! anywhere the GC walker can find it. Rust-local Values that hold
//! Word slots **must not** straddle a GC safepoint.

use std::collections::HashMap;
use std::rc::Rc;

use newgc_core::page_heap::evac::PageEvacuator;
use newgc_core::page_heap::space::PageHeap;
use newgc_core::{Generation, HeapLayout, LispLayout, Mutator, Word};

use crate::reader::Sexp;
use crate::value::{self, Heap, Value};

#[derive(Debug)]
pub enum RunError {
    Unbound(String),
    BadArity {
        name: String,
        expected: usize,
        got: usize,
    },
    TypeError(String),
    Other(String),
}

impl RunError {
    pub fn msg(&self) -> String {
        match self {
            RunError::Unbound(s) => format!("unbound symbol: {s}"),
            RunError::BadArity { name, expected, got } => {
                format!("bad arity for {name}: expected {expected}, got {got}")
            }
            RunError::TypeError(s) => format!("type error: {s}"),
            RunError::Other(s) => s.clone(),
        }
    }
}

struct FunctionDef {
    params: Vec<Rc<String>>,
    body: Sexp,
}

struct Frame {
    bindings: Vec<(Rc<String>, Value)>,
}

pub struct Interpreter {
    pub heap: Heap,
    globals: HashMap<Rc<String>, Value>,
    functions: HashMap<Rc<String>, FunctionDef>,
    frames: Vec<Frame>,
    /// Intermediate values during compound-expression evaluation.
    /// Push after evaluating each subexpression; pop when applying
    /// the parent operation. Scanned by the GC walker.
    value_stack: Vec<Value>,
    /// Deterministic RNG state for the `(random)` builtin. Seeded
    /// to a non-zero default so runs without explicit
    /// `(random-seed! n)` are still reproducible.
    rng_state: u64,
    /// Shared (multi-mutator) mode only: counts safepoint polls; every
    /// `SHARED_DRIVE_EVERY` the interpreter drives a minor collection
    /// itself, since there is no auto-trigger behind a `Mutator`.
    gc_drive_counter: usize,
    pub stats: Stats,
}

/// In `Heap::Shared` (multi-mutator) mode, drive a minor collection every
/// this-many safepoint polls.
const SHARED_DRIVE_EVERY: usize = 48;

#[derive(Copy, Clone, Debug)]
enum GcKind {
    Minor,
    Major,
    Auto,
}

#[derive(Default, Debug, Clone)]
pub struct Stats {
    pub top_level_forms: usize,
    pub function_calls: usize,
    pub minor_gcs: usize,
    pub major_gcs: usize,
    pub allocations: usize,
    pub max_frame_depth: usize,
    pub max_value_stack: usize,
}

impl Interpreter {
    pub fn new(reservation_bytes: usize) -> Self {
        Interpreter {
            heap: Heap::Direct(PageHeap::with_reservation(reservation_bytes)),
            globals: HashMap::new(),
            functions: HashMap::new(),
            frames: Vec::new(),
            value_stack: Vec::new(),
            rng_state: 0xc0ffee_u64,
            gc_drive_counter: 0,
            stats: Stats::default(),
        }
    }

    /// Build an interpreter that allocates from a `Mutator` handle on a
    /// shared `GcCoordinator` heap — the multi-mutator ("multi-nano-lisp")
    /// mode. N of these run on separate threads concurrently; each
    /// cooperates at safepoints and periodically drives a collection that
    /// stops every interpreter and forwards all of their roots.
    pub fn from_mutator(m: Mutator<LispLayout>) -> Self {
        Interpreter {
            heap: Heap::Shared(m),
            globals: HashMap::new(),
            functions: HashMap::new(),
            frames: Vec::new(),
            value_stack: Vec::new(),
            rng_state: 0xc0ffee_u64,
            gc_drive_counter: 0,
            stats: Stats::default(),
        }
    }

    /// Override the auto-GC threshold. Forwards to the heap's
    /// `set_gc_budget_min_bytes`. Higher = less frequent GC.
    /// Default is 8 MB (set by the heap).
    pub fn set_minor_threshold(&mut self, n: usize) {
        // `n` is in bytes, matching the heap-side semantics.
        self.heap.set_gc_budget_min_bytes(n);
    }

    /// Adjust how often major cycles fire relative to minor. The
    /// auto-trigger uses Tenured fill (default 75%) rather than a
    /// minor-cycle count, so this is now translated into a Tenured
    /// threshold (basis points). Passing `n=10` ⇒ trigger major
    /// when Tenured > 10% of the reservation; `n=1` ⇒ very
    /// aggressive (1% threshold).
    pub fn set_majors_every(&mut self, n: usize) {
        let bps = ((100 / n.max(1)) * 100).min(10000) as u32;
        self.heap.set_tenured_full_threshold_bps(bps);
    }

    // -- Top-level entry points ------------------------------------------

    /// Run all top-level forms in the source; returns the value of
    /// the last form.
    pub fn run_source(&mut self, src: &str) -> Result<Value, RunError> {
        let forms = crate::reader::read_all(src)
            .map_err(|e| RunError::Other(format!("read: {e}")))?;
        let mut last = Value::Nil;
        for form in &forms {
            last = self.eval_top_level(form)?;
        }
        Ok(last)
    }

    /// Evaluate one top-level form. Triggers an auto-safepoint after.
    pub fn eval_top_level(&mut self, sexp: &Sexp) -> Result<Value, RunError> {
        self.stats.top_level_forms += 1;
        self.eval(sexp)?;
        let v = self
            .value_stack
            .pop()
            .ok_or_else(|| RunError::Other("eval produced no value".into()))?;
        self.maybe_gc();
        Ok(v)
    }

    // -- Core eval --------------------------------------------------------

    /// Evaluate `sexp`, pushing its value onto `value_stack`.
    fn eval(&mut self, sexp: &Sexp) -> Result<(), RunError> {
        self.stats.max_value_stack =
            self.stats.max_value_stack.max(self.value_stack.len());
        match sexp {
            Sexp::Number(n) => {
                self.value_stack.push(Value::Number(*n));
                Ok(())
            }
            Sexp::Bool(b) => {
                self.value_stack.push(Value::Bool(*b));
                Ok(())
            }
            Sexp::Nil => {
                self.value_stack.push(Value::Nil);
                Ok(())
            }
            Sexp::String(s) => {
                self.maybe_gc();
                let w = value::alloc_string(&mut self.heap, s.as_bytes());
                self.stats.allocations += 1;
                self.value_stack.push(Value::Str(w));
                Ok(())
            }
            Sexp::Symbol(name) => {
                let v = self.lookup(name)?;
                self.value_stack.push(v);
                Ok(())
            }
            Sexp::List(items) => self.eval_list(items),
        }
    }

    fn eval_list(&mut self, items: &[Sexp]) -> Result<(), RunError> {
        // Empty list ≡ nil — but the reader returns Sexp::Nil for `()`,
        // so this is unreachable in practice. Defensive:
        if items.is_empty() {
            self.value_stack.push(Value::Nil);
            return Ok(());
        }
        // Dispatch on the head symbol.
        if let Some(sym) = items[0].as_symbol() {
            match sym {
                "quote" => return self.eval_quote(&items[1..]),
                "if" => return self.eval_if(&items[1..]),
                "let" => return self.eval_let(&items[1..]),
                "begin" => return self.eval_begin(&items[1..]),
                "define" => return self.eval_define(&items[1..]),
                "set!" => return self.eval_set(&items[1..]),
                "when" => return self.eval_when(&items[1..]),
                "unless" => return self.eval_unless(&items[1..]),
                "and" => return self.eval_and(&items[1..]),
                "or" => return self.eval_or(&items[1..]),
                _ => return self.eval_call(sym, &items[1..]),
            }
        }
        Err(RunError::Other(format!(
            "head of list isn't a symbol: {:?}",
            items[0]
        )))
    }

    // -- Special forms ----------------------------------------------------

    fn eval_quote(&mut self, args: &[Sexp]) -> Result<(), RunError> {
        if args.len() != 1 {
            return Err(RunError::BadArity {
                name: "quote".into(),
                expected: 1,
                got: args.len(),
            });
        }
        // `quote` returns its argument as a literal value. We support
        // numbers, bools, nil, and symbol-as-string. Lists in quoted
        // position would need to be materialised as Pair chains —
        // this driver only uses quote on atoms today.
        let v = match &args[0] {
            Sexp::Number(n) => Value::Number(*n),
            Sexp::Bool(b) => Value::Bool(*b),
            Sexp::Nil => Value::Nil,
            Sexp::Symbol(s) => Value::Symbol(s.clone()),
            Sexp::String(s) => {
                self.maybe_gc();
                let w = value::alloc_string(&mut self.heap, s.as_bytes());
                self.stats.allocations += 1;
                Value::Str(w)
            }
            Sexp::List(_) => {
                return Err(RunError::Other(
                    "(quote (...)) on a list is not supported in this mini-Lisp".into(),
                ));
            }
        };
        self.value_stack.push(v);
        Ok(())
    }

    fn eval_if(&mut self, args: &[Sexp]) -> Result<(), RunError> {
        if args.len() != 2 && args.len() != 3 {
            return Err(RunError::BadArity {
                name: "if".into(),
                expected: 3,
                got: args.len(),
            });
        }
        self.eval(&args[0])?;
        let cond = self.value_stack.pop().unwrap();
        if cond.is_truthy() {
            self.eval(&args[1])
        } else if args.len() == 3 {
            self.eval(&args[2])
        } else {
            self.value_stack.push(Value::Nil);
            Ok(())
        }
    }

    fn eval_let(&mut self, args: &[Sexp]) -> Result<(), RunError> {
        if args.len() < 2 {
            return Err(RunError::Other("let: malformed".into()));
        }
        let bindings_sexp = &args[0];
        let bindings = match bindings_sexp {
            Sexp::List(b) => b,
            Sexp::Nil => &Vec::new(),
            _ => return Err(RunError::Other("let: bindings must be a list".into())),
        };
        let mut frame = Frame { bindings: Vec::new() };
        // Evaluate each binding's value in the *outer* env (parallel
        // let semantics), pushing onto value_stack and gathering at
        // the end.
        let high_water = self.value_stack.len();
        let mut names: Vec<Rc<String>> = Vec::new();
        for b in bindings {
            match b {
                Sexp::List(pair) if pair.len() == 2 => {
                    let name = match &pair[0] {
                        Sexp::Symbol(s) => s.clone(),
                        _ => {
                            return Err(RunError::Other(
                                "let binding's lhs must be a symbol".into(),
                            ))
                        }
                    };
                    self.eval(&pair[1])?;
                    names.push(name);
                }
                _ => {
                    return Err(RunError::Other(
                        "let binding must be a (name expr) pair".into(),
                    ))
                }
            }
        }
        // Now pop values in reverse order and pair with names.
        let mut values: Vec<Value> = self.value_stack.split_off(high_water);
        debug_assert_eq!(values.len(), names.len());
        for (name, value) in names.into_iter().zip(values.drain(..)) {
            frame.bindings.push((name, value));
        }
        self.frames.push(frame);
        self.stats.max_frame_depth =
            self.stats.max_frame_depth.max(self.frames.len());
        // Evaluate body forms; last one's value is the let's result.
        let body = &args[1..];
        for (i, form) in body.iter().enumerate() {
            self.eval(form)?;
            if i + 1 < body.len() {
                // Discard intermediate values.
                self.value_stack.pop();
            }
        }
        self.frames.pop();
        Ok(())
    }

    fn eval_begin(&mut self, args: &[Sexp]) -> Result<(), RunError> {
        if args.is_empty() {
            self.value_stack.push(Value::Nil);
            return Ok(());
        }
        for (i, form) in args.iter().enumerate() {
            self.eval(form)?;
            if i + 1 < args.len() {
                self.value_stack.pop();
            }
        }
        Ok(())
    }

    fn eval_define(&mut self, args: &[Sexp]) -> Result<(), RunError> {
        if args.is_empty() {
            return Err(RunError::Other("define: no args".into()));
        }
        match &args[0] {
            // (define x value)
            Sexp::Symbol(name) => {
                if args.len() != 2 {
                    return Err(RunError::Other(
                        "define: expected (define name value)".into(),
                    ));
                }
                self.eval(&args[1])?;
                let v = self.value_stack.pop().unwrap();
                self.globals.insert(name.clone(), v);
                self.value_stack.push(Value::Nil);
                Ok(())
            }
            // (define (name params...) body...)
            Sexp::List(header) => {
                if header.is_empty() {
                    return Err(RunError::Other(
                        "define: empty function header".into(),
                    ));
                }
                let fname = match &header[0] {
                    Sexp::Symbol(s) => s.clone(),
                    _ => {
                        return Err(RunError::Other(
                            "define: function name must be a symbol".into(),
                        ))
                    }
                };
                let params: Result<Vec<Rc<String>>, RunError> = header[1..]
                    .iter()
                    .map(|p| match p {
                        Sexp::Symbol(s) => Ok(s.clone()),
                        _ => Err(RunError::Other(
                            "define: param must be a symbol".into(),
                        )),
                    })
                    .collect();
                let params = params?;
                // Body: (begin body...)
                let body = if args.len() == 2 {
                    args[1].clone()
                } else {
                    let mut body_items = vec![Sexp::Symbol(Rc::new("begin".into()))];
                    body_items.extend(args[1..].iter().cloned());
                    Sexp::List(body_items)
                };
                self.functions.insert(
                    fname,
                    FunctionDef { params, body },
                );
                self.value_stack.push(Value::Nil);
                Ok(())
            }
            _ => Err(RunError::Other("define: malformed".into())),
        }
    }

    fn eval_set(&mut self, args: &[Sexp]) -> Result<(), RunError> {
        if args.len() != 2 {
            return Err(RunError::BadArity {
                name: "set!".into(),
                expected: 2,
                got: args.len(),
            });
        }
        let name = match &args[0] {
            Sexp::Symbol(s) => s.clone(),
            _ => return Err(RunError::Other("set!: lhs must be a symbol".into())),
        };
        self.eval(&args[1])?;
        let v = self.value_stack.pop().unwrap();
        // Search locals first, then globals.
        for frame in self.frames.iter_mut().rev() {
            for (n, slot) in frame.bindings.iter_mut() {
                if Rc::ptr_eq(n, &name) || **n == *name {
                    *slot = v;
                    self.value_stack.push(Value::Nil);
                    return Ok(());
                }
            }
        }
        if self.globals.contains_key(&name) {
            self.globals.insert(name, v);
            self.value_stack.push(Value::Nil);
            Ok(())
        } else {
            Err(RunError::Unbound(name.to_string()))
        }
    }

    fn eval_when(&mut self, args: &[Sexp]) -> Result<(), RunError> {
        if args.is_empty() {
            return Err(RunError::Other("when: no args".into()));
        }
        self.eval(&args[0])?;
        let c = self.value_stack.pop().unwrap();
        if c.is_truthy() {
            self.eval_begin(&args[1..])
        } else {
            self.value_stack.push(Value::Nil);
            Ok(())
        }
    }

    fn eval_unless(&mut self, args: &[Sexp]) -> Result<(), RunError> {
        if args.is_empty() {
            return Err(RunError::Other("unless: no args".into()));
        }
        self.eval(&args[0])?;
        let c = self.value_stack.pop().unwrap();
        if !c.is_truthy() {
            self.eval_begin(&args[1..])
        } else {
            self.value_stack.push(Value::Nil);
            Ok(())
        }
    }

    fn eval_and(&mut self, args: &[Sexp]) -> Result<(), RunError> {
        if args.is_empty() {
            self.value_stack.push(Value::Bool(true));
            return Ok(());
        }
        for (i, a) in args.iter().enumerate() {
            self.eval(a)?;
            if i + 1 == args.len() {
                return Ok(()); // last value is the result
            }
            let v = self.value_stack.pop().unwrap();
            if !v.is_truthy() {
                self.value_stack.push(v);
                return Ok(());
            }
        }
        Ok(())
    }

    fn eval_or(&mut self, args: &[Sexp]) -> Result<(), RunError> {
        if args.is_empty() {
            self.value_stack.push(Value::Bool(false));
            return Ok(());
        }
        for (i, a) in args.iter().enumerate() {
            self.eval(a)?;
            if i + 1 == args.len() {
                return Ok(());
            }
            let v = self.value_stack.pop().unwrap();
            if v.is_truthy() {
                self.value_stack.push(v);
                return Ok(());
            }
        }
        Ok(())
    }

    // -- Function calls ---------------------------------------------------

    fn eval_call(&mut self, name: &str, args: &[Sexp]) -> Result<(), RunError> {
        // Evaluate each arg, leaving values on value_stack.
        let high_water = self.value_stack.len();
        for a in args {
            self.eval(a)?;
        }
        let arg_values: Vec<Value> = self.value_stack.split_off(high_water);
        // Try built-in first, then user-defined function.
        if self.try_builtin(name, arg_values.clone())? {
            return Ok(());
        }
        let key = Rc::new(name.to_string());
        let fdef = self.functions.get(&key).ok_or_else(|| {
            RunError::Unbound(name.to_string())
        })?;
        if fdef.params.len() != arg_values.len() {
            return Err(RunError::BadArity {
                name: name.to_string(),
                expected: fdef.params.len(),
                got: arg_values.len(),
            });
        }
        // Push a frame binding each param to its arg.
        let mut frame = Frame { bindings: Vec::new() };
        for (p, a) in fdef.params.iter().zip(arg_values.into_iter()) {
            frame.bindings.push((p.clone(), a));
        }
        self.frames.push(frame);
        self.stats.max_frame_depth =
            self.stats.max_frame_depth.max(self.frames.len());
        self.stats.function_calls += 1;
        // Clone the body so we don't hold a borrow on self.functions
        // across the recursive eval call.
        let body = fdef.body.clone();
        let result = self.eval(&body);
        self.frames.pop();
        result
    }

    // -- Built-ins --------------------------------------------------------

    /// Returns `Ok(true)` if `name` matched a built-in (and the
    /// result has been pushed); `Ok(false)` if no built-in matched.
    fn try_builtin(
        &mut self,
        name: &str,
        args: Vec<Value>,
    ) -> Result<bool, RunError> {
        macro_rules! arity {
            ($n:expr) => {
                if args.len() != $n {
                    return Err(RunError::BadArity {
                        name: name.into(),
                        expected: $n,
                        got: args.len(),
                    });
                }
            };
        }
        macro_rules! number {
            ($v:expr, $where:expr) => {
                match $v {
                    Value::Number(n) => n,
                    other => {
                        return Err(RunError::TypeError(format!(
                            "{}: expected number, got {}",
                            $where,
                            other.type_name()
                        )))
                    }
                }
            };
        }
        macro_rules! pair {
            ($v:expr, $where:expr) => {
                match $v {
                    Value::Pair(w) => w,
                    other => {
                        return Err(RunError::TypeError(format!(
                            "{}: expected pair, got {}",
                            $where,
                            other.type_name()
                        )))
                    }
                }
            };
        }
        match name {
            "+" => {
                let mut acc: i64 = 0;
                for a in args {
                    acc = acc.wrapping_add(number!(a, "+"));
                }
                self.value_stack.push(Value::Number(acc));
            }
            "-" => {
                if args.is_empty() {
                    return Err(RunError::BadArity {
                        name: "-".into(),
                        expected: 1,
                        got: 0,
                    });
                }
                let mut it = args.into_iter();
                let first = number!(it.next().unwrap(), "-");
                if let Some(next) = it.next() {
                    let mut acc = first.wrapping_sub(number!(next, "-"));
                    for a in it {
                        acc = acc.wrapping_sub(number!(a, "-"));
                    }
                    self.value_stack.push(Value::Number(acc));
                } else {
                    self.value_stack.push(Value::Number(first.wrapping_neg()));
                }
            }
            "*" => {
                let mut acc: i64 = 1;
                for a in args {
                    acc = acc.wrapping_mul(number!(a, "*"));
                }
                self.value_stack.push(Value::Number(acc));
            }
            "=" => {
                arity!(2);
                let a = number!(args[0].clone(), "=");
                let b = number!(args[1].clone(), "=");
                self.value_stack.push(Value::Bool(a == b));
            }
            "<" => {
                arity!(2);
                let a = number!(args[0].clone(), "<");
                let b = number!(args[1].clone(), "<");
                self.value_stack.push(Value::Bool(a < b));
            }
            ">" => {
                arity!(2);
                let a = number!(args[0].clone(), ">");
                let b = number!(args[1].clone(), ">");
                self.value_stack.push(Value::Bool(a > b));
            }
            "cons" => {
                arity!(2);
                // Push args onto value_stack BEFORE any safepoint —
                // otherwise the Rust-local `args` Vec is the only
                // holder of these Words, and a GC mid-builtin would
                // reclaim/move targets we still need.
                self.value_stack.push(args[0].clone());
                self.value_stack.push(args[1].clone());
                self.maybe_gc();
                let cdr = self.value_stack.pop().unwrap();
                let car = self.value_stack.pop().unwrap();
                let w = value::alloc_pair(&mut self.heap, car, cdr);
                self.stats.allocations += 1;
                self.value_stack.push(Value::Pair(w));
            }
            "car" => {
                arity!(1);
                let p = pair!(args[0].clone(), "car");
                self.value_stack.push(value::pair_car(p));
            }
            "cdr" => {
                arity!(1);
                let p = pair!(args[0].clone(), "cdr");
                self.value_stack.push(value::pair_cdr(p));
            }
            "null?" => {
                arity!(1);
                self.value_stack.push(Value::Bool(matches!(args[0], Value::Nil)));
            }
            "pair?" => {
                arity!(1);
                self.value_stack.push(Value::Bool(matches!(args[0], Value::Pair(_))));
            }
            "number?" => {
                arity!(1);
                self.value_stack.push(Value::Bool(matches!(args[0], Value::Number(_))));
            }
            "vector?" => {
                arity!(1);
                self.value_stack.push(Value::Bool(matches!(args[0], Value::Vector(_))));
            }
            "string?" => {
                arity!(1);
                self.value_stack.push(Value::Bool(matches!(args[0], Value::Str(_))));
            }
            "eq?" => {
                arity!(2);
                let eq = match (&args[0], &args[1]) {
                    (Value::Number(a), Value::Number(b)) => a == b,
                    (Value::Bool(a), Value::Bool(b)) => a == b,
                    (Value::Nil, Value::Nil) => true,
                    (Value::Pair(a), Value::Pair(b)) => a.raw() == b.raw(),
                    (Value::Vector(a), Value::Vector(b)) => a.raw() == b.raw(),
                    (Value::Str(a), Value::Str(b)) => a.raw() == b.raw(),
                    (Value::Symbol(a), Value::Symbol(b)) => Rc::ptr_eq(a, b) || **a == **b,
                    _ => false,
                };
                self.value_stack.push(Value::Bool(eq));
            }
            "list" => {
                // (list a b c) ≡ (cons a (cons b (cons c nil)))
                // Push args, then build the list right-to-left.
                for a in &args {
                    self.value_stack.push(a.clone());
                }
                let n = args.len();
                self.value_stack.push(Value::Nil);
                for _ in 0..n {
                    self.maybe_gc();
                    let cdr = self.value_stack.pop().unwrap();
                    // The next car is now at position (high_water - 1).
                    let car = self.value_stack.pop().unwrap();
                    let w = value::alloc_pair(&mut self.heap, car, cdr);
                    self.stats.allocations += 1;
                    self.value_stack.push(Value::Pair(w));
                }
                // value_stack top now holds the list head.
            }
            "length" => {
                arity!(1);
                let n = value::list_length(&args[0]).ok_or_else(|| {
                    RunError::TypeError(format!(
                        "length: not a list ({})",
                        args[0].type_name()
                    ))
                })?;
                self.value_stack.push(Value::Number(n as i64));
            }
            "make-vector" => {
                arity!(2);
                let n = number!(args[0].clone(), "make-vector");
                if n < 0 || n > 1_000_000 {
                    return Err(RunError::Other(format!(
                        "make-vector: bad size {n}"
                    )));
                }
                // Push init onto value_stack so GC can see it.
                self.value_stack.push(args[1].clone());
                self.maybe_gc();
                let init = self.value_stack.pop().unwrap();
                let w = value::alloc_vector(&mut self.heap, n as u32, init);
                self.stats.allocations += 1;
                // Push the new vector so subsequent operations (none
                // here, but a safepoint inside alloc_vector if added
                // later) can find it.
                self.value_stack.push(Value::Vector(w));
            }
            "vector" => {
                // Push all args onto value_stack before any safepoint.
                for a in &args {
                    self.value_stack.push(a.clone());
                }
                let n = args.len() as u32;
                self.maybe_gc();
                let w = value::alloc_vector(&mut self.heap, n, Value::Nil);
                self.stats.allocations += 1;
                // Pop the args (post-GC) and store into the vector.
                // No safepoint between pop and vector_set: vector_set
                // is a non-allocating write.
                let mut vals: Vec<Value> =
                    Vec::with_capacity(args.len());
                for _ in 0..args.len() {
                    vals.push(self.value_stack.pop().unwrap());
                }
                vals.reverse();
                for (i, a) in vals.into_iter().enumerate() {
                    value::vector_set(&self.heap, w, i, a);
                }
                self.value_stack.push(Value::Vector(w));
            }
            "vector-ref" => {
                arity!(2);
                let v = match &args[0] {
                    Value::Vector(w) => *w,
                    other => {
                        return Err(RunError::TypeError(format!(
                            "vector-ref: not a vector ({})",
                            other.type_name()
                        )))
                    }
                };
                let i = number!(args[1].clone(), "vector-ref");
                self.value_stack.push(value::vector_ref(v, i as usize));
            }
            "vector-set!" => {
                arity!(3);
                let v = match args[0] {
                    Value::Vector(w) => w,
                    _ => {
                        return Err(RunError::TypeError(
                            "vector-set!: not a vector".into(),
                        ))
                    }
                };
                let i = number!(args[1].clone(), "vector-set!");
                value::vector_set(&self.heap, v, i as usize, args[2].clone());
                self.value_stack.push(Value::Nil);
            }
            "vector-length" => {
                arity!(1);
                let v = match args[0] {
                    Value::Vector(w) => w,
                    _ => {
                        return Err(RunError::TypeError(
                            "vector-length: not a vector".into(),
                        ))
                    }
                };
                self.value_stack
                    .push(Value::Number(value::vector_len(v) as i64));
            }
            "string-length" => {
                arity!(1);
                let s = match args[0] {
                    Value::Str(w) => w,
                    _ => {
                        return Err(RunError::TypeError(
                            "string-length: not a string".into(),
                        ))
                    }
                };
                self.value_stack
                    .push(Value::Number(value::string_len(s) as i64));
            }
            // GC controls.
            "gc-now" | "gc-minor" => {
                arity!(0);
                self.collect_minor();
                self.value_stack.push(Value::Nil);
            }
            "gc-major" => {
                arity!(0);
                self.collect_major();
                self.value_stack.push(Value::Nil);
            }
            // Stats / diagnostics.
            "gc-stats-minor-cycles" => {
                arity!(0);
                self.value_stack.push(Value::Number(self.stats.minor_gcs as i64));
            }
            "gc-stats-major-cycles" => {
                arity!(0);
                self.value_stack.push(Value::Number(self.stats.major_gcs as i64));
            }
            "gc-stats-allocations" => {
                arity!(0);
                self.value_stack.push(Value::Number(self.stats.allocations as i64));
            }
            // Heap-side stats accessors. Pull from `heap.stats()`.
            "heap-used-bytes" => {
                arity!(0);
                self.value_stack
                    .push(Value::Number(self.heap.stats().total_used_bytes as i64));
            }
            "heap-g0-used-bytes" => {
                arity!(0);
                self.value_stack
                    .push(Value::Number(self.heap.stats().g0_used_bytes as i64));
            }
            "heap-tenured-used-bytes" => {
                arity!(0);
                self.value_stack
                    .push(Value::Number(self.heap.stats().tenured_used_bytes as i64));
            }
            "heap-free-pages" => {
                arity!(0);
                self.value_stack
                    .push(Value::Number(self.heap.stats().free_pages as i64));
            }
            "heap-bytes-alloc-since-gc" => {
                arity!(0);
                self.value_stack
                    .push(Value::Number(self.heap.stats().bytes_alloc_since_gc as i64));
            }
            "heap-auto-gc-trigger-bytes" => {
                arity!(0);
                self.value_stack
                    .push(Value::Number(self.heap.stats().auto_gc_trigger_bytes as i64));
            }
            // Deterministic RNG. `(random n)` returns a number in
            // `[0, n)`. State persists across calls; `(random-seed!
            // seed)` resets.
            "random" => {
                arity!(1);
                let n = number!(args[0].clone(), "random");
                if n <= 0 {
                    return Err(RunError::Other(format!(
                        "random: bound must be positive, got {n}"
                    )));
                }
                self.rng_state = self.rng_state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let v = ((self.rng_state >> 1) as i64).rem_euclid(n);
                self.value_stack.push(Value::Number(v));
            }
            "random-seed!" => {
                arity!(1);
                let s = number!(args[0].clone(), "random-seed!");
                self.rng_state = (s as u64) | 1;
                self.value_stack.push(Value::Nil);
            }
            // Test-only assertion builtins.
            "assert" => {
                arity!(1);
                if !args[0].is_truthy() {
                    return Err(RunError::Other(format!(
                        "assertion failed: {:?}",
                        args[0]
                    )));
                }
                self.value_stack.push(Value::Nil);
            }
            "check-equal" => {
                arity!(2);
                let eq = match (&args[0], &args[1]) {
                    (Value::Number(a), Value::Number(b)) => a == b,
                    (Value::Bool(a), Value::Bool(b)) => a == b,
                    (Value::Nil, Value::Nil) => true,
                    (Value::Symbol(a), Value::Symbol(b)) => **a == **b,
                    (Value::Pair(a), Value::Pair(b)) => a.raw() == b.raw(),
                    (Value::Vector(a), Value::Vector(b)) => a.raw() == b.raw(),
                    (Value::Str(a), Value::Str(b)) => {
                        value::string_bytes(*a) == value::string_bytes(*b)
                    }
                    _ => false,
                };
                if !eq {
                    return Err(RunError::Other(format!(
                        "check-equal failed: expected {:?}, got {:?}",
                        args[0], args[1]
                    )));
                }
                self.value_stack.push(Value::Nil);
            }
            // GC threshold control. `n` is the byte budget between
            // auto-collections.
            "set-gc-threshold!" => {
                arity!(1);
                let n = number!(args[0].clone(), "set-gc-threshold!");
                if n < 0 {
                    return Err(RunError::Other(
                        "set-gc-threshold!: must be non-negative".into(),
                    ));
                }
                self.heap.set_gc_budget_min_bytes(n.max(1) as usize);
                self.value_stack.push(Value::Nil);
            }
            "error" => {
                let msg = match args.first() {
                    Some(Value::Str(w)) => {
                        String::from_utf8_lossy(&value::string_bytes(*w))
                            .into_owned()
                    }
                    _ => "error called from Lisp".into(),
                };
                return Err(RunError::Other(msg));
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    // -- Lookup -----------------------------------------------------------

    fn lookup(&self, name: &Rc<String>) -> Result<Value, RunError> {
        // Locals: search frames top to bottom.
        for frame in self.frames.iter().rev() {
            for (n, v) in frame.bindings.iter() {
                if Rc::ptr_eq(n, name) || **n == **name {
                    return Ok(v.clone());
                }
            }
        }
        if let Some(v) = self.globals.get(name) {
            return Ok(v.clone());
        }
        Err(RunError::Unbound(name.to_string()))
    }

    // -- GC integration ---------------------------------------------------

    fn maybe_gc(&mut self) {
        if matches!(self.heap, Heap::Shared(_)) {
            // Multi-mutator: always cooperate (publish roots + park if a
            // peer is collecting), and drive a minor ourselves every so
            // often, since there is no auto-trigger behind a `Mutator`.
            self.poll_shared();
            self.gc_drive_counter += 1;
            if self.gc_drive_counter >= SHARED_DRIVE_EVERY {
                self.gc_drive_counter = 0;
                self.do_collect(GcKind::Minor);
            }
        } else if self.heap.should_collect() {
            // Single-mutator: defer the trigger decision to the heap;
            // `collect_auto` picks minor vs major from Tenured pressure.
            self.do_collect(GcKind::Auto);
        }
    }

    /// Multi-mutator safepoint: publish our roots and cooperate (park if a
    /// peer is driving a collection), then write the forwarded values
    /// back. No-op in `Direct` mode.
    fn poll_shared(&mut self) {
        let (mut roots, layout) = self.gather_roots();
        if let Heap::Shared(m) = &mut self.heap {
            m.poll_safepoint(&mut roots);
        }
        self.scatter_roots(&layout, &roots);
    }

    pub fn collect_minor(&mut self) {
        self.do_collect(GcKind::Minor);
    }

    pub fn collect_major(&mut self) {
        self.do_collect(GcKind::Major);
    }

    fn do_collect(&mut self, kind: GcKind) {
        // Gather every live Word into a flat Vec (we can't hand pointers
        // into `self` to the collector — `self` is borrowed), run the
        // collection so it forwards them in place, then write them back.
        let (mut roots, layout) = self.gather_roots();
        let mut did_minor = 0usize;
        let mut did_major = 0usize;

        match &mut self.heap {
            Heap::Direct(h) => match kind {
                GcKind::Minor => {
                    h.collect_minor(|evac: &mut PageEvacuator<'_, LispLayout>| {
                        for r in roots.iter_mut() {
                            evac.visit(r);
                        }
                    });
                    h.recompute_auto_trigger();
                    did_minor = 1;
                }
                GcKind::Major => {
                    h.collect_major(|evac| {
                        for r in roots.iter_mut() {
                            evac.visit(r);
                        }
                    });
                    h.recompute_auto_trigger();
                    did_major = 1;
                }
                GcKind::Auto => {
                    let picked_major = h.should_collect_major();
                    let _ = h.collect_auto(|evac| {
                        for r in roots.iter_mut() {
                            evac.visit(r);
                        }
                    });
                    if picked_major {
                        did_major = 1;
                    } else {
                        did_minor = 1;
                    }
                }
            },
            Heap::Shared(m) => match kind {
                // The driver self-parks, stops the world, and forwards
                // these roots (plus every peer's published roots) in place.
                GcKind::Major => {
                    m.collect_full(&mut roots, |_| {});
                    did_major = 1;
                }
                _ => {
                    m.collect_minor(&mut roots, |_| {});
                    did_minor = 1;
                }
            },
        }

        self.stats.minor_gcs += did_minor;
        self.stats.major_gcs += did_major;
        self.scatter_roots(&layout, &roots);
    }

    /// Gather every live `Word` slot (value_stack + frames + globals) into
    /// a flat `Vec<Word>` plus a parallel `layout` describing where each
    /// came from, so the forwarded values can be written back afterward.
    /// Shared between the driven-collection path and the shared poll.
    fn gather_roots(&self) -> (Vec<Word>, Vec<RootSlot>) {
        let mut roots: Vec<Word> = Vec::new();
        let mut layout: Vec<RootSlot> = Vec::new();
        for (i, v) in self.value_stack.iter().enumerate() {
            collect_value_words(v, &mut roots, &mut layout, RootKind::ValueStack(i));
        }
        for (frame_i, frame) in self.frames.iter().enumerate() {
            for (binding_i, (_, v)) in frame.bindings.iter().enumerate() {
                collect_value_words(
                    v,
                    &mut roots,
                    &mut layout,
                    RootKind::Frame {
                        frame: frame_i,
                        binding: binding_i,
                    },
                );
            }
        }
        for (name, v) in self.globals.iter() {
            collect_value_words(v, &mut roots, &mut layout, RootKind::Global(name.clone()));
        }
        (roots, layout)
    }

    /// Write forwarded `Word`s back into their value slots after a cycle.
    fn scatter_roots(&mut self, layout: &[RootSlot], roots: &[Word]) {
        for (idx, slot) in layout.iter().enumerate() {
            apply_redistribute(self, slot, roots[idx]);
        }
    }
}

// =========================================================================
// Root collection / redistribution helpers
// =========================================================================

#[derive(Clone, Debug)]
enum RootKind {
    ValueStack(usize),
    Frame { frame: usize, binding: usize },
    Global(Rc<String>),
}

#[derive(Clone, Debug)]
struct RootSlot {
    kind: RootKind,
}

fn collect_value_words(
    v: &Value,
    roots: &mut Vec<Word>,
    layout: &mut Vec<RootSlot>,
    kind: RootKind,
) {
    match v {
        Value::Pair(w) | Value::Vector(w) | Value::Str(w) => {
            roots.push(*w);
            layout.push(RootSlot { kind });
        }
        _ => {}
    }
}

fn apply_redistribute(interp: &mut Interpreter, slot: &RootSlot, new: Word) {
    let target = match &slot.kind {
        RootKind::ValueStack(i) => &mut interp.value_stack[*i],
        RootKind::Frame { frame, binding } => {
            &mut interp.frames[*frame].bindings[*binding].1
        }
        RootKind::Global(name) => {
            // Globals is HashMap — get_mut works.
            interp.globals.get_mut(name).expect("global vanished mid-GC")
        }
    };
    match target {
        Value::Pair(w) | Value::Vector(w) | Value::Str(w) => *w = new,
        _ => panic!("redistribution slot is not a heap value"),
    }
}
