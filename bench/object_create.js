var last;
for (var i = 0; i < 100000; i = i + 1) {
    last = { x: i, y: i + 1, z: i + 2 };
}
console.log(last.x + last.y + last.z);
