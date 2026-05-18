; Old→young mutation: build a container in old gen, then mutate its
; slots to point at freshly-allocated young objects. Soft card barrier
; (sub-phase 9 of GC_DESIGN.md) handles this transparently — no manual
; `(gc-major)` cadence or threshold override needed.

(define container (make-vector 30 nil))

(define (build-row n)
  (if (= n 0) nil (cons n (build-row (- n 1)))))

(define (fill i)
  (if (= i 30)
      nil
      (begin
        (vector-set! container i (build-row (+ i 1)))
        (fill (+ i 1)))))

(fill 0)

; Promote container to old gen via two major cycles.
(gc-major)
(gc-major)

(define (verify i)
  (if (= i 30)
      #t
      (and (= (length (vector-ref container i)) (+ i 1))
           (verify (+ i 1)))))

(assert (verify 0))

; Mutate: replace each slot with a NEW list of length 100-i. The new
; lists are allocated in G0; the container is in old gen. The card
; barrier finds the cross-gen pointers during minor and major cycles
; that fire automatically as remutate allocates.

(define (remutate i)
  (if (= i 30)
      nil
      (begin
        (vector-set! container i (build-row (- 100 i)))
        (remutate (+ i 1)))))

(remutate 0)

(define (verify-new i)
  (if (= i 30)
      #t
      (and (= (length (vector-ref container i)) (- 100 i))
           (verify-new (+ i 1)))))

(assert (verify-new 0))
