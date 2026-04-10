// SunSpider benchmark: bitops-nsieve-bits
// Sieve of Eratosthenes using bit manipulation

function primes(isPrime, n) {
    var count = 0;
    for (var i = 2; i <= n; i = i + 1) { isPrime[i] = true; }
    for (var i = 2; i <= n; i = i + 1) {
        if (isPrime[i]) {
            for (var k = i + i; k <= n; k = k + i) {
                isPrime[k] = false;
            }
            count = count + 1;
        }
    }
    return count;
}

function nsievebits(m) {
    var isPrime = [];
    for (var i = 0; i <= m; i = i + 1) { isPrime[i] = false; }
    return primes(isPrime, m);
}

var result = 0;
for (var i = 0; i < 4; i = i + 1) {
    result = nsievebits(40000);
}
console.log("done");
