// Zinc JavaScript Engine Demo

// 1. Basic arithmetic
console.log("=== Arithmetic ===");
console.log("2 + 3 =", 2 + 3);
console.log("2 ** 10 =", 2 ** 10);
console.log("17 % 5 =", 17 % 5);

// 2. String operations
console.log("=== Strings ===");
var greeting = "Hello" + ", " + "World!";
console.log(greeting);

// 3. Variables and control flow
console.log("=== Control Flow ===");
var x = 42;
if (x > 40) {
    console.log("x is greater than 40");
} else {
    console.log("x is not greater than 40");
}

// 4. Loops
console.log("=== Loops ===");
var sum = 0;
for (var i = 1; i <= 100; i = i + 1) {
    sum = sum + i;
}
console.log("Sum 1-100:", sum);

// 5. Functions
console.log("=== Functions ===");
function factorial(n) {
    if (n <= 1) return 1;
    return n * factorial(n - 1);
}
console.log("10! =", factorial(10));

// 6. Fibonacci (recursive)
function fibonacci(n) {
    if (n <= 1) return n;
    return fibonacci(n - 1) + fibonacci(n - 2);
}
console.log("fib(20) =", fibonacci(20));

// 7. Logical operators
console.log("=== Logic ===");
console.log("true && false =", true && false);
console.log("null ?? 42 =", null ?? 42);
console.log("0 || 'default' =", 0 || "default");

// 8. typeof
console.log("=== typeof ===");
console.log("typeof 42 =", typeof 42);
console.log("typeof 'hello' =", typeof "hello");
console.log("typeof true =", typeof true);
console.log("typeof null =", typeof null);
console.log("typeof undefined =", typeof undefined);

console.log("=== Done! ===");
