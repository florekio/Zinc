# Milestone: Stop interning transient string values

> **Superseded by [SSO_MILESTONE.md](SSO_MILESTONE.md).** The FlatString
> (heap-object) approach scoped here measured ~15% *slower* on decode (GC of
> per-string heap objects cost more than the interner's leak). It was
> abandoned in favour of inline small strings (SSO): strings ≤5 bytes packed
> directly into the NaN-boxed `Value` — no heap, no GC, no interning. This
> document is kept for the problem analysis and the measurement that ruled
> the heap approach out.

## Problem

copper interns **every** string value through `Interner` (`Value::string(id)`
holds an interned `StringId`). Every `charAt`/`substr`/`slice`/`fromCharCode`/
`toUpperCase`/template/`String(x)` result is interned. On the decode (SERP) path
the CPU profile shows `Interner::intern` + the string-content allocations
dominating the called leaves. Two costs:

1. **Speed:** an allocation + FxHash insert per produced string value.
2. **Memory (correctness):** the interner grows **unboundedly** — a long page
   that produces millions of distinct strings (the SERP decodes thousands of
   results) keeps every one alive forever in the interner map. Suspected major
   contributor to the SERP being slow/heavy, on top of raw throughput.

## Why this is tractable (key finding)

The engine **already** separates "a string value" from "an interned id". A
string value is either:
- `TAG_STRING` (NaN-boxed) carrying a `StringId` — interned; or
- an `object_id` → `ObjectKind::ConsString { left, right, len }` — a deferred
  concatenation that is **not interned**.

`as_string_id()` returns `None` for ConsString. Code that needs an id calls
`flatten_to_string_id()` (intern-on-demand); the polymorphic accessors
(`is_string_like`, `string_char_len`, `flatten_cons_to_string`) and
`strict_eq` already handle the non-interned case (content compare). So a
non-interned string value is an existing, exercised pattern.

**This milestone adds a *flat leaf* variant of that pattern** —
`ObjectKind::FlatString` — and routes string-*producing* ops to it instead of
interning. Interning stays only where an id is genuinely needed (property keys,
identifiers, bytecode string literals).

## Design

### 1. New value kind
`ObjectKind::FlatString(Box<str>)` — immutable, owned, not interned. Optionally
cache `char_len: u32` alongside (for O(1) `.length`; ConsString already caches
`len`). GC `trace` is a no-op (no child Values); the `Box<str>` frees on sweep.

### 2. Constructor choke point
`fn new_string_value(&mut self, s: String) -> Value` — allocate a `FlatString`
heap object, return its `object_id` Value. **Every string-producing site uses
this instead of `Value::string(self.interner.intern(&s))`.**

### 3. Extend the polymorphic helpers (add one `FlatString` arm each)
- `is_string_like` / `is_cons_string`-style checks → include FlatString.
- `string_char_len` → FlatString leaf (cached or counted).
- `flatten_cons_to_string` → FlatString leaf pushes its content.
- `value_to_string` → FlatString returns its content.
- `strict_eq` / `try_abstract_eq` → already content-compare `is_string_like`;
  Just Works once FlatString is `is_string_like`.
- `flatten_to_string_id` → FlatString interns on demand (the key path).

### 4. Route producers to `new_string_value` (stop interning)
Primary: `exec_string_method` results (~39 intern sites in builtins.rs):
`charAt`, `substr`, `substring`, `slice`, `toUpperCase`, `toLowerCase`,
`trim*`, `replace*`, `repeat`, `pad*`, `concat`, `split` (array of FlatStrings),
`normalize`, etc. Plus `String.fromCharCode`/`fromCodePoint`, `String(x)`
coercion, template-literal concatenation, and **ConsString flattening produces a
FlatString** (not an interned id) unless an id is requested.

### 5. Interning stays for
Property keys (`GetProperty`/`SetProperty`/`CallMethod` computed keys already
funnel through `flatten_to_string_id`), identifiers, and `Const` string literals
(compiler pre-interns these — leave as-is). So a FlatString used as a key
auto-interns exactly once, when used as a key.

## Migration surface & risks

- **`as_string_id()` callers (~62 in vm.rs/builtins.rs)** — the audit. Any that
  assume a string value is a bare id must switch to `flatten_to_string_id` (need
  id) or `value_to_string`/`flatten_cons_to_string` (need content). Since
  ConsString already returns `None` there, most either handle it or are only
  reached with interned strings — but each must be checked.
- **Equality / ordering / keys (#1 correctness risk):** `===`, `==`, `<`,
  `switch`, `Map`/`Set` keys, object property keys. Content compare already
  exists for `is_string_like`; verify Map/Set key hashing and `switch` use it.
  Heavy test262 coverage here — it's the safety net.
- **Performance trade:** content-compare is O(n) vs interned id compare O(1).
  Frequently-compared strings (Map keys, hot `===`) should be interned — the
  flatten-on-key-use already achieves this for keys; watch hot `===` of long
  strings.
- **GC pressure:** FlatStrings are heap objects (more GC traffic than interned),
  but interning was *also* a heap String + map entry that never freed. Net
  expected better: transient strings now get collected instead of living forever
  in the interner.

## Phasing (de-risked, test262 ≥ 93.0% at every step)

- **Phase A — infrastructure, no behavior change.** Add `FlatString` kind,
  `new_string_value`, extend all polymorphic helpers + GC trace. Nothing
  produces FlatString yet. Land; test262 unchanged.
- **Phase B — hot producers (the win).** Route the decode-path producers to
  FlatString: `charAt`, `substr`, `slice`, `fromCharCode`, `toUpper/Lower`, and
  ConsString→FlatString flattening. Measure `bench/decode.js` + `strings.js`;
  test262 must hold.
- **Phase C — remaining producers** (`replace`, `split`, `pad*`, `String()`,
  templates, …). Full audit of `as_string_id` callers.
- **Phase D — verify the memory win.** Add a metric: `Interner::len()` after
  `decode.js` should drop from ~millions to ~hundreds. Re-profile for speed.

## Acceptance criteria

- test262 ≥ 93.0% (no string-semantics regressions) at every phase.
- `bench/decode.js` and `strings.js` faster (target: the intern/alloc share of
  the profile largely gone).
- Interner size after `decode.js` is bounded (small), proving transient values
  are no longer interned.
- e2e string tests green.

## Estimate

Phase A: ~1–2 days (mechanical, low risk). Phase B: ~2–3 days incl. measurement.
Phase C: ~2–4 days (the `as_string_id` audit is the long pole). Phase D: ~1 day.
Total ~1.5–2 weeks. Risk concentrated in equality/key correctness (Phase B/C),
covered by test262. Higher leverage and more contained than the function-JIT,
and it fixes a real memory bug as well as speed.
