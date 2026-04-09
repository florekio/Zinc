// SunSpider benchmark: controlflow-recursive
function ack(m, n) {
    if (m === 0) return n + 1;
    if (n === 0) return ack(m - 1, 1);
    return ack(m - 1, ack(m, n - 1));
}

function fib(n) {
    if (n < 2) return 1;
    return fib(n - 1) + fib(n - 2);
}

function tak(x, y, z) {
    if (y >= x) return z;
    return tak(tak(x - 1, y, z), tak(y - 1, z, x), tak(z - 1, x, y));
}

for (var i = 0; i < 5; i++) {
    ack(3, 9);
    fib(27);
    tak(18, 12, 6);
}
console.log("done");
