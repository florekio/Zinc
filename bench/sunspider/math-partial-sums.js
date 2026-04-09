// SunSpider benchmark: math-partial-sums
function partial(n) {
    var a1 = 0, a2 = 0, a3 = 0, a4 = 0, a5 = 0;
    var a6 = 0, a7 = 0, a8 = 0, a9 = 0;
    var twothirds = 2.0 / 3.0;
    var alt = -1.0;
    var k2, k3, sk, ck;

    for (var k = 1; k <= n; k++) {
        k2 = k * k;
        k3 = k2 * k;
        sk = Math.sin(k);
        ck = Math.cos(k);
        alt = -alt;

        a1 = a1 + Math.pow(twothirds, k - 1);
        a2 = a2 + Math.pow(k, -0.5);
        a3 = a3 + 1.0 / (k * (k + 1.0));
        a4 = a4 + 1.0 / (k3 * sk * sk);
        a5 = a5 + 1.0 / (k3 * ck * ck);
        a6 = a6 + 1.0 / k;
        a7 = a7 + 1.0 / k2;
        a8 = a8 + alt / k;
        a9 = a9 + alt / (2 * k - 1);
    }
    return a6;
}

var result = 0;
for (var i = 0; i < 4; i++) {
    result = partial(25000);
}
console.log(Math.round(result * 1000) / 1000);
