// Numeric tight loop — already covered by the partial JIT (chunk-0 loops).
// Regression guard: this must stay fast.
var s = 0;
for (var i = 0; i < 20000000; i++) { s += (i * 3 + 7) % 13; }
console.log(s);
