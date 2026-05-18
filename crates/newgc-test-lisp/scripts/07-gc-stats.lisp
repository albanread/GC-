; Exercise the heap-stats accessors. They expose the same fields as
; `newgc_core::GcStats` so a Lisp program can introspect the GC.
;
; Set a high threshold so auto-minor doesn't fire mid-test and move
; the data we're trying to inspect.
(set-gc-threshold! 1000000)

(define (build-row n)
  (if (= n 0) nil (cons n (build-row (- n 1)))))

; Fresh heap (post-define): zero used.
(check-equal 0 (heap-tenured-used-bytes))

; Allocate a 50-cell list. 50 cons × 16 bytes = 800 bytes, all in G0.
(define xs (build-row 50))
(check-equal 800 (heap-g0-used-bytes))
(check-equal 800 (heap-bytes-alloc-since-gc))

; Major collection. collect_major does G1→Tenured then G0→G0
; (within-gen for G0), so xs's data moves within G0 to fresh
; addresses but stays in G0. bytes-alloc-since-gc resets.
(gc-major)
(check-equal 0 (heap-bytes-alloc-since-gc))
; xs still walks (data preserved across the within-gen evac).
(check-equal 50 (length xs))
; G0 still holds the live data (major doesn't promote G0).
(check-equal 800 (heap-g0-used-bytes))

; Drop the root: next major reclaims G0.
(set! xs nil)
(gc-major)
(check-equal 0 (heap-g0-used-bytes))
(check-equal 0 (heap-used-bytes))
