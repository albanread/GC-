; Build a 200-cell list. GC several times. Verify length + contents survive.
(define (count-down n)
  (if (= n 0)
      nil
      (cons n (count-down (- n 1)))))

(define xs (count-down 200))
(check-equal 200 (length xs))

; Force multiple GCs.
(gc-major)
(gc-major)
(gc-now)
(gc-major)

; List must still walk.
(check-equal 200 (length xs))

; Walk and verify contents (descending 200..1).
(define (walk-check xs expected)
  (if (null? xs)
      (= expected 0)
      (and (= (car xs) expected)
           (walk-check (cdr xs) (- expected 1)))))

(assert (walk-check xs 200))
