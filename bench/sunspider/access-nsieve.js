// SunSpider benchmark: access-nsieve
// Sieve of Eratosthenes with boolean array

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

function sieve() {
    for (var i = 1; i <= 3; i = i + 1) {
        var m = (1 << i) * 10000;
        var flags = [];
        for (var j = 0; j <= m; j = j + 1) { flags[j] = false; }
        primes(flags, m);
    }
}

sieve();
console.log("done");
