; Build a balanced binary tree of depth 7 (255 leaves + 127 internal vectors),
; force GC, walk the tree and count leaves.

(define (make-leaf v) (vector v nil nil))
(define (make-node v l r) (vector v l r))

(define (build depth value)
  (if (= depth 0)
      (make-leaf value)
      (make-node value
                 (build (- depth 1) (- value 1))
                 (build (- depth 1) (+ value 1)))))

(define t (build 7 0))
(gc-major)
(gc-major)

(define (count n)
  (if (null? n)
      0
      (+ 1
         (count (vector-ref n 1))
         (count (vector-ref n 2)))))

; 2^8 - 1 = 255 nodes for depth 7
(check-equal 255 (count t))

; Sum of leaf "value" fields. Each leaf is at depth 7, with value = 0 + (±1)^7.
; The structure means we have a known invariant; for our test, just sanity-check
; the root's value is unchanged after GC.
(check-equal 0 (vector-ref t 0))
