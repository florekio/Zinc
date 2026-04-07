function makeCounter() {
    var count = 0;
    function inc() {
        count = count + 1;
        return count;
    }
    return inc;
}
var counter = makeCounter();
var result = 0;
for (var i = 0; i < 100000; i = i + 1) {
    result = counter();
}
console.log(result);
