// SunSpider benchmark: access-fannkuch
// Fannkuch benchmark (pancake flipping)

function fannkuch(n) {
    var check = 0;
    var perm = [];
    var perm1 = [];
    var count = [];
    var maxPerm = [];
    var maxFlipsCount = 0;
    var m = n - 1;

    for (var i = 0; i < n; i = i + 1) {
        perm1[i] = i;
    }
    var r = n;

    while (true) {
        while (r !== 1) { count[r - 1] = r; r = r - 1; }

        if (!(perm1[0] === 0 || perm1[m] === m)) {
            for (var i = 0; i < n; i = i + 1) { perm[i] = perm1[i]; }

            var flipsCount = 0;
            var k;

            while (!((k = perm[0]) === 0)) {
                var k2 = (k + 1) >> 1;
                for (var i = 0; i < k2; i = i + 1) {
                    var temp = perm[i];
                    perm[i] = perm[k - i];
                    perm[k - i] = temp;
                }
                flipsCount = flipsCount + 1;
            }

            if (flipsCount > maxFlipsCount) {
                maxFlipsCount = flipsCount;
                for (var i = 0; i < n; i = i + 1) { maxPerm[i] = perm1[i]; }
            }
        }

        while (true) {
            if (r === n) { return maxFlipsCount; }
            var perm0 = perm1[0];
            var i = 0;
            while (i < r) {
                var j = i + 1;
                perm1[i] = perm1[j];
                i = j;
            }
            perm1[r] = perm0;

            count[r] = count[r] - 1;
            if (count[r] > 0) { break; }
            r = r + 1;
        }
    }
}

var n = 9;
var ret = fannkuch(n);
console.log("done");
