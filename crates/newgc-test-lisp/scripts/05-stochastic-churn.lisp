; Stochastic allocation churn: random mix of lists/vectors/strings,
; some retained in a slot table, most discarded. Many GCs implicit.

(random-seed! 99)

(define table (make-vector 50 nil))
; Bounded at 100 so the loop recursion stays well under Rust's 1 MB
; stack; the per-frame cost in our tree-walking interpreter is ~3 KB.
(define iters 100)

(define (build-list n)
  (if (= n 0) nil (cons (random 1000) (build-list (- n 1)))))

(define (loop k)
  (if (= k 0)
      nil
      (let ((slot (random 50))
            (kind (random 4)))
        (begin
          (if (= kind 0)
              (vector-set! table slot (build-list (random 20)))
              (if (= kind 1)
                  (vector-set! table slot (make-vector (random 8) 42))
                  (if (= kind 2)
                      (vector-set! table slot "marker-string")
                      ; kind 3: drop the slot
                      (vector-set! table slot nil))))
          (loop (- k 1))))))

(loop iters)

; Count populated slots.
(define (count-from i acc)
  (if (= i 50)
      acc
      (count-from (+ i 1)
                  (if (null? (vector-ref table i))
                      acc
                      (+ acc 1)))))

; Final GC and verify everything walks correctly.
(gc-major)
(gc-major)

; populated should be > 0 (statistically extremely likely with 250 iters
; into 50 slots).
(assert (> (count-from 0 0) 0))

; Walk each populated slot and dispatch on type. If any is corrupted,
; the type-specific operation will fail.
(define (verify-from i)
  (if (= i 50)
      #t
      (let ((v (vector-ref table i)))
        (begin
          (if (null? v) #t
            (if (pair? v) (begin (length v) #t)
              (if (vector? v) (begin (vector-length v) #t)
                (if (string? v) (begin (string-length v) #t)
                  (error "unknown slot type")))))
          (verify-from (+ i 1))))))

(assert (verify-from 0))
