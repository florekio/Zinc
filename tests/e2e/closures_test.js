function makeCounter() {
    var count = 0;
    function inc() {
        count = count + 1;
        return count;
    }
    return inc;
}
var c = makeCounter();
console.log(c());
console.log(c());
console.log(c());
