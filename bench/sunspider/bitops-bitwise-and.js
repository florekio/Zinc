// SunSpider benchmark: bitops-bitwise-and
// Tight loop with bitwise AND operations

var birone = 0;
var birrone = 0;

for (var i = 0; i < 600000; i = i + 1) {
    birone = birone & i;
    birrone = birrone & i;
    birone = birone & i;
    birrone = birrone & i;
    birone = birone & i;
    birrone = birrone & i;
    birone = birone & i;
    birrone = birrone & i;
    birone = birone & i;
    birrone = birrone & i;
    birone = birone & i;
    birrone = birrone & i;
    birone = birone & i;
    birrone = birrone & i;
    birone = birone & i;
    birrone = birrone & i;
}
console.log("done");
