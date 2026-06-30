// String-op heavy: charCodeAt / fromCharCode / concat in a hot loop,
// without the property/branch mix of decode.
var src = "The quick brown fox jumps over the lazy dog 0123456789";
var total = 0;
for (var k = 0; k < 200000; k++) {
  var out = "";
  for (var i = 0; i < src.length; i++) {
    var c = src.charCodeAt(i);
    out += String.fromCharCode(((c + k) % 95) + 32);
  }
  total += out.length;
}
console.log(total);
