// SunSpider benchmark: string-validate-input
// String validation with regex matching

var letters = /[a-zA-Z]/;
var numbers = /[0-9]/;
var alphanum = /[a-zA-Z0-9]/;

function buildInput() {
    var chars = "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    var result = "";
    for (var i = 0; i < 500; i = i + 1) {
        var idx = Math.floor(Math.random() * chars.length);
        result = result + chars.charAt(idx);
    }
    return result;
}

function validateInput(input) {
    var valid = true;
    var letterCount = 0;
    var numberCount = 0;
    for (var i = 0; i < input.length; i = i + 1) {
        var ch = input.charAt(i);
        if (letters.test(ch)) {
            letterCount = letterCount + 1;
        } else if (numbers.test(ch)) {
            numberCount = numberCount + 1;
        } else {
            valid = false;
        }
    }
    return valid;
}

for (var i = 0; i < 100; i = i + 1) {
    var input = buildInput();
    validateInput(input);
}
console.log("done");
