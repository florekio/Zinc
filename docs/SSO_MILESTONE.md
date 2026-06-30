# Milestone: inline small strings (SSO)

## Why

The DDG SERP / `bench/decode.js` hot path is a percent-decode loop that produces
a flood of **very short, transient** strings — 1-char `charAt`/`fromCharCode`
results, 1–2 char `substr` slices — feeds them through `===`, `+` concat, and
`parseInt`, and discards them. Today every such string is **interned**: a
HashMap insert + a `Box<str>` allocation that is *never freed* (the interner
leaks by design, so keys stay GC-free).

The earlier FlatString experiment (heap-object transient strings) was **~15%
slower** on decode: GC-managed string objects cost more than the interner's
leak. The lesson: for the hot path, the win is **not allocating at all**.

## Approach

Pack strings of **≤5 UTF-8 bytes directly into the NaN-boxed `Value`** — no
heap, no GC, no interner touch. The 3-bit tag space is full, so inline strings
**share `TAG_STRING`** and are distinguished by payload bit 47:

```
TAG_STRING payload (48 bits):
  bit 47        INLINE flag  (0 = interned id, 1 = inline)
  bits 44..=46  length 0..=5
  bits 0..=39   up to 5 raw UTF-8 bytes
```

Sharing `TAG_STRING` is the key simplification: **`is_string()` stays true for
inline strings for free**, so the vast majority of string-handling code is
already correct. Only code that pulls a `StringId` out of a string must adapt.

## Migration surface

`as_string_id()` now returns `None` for inline strings (they have no id). The
risk is the ~30 `as_string_id().unwrap()` sites. They fall into two classes:

- **Property keys / function names** (the majority): compiler-interned
  constants, never inline → safe. Computed keys (`obj[expr]`) *can* be inline →
  those sites must intern-on-demand (rare, correctness-only path).
- **Hot-path value ops** (`===`, `+`/concat, `typeof`, `to_string`): must read
  inline content directly via `as_inline_string()` — fast, no interning.

## Phases

- **Phase A — encoding (done).** `Value::inline_string`, `is_inline_string`,
  `is_interned_string`, `as_inline_string`/`InlineStr`; `as_string_id` guarded;
  in-file `Debug`/`Display`/`to_boolean` made inline-aware. Behavior-neutral —
  nothing produces inline strings yet. test262 unchanged (14982).
- **Phase B — consumers.** Make the central string readers inline-aware:
  string equality, concat/flatten, `value_to_string`/coercion, `typeof`,
  property-key resolution (intern-on-demand fallback). Still no producers →
  behavior-neutral, but defensive. Gate on test262.
- **Phase C — producers.** Route short results to `inline_string` with intern
  fallback: `charAt`, `fromCharCode`, `String.fromCharCode`, `substr`/`slice`/
  `substring`/`at` when ≤5 bytes, single-char index `s[i]`. **This is where
  inline strings first exist** → exercises every Phase B consumer. Gate hard on
  test262, then measure `bench/decode.js`.

## Success criterion

`bench/decode.js` throughput up (target: beat the 0.78s interned baseline) with
test262 net ≥ 0. Interner growth on decode drops toward zero.
