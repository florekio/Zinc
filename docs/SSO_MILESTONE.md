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

## Results (all phases shipped)

- **test262 unchanged at 14982** across A/B/C — net zero, as required.
- **decode.js: 0.78s → 0.75s, ~49M → ~54M ops/sec** (~8%). Modest because the
  *dominant* decode cost is `charAt`/`substr` on the long receiver string
  (O(n) per access), which SSO does not touch — that needs O(1) string
  indexing, a separate milestone. SSO removed the per-result allocations.
- **Interner growth on decode halved (40k → 20k).** The residual 20k is
  `String(consArg)` interning the unique per-iteration argument once so the
  loop's repeated `charAt` resolves in O(1) — a deliberate trade, not waste.

## Lesson: the embedder string-reading surface

test262 exercises the **language**, not the **embedder API**. A regression hid
in `Vm::string_content` (the DOM bindings' `read_str`): it gated on
`as_string_id().is_some()`, which is false for inline strings, so it returned
`None` and `__addEventListener` silently dropped React's `"click"` listener
(the event name is derived via `slice(2).toLowerCase()` → a 5-byte inline
string). test262 stayed green; the **React smoke caught it**. When changing the
`Value` representation, audit the public `Vm` methods embedders call (and
`as_string_id().is_some()/.is_none()`, not just `.unwrap()`), and run the
real-page smokes — they cover what test262 structurally cannot.
