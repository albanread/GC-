//! Smoke tests — verify the evaluator runs basic forms correctly.
//!
//! If these fail, the bug is in the evaluator, not the GC. They
//! exist to keep "GC works" and "evaluator works" separable.

use newgc_test_lisp::{Interpreter, Value};

fn interp() -> Interpreter {
    let mut i = Interpreter::new(8 * 64 * 1024);
    i.set_minor_threshold(100);
    i
}

fn expect_number(v: Value) -> i64 {
    match v {
        Value::Number(n) => n,
        other => panic!("expected number, got {other:?}"),
    }
}

#[test]
fn arithmetic() {
    let mut i = interp();
    let v = i.run_source("(+ 1 2 3 4 5)").unwrap();
    assert_eq!(expect_number(v), 15);
    let v = i.run_source("(- 10 3 2)").unwrap();
    assert_eq!(expect_number(v), 5);
    let v = i.run_source("(* 2 3 4)").unwrap();
    assert_eq!(expect_number(v), 24);
    let v = i.run_source("(- 7)").unwrap();
    assert_eq!(expect_number(v), -7);
}

#[test]
fn boolean_and_comparison() {
    let mut i = interp();
    assert!(matches!(i.run_source("(= 1 1)").unwrap(), Value::Bool(true)));
    assert!(matches!(i.run_source("(< 1 2)").unwrap(), Value::Bool(true)));
    assert!(matches!(i.run_source("(> 1 2)").unwrap(), Value::Bool(false)));
    assert!(matches!(i.run_source("#t").unwrap(), Value::Bool(true)));
    assert!(matches!(i.run_source("#f").unwrap(), Value::Bool(false)));
}

#[test]
fn if_form() {
    let mut i = interp();
    let v = i.run_source("(if (= 1 1) 42 99)").unwrap();
    assert_eq!(expect_number(v), 42);
    let v = i.run_source("(if (= 1 2) 42 99)").unwrap();
    assert_eq!(expect_number(v), 99);
}

#[test]
fn let_form() {
    let mut i = interp();
    let v = i.run_source("(let ((x 1) (y 2)) (+ x y))").unwrap();
    assert_eq!(expect_number(v), 3);
}

#[test]
fn cons_and_list_access() {
    let mut i = interp();
    let v = i.run_source(r#"
        (define xs (cons 1 (cons 2 (cons 3 nil))))
        (car xs)
    "#).unwrap();
    assert_eq!(expect_number(v), 1);
    let v = i.run_source("(car (cdr xs))").unwrap();
    assert_eq!(expect_number(v), 2);
    let v = i.run_source("(length xs)").unwrap();
    assert_eq!(expect_number(v), 3);
}

#[test]
fn list_builtin() {
    let mut i = interp();
    let v = i.run_source("(length (list 10 20 30 40 50))").unwrap();
    assert_eq!(expect_number(v), 5);
    let v = i.run_source("(car (cdr (cdr (list 10 20 30))))").unwrap();
    assert_eq!(expect_number(v), 30);
}

#[test]
fn vector_ops() {
    let mut i = interp();
    let v = i.run_source(r#"
        (define v (make-vector 5 0))
        (vector-set! v 0 100)
        (vector-set! v 1 200)
        (vector-set! v 2 300)
        (+ (vector-ref v 0) (vector-ref v 1) (vector-ref v 2))
    "#).unwrap();
    assert_eq!(expect_number(v), 600);
    let v = i.run_source("(vector-length v)").unwrap();
    assert_eq!(expect_number(v), 5);
}

#[test]
fn vector_literal() {
    let mut i = interp();
    let v = i.run_source("(vector-ref (vector 10 20 30) 1)").unwrap();
    assert_eq!(expect_number(v), 20);
}

#[test]
fn string_length() {
    let mut i = interp();
    let v = i.run_source(r#"(string-length "hello")"#).unwrap();
    assert_eq!(expect_number(v), 5);
    let v = i.run_source(r#"(string-length "")"#).unwrap();
    assert_eq!(expect_number(v), 0);
}

#[test]
fn user_function_factorial() {
    let mut i = interp();
    let v = i.run_source(r#"
        (define (fact n)
          (if (= n 0)
              1
              (* n (fact (- n 1)))))
        (fact 8)
    "#).unwrap();
    assert_eq!(expect_number(v), 40320);
}

#[test]
fn user_function_mutual_recursion() {
    let mut i = interp();
    let v = i.run_source(r#"
        (define (even? n) (if (= n 0) #t (odd? (- n 1))))
        (define (odd? n)  (if (= n 0) #f (even? (- n 1))))
        (if (even? 100) 1 0)
    "#).unwrap();
    assert_eq!(expect_number(v), 1);
}

#[test]
fn begin_returns_last() {
    let mut i = interp();
    let v = i.run_source("(begin 1 2 3 4 5)").unwrap();
    assert_eq!(expect_number(v), 5);
}

#[test]
fn set_locals_and_globals() {
    let mut i = interp();
    let v = i.run_source(r#"
        (define x 10)
        (set! x 20)
        x
    "#).unwrap();
    assert_eq!(expect_number(v), 20);
}

#[test]
fn predicates() {
    let mut i = interp();
    assert!(matches!(
        i.run_source("(null? nil)").unwrap(),
        Value::Bool(true)
    ));
    assert!(matches!(
        i.run_source("(pair? (cons 1 2))").unwrap(),
        Value::Bool(true)
    ));
    assert!(matches!(
        i.run_source("(number? 42)").unwrap(),
        Value::Bool(true)
    ));
    assert!(matches!(
        i.run_source(r#"(string? "hi")"#).unwrap(),
        Value::Bool(true)
    ));
    assert!(matches!(
        i.run_source("(vector? (vector 1 2 3))").unwrap(),
        Value::Bool(true)
    ));
}
