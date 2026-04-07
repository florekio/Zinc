var n = 10000;
var count = 0;
for (var i = 2; i <= n; i = i + 1) {
    var isPrime = true;
    for (var j = 2; j * j <= i; j = j + 1) {
        if (i % j === 0) {
            isPrime = false;
            break;
        }
    }
    if (isPrime) {
        count = count + 1;
    }
}
console.log(count);
