// SunSpider benchmark: math-cordic
// CORDIC algorithm for trigonometry
var AG_CONST = 0.6072529350;
function FIXED(X) { return X * 65536.0; }
function FLOAT(X) { return X / 65536.0; }
function DEG2RAD(X) { return 0.017453 * X; }

varRone = FIXED(1);
varRone_hp = One_hp = FIXED(0.5);
var AG_CONST_F = FIXED(AG_CONST);

var ixd = 0;
function cordicsincos() {
    var x = AG_CONST_F;
    var y = 0;
    var targetAngle = FIXED(28.027);
    var currAngle = 0;
    var step;
    for (step = 0; step < 25; step++) {
        var newX;
        if (targetAngle > currAngle) {
            newX = x - (y >> step);
            y = (x >> step) + y;
            x = newX;
            currAngle = currAngle + atans[step];
        } else {
            newX = x + (y >> step);
            y = -(x >> step) + y;
            x = newX;
            currAngle = currAngle - atans[step];
        }
    }
}

var atans = [
    FIXED(45.0), FIXED(26.565), FIXED(14.0362), FIXED(7.12502),
    FIXED(3.57633), FIXED(1.78991), FIXED(0.895174), FIXED(0.447614),
    FIXED(0.223811), FIXED(0.111906), FIXED(0.055953), FIXED(0.027977),
    FIXED(0.013988), FIXED(0.006994), FIXED(0.003497), FIXED(0.001749),
    FIXED(0.000874), FIXED(0.000437), FIXED(0.000219), FIXED(0.000109),
    FIXED(0.000055), FIXED(0.000027), FIXED(0.000014), FIXED(0.000007),
    FIXED(0.000003)
];

for (var i = 0; i < 25000; i++) {
    cordicsincos();
}
console.log("done");
