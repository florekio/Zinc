function makeCounter() {
    var count = 0;
    function increment() {
        count = count + 1;
        return count;
    }
    return increment;
}
var counter = makeCounter();
counter();
counter();
counter();
