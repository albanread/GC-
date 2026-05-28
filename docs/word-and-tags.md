# Word and Tags

## The `Word` type

Every value in a NewGC-managed heap — whether on the stack, in a register, or stored in a heap cell — is a `Word`: a 64-bit, 8-byte-aligned integer that encodes both its type and its payload in a single machine word.

```rust
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct Word(u64);
```

`Word` is `Copy` and fits in a register. There is no boxing, no indirection.

## Tag layout

The low 3 bits (`TAG_MASK = 0b111`) hold the tag. The upper 61 bits hold the payload — either a signed integer, a heap pointer (8-byte aligned, so the low 3 bits are always zero and can be safely reused for the tag), or an immediate sub-tag and value.

```
 63                    3  2  1  0
 ┌────────────────────┬──┴──┴──┘
 │   payload (61 b)   │  tag (3 b)
 └────────────────────┴──────────
```

## Tags

| Constant | Bits | Meaning |
|----------|------|---------|
| `Tag::Fixnum` | `000` | Signed 61-bit integer; payload is `value << 3` |
| `Tag::Cons` | `001` | Heap pointer to a 2-cell headerless pair |
| `Tag::Symbol` | `010` | Heap pointer to a header-bearing symbol object |
| `Tag::Vector` | `011` | Heap pointer to a header-bearing vector |
| `Tag::Function` | `100` | Heap pointer to a header-bearing function |
| `Tag::String` | `101` | Heap pointer to a header-bearing string |
| `Tag::Immediate` | `110` | Non-pointer immediate (nil, T, char, unbound) |
| `Tag::Forward` | `111` | GC-internal forwarding marker ("object moved here") |

`Tag::Forward` is never visible to user code; the evacuator writes it into the source cell of a moved object and the next visitor follows it to the new address.

## Fixnums

A fixnum is a 61-bit signed integer. The tag is `000`, so the payload occupies bits 3–63.

```
range: -(2^60) .. (2^60 - 1)
encoding: value << TAG_BITS
```

Because the low 3 bits of a fixnum are always zero, addition and subtraction on raw `Word` values produce correct results without untagging. This is the classic SBCL/Allegro fixnum trick — confirmed by a dedicated test:

```rust
let a = Word::fixnum(123);
let b = Word::fixnum(456);
let sum_raw = Word::from_raw(a.raw().wrapping_add(b.raw()));
assert_eq!(sum_raw.as_fixnum(), Some(123 + 456));
```

`Word::try_fixnum(n)` returns `None` if `n` is out of the 61-bit range. `Word::fixnum(n)` panics in debug builds for the same condition.

## Immediates

The `Immediate` tag (`110`) uses bits 3–7 (a 5-bit subtag) to distinguish several non-pointer singleton values:

| Subtag | Value | Meaning |
|--------|-------|---------|
| `0` | `Word::T` | Canonical truth |
| `1` | `Word::char(c)` | Unicode scalar; payload in bits 8–63 |
| `2` | `Word::UNBOUND` | Unbound symbol or function cell |
| `3` | `Word::NIL` | Empty list / false |

`Word::NIL` is **not** a fixnum zero, even though it encodes the Immediate tag and a zero payload. This matches Common Lisp semantics where `(eq nil 0)` is false. The raw bit pattern of NIL (`immediate(3, 0) = 0b...0001_1110`) is distinct from `Word::fixnum(0)` (raw value `0`).

## Heap pointers

For `Cons`, `Symbol`, `Vector`, `Function`, and `String` tags, the payload is an 8-byte-aligned heap address with the tag OR'd in. To recover the raw pointer:

```rust
let addr = (word.raw() & PAYLOAD_MASK) as *const T;
```

`PAYLOAD_MASK = !TAG_MASK = !0b111`.

The `as_ptr` and `as_mut_ptr` methods do this check and return `None` if the tag doesn't match.

## Forwarding pointers

Tag `111` (`Forward`) is written by the evacuator into the first cell of a moved object. The payload is the new address. Subsequent calls to `classify` on that cell return `WordKind::Forwarded(new_addr)`, telling the evacuator to follow the chain.

```rust
pub fn forward(new_addr: *const ()) -> Word {
    Word(new_addr as u64 | Tag::Forward as u64)
}
```

Forwarding markers are cleared when the page's start-bit entries are zeroed during page reclamation. They are never visible after a collection cycle completes.

## Size guarantee

```rust
assert_eq!(std::mem::size_of::<Word>(), 8);
assert_eq!(std::mem::align_of::<Word>(), 8);
```

A `Word` always occupies exactly one heap cell (8 bytes). The alignment guarantee means tagged pointers never have non-zero low bits from padding or packing.

---

Back to [Home](index.md).
