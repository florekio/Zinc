// Property-access + method-call heavy: exercises inline caches and the
// CallMethod path (the SERP predicate called 133k+ times).
function Box(v){ this.v = v; }
Box.prototype.get = function(){ return this.v; };
Box.prototype.bump = function(n){ this.v = this.v + n; return this; };
var boxes = [];
for (var i = 0; i < 5000; i++) { boxes.push(new Box(i)); }
var sum = 0;
for (var pass = 0; pass < 400; pass++) {
  for (var j = 0; j < boxes.length; j++) {
    sum += boxes[j].bump(1).get();
  }
}
console.log(sum);
