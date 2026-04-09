// SunSpider benchmark: bitops-3bit-bits-in-byte
function fast3bitlookup(b) {
    var c, bi3b = 0xE994;
    c  = 3 & (bi3b >> ((b << 1) & 14));
    c += 3 & (bi3b >> ((b >> 2) & 14));
    c += 3 & (bi3b >> ((b >> 5) & 6));
    return c;
}

function TimeFunc() {
    var sum = 0;
    for (var x = 0; x < 500; x++) {
        for (var y = 0; y < 256; y++) {
            sum += fast3bitlookup(y);
        }
    }
    return sum;
}

var result = TimeFunc();
console.log(result);
