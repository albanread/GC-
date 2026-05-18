; Build a DAG: one shared "leaf" referenced from 100 cons cells. After GC,
; verify all references point at the SAME object (eq?).

(define shared (cons 42 nil))

(define (build n)
  (if (= n 0)
      nil
      (cons shared (build (- n 1)))))

(define xs (build 100))

(gc-major)

; Walk and check every car is eq? to the shared object.
(define (verify xs)
  (if (null? xs)
      #t
      (and (eq? (car xs) shared)
           (verify (cdr xs)))))

(assert (verify xs))
; And shared's car is still 42.
(check-equal 42 (car shared))
