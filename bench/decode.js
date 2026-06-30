// Lighthouse benchmark: mirrors the DuckDuckGo SERP hot path — a JS
// decodeURIComponent polyfill (like the page's chunk 78) applied across a
// large collection of percent-encoded items. This is the workload that
// exceeds copper's fuel budget today.
function jsDecode(s) {
  s = String(s);
  var out = "";
  for (var i = 0; i < s.length; i++) {
    if (s.charAt(i) === "%" && i + 2 < s.length) {
      var code = parseInt(s.substr(i + 1, 2), 16);
      if (!isNaN(code)) { out += String.fromCharCode(code); i += 2; }
      else { out += s.charAt(i); }
    } else {
      out += s.charAt(i);
    }
  }
  return out;
}
// Build a realistic encoded item (label + percent-encoded payload).
var item = "Result%20title%20%E2%80%94%20with%20%22quotes%22%20and%20%2Fslashes%2F%20%26amp%3B";
// A "collection" of ~3000 items decoded once each (search-result-set scale).
var N = 20000;
var acc = 0;
for (var k = 0; k < N; k++) {
  var d = jsDecode(item + "%20" + k);
  acc += d.length;
}
console.log(acc);
