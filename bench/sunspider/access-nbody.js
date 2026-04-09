// SunSpider benchmark: access-nbody (simplified n-body simulation)
var PI = 3.141592653589793;
var SOLAR_MASS = 4 * PI * PI;
var DAYS_PER_YEAR = 365.24;

function Body(x, y, z, vx, vy, vz, mass) {
    this.x = x;
    this.y = y;
    this.z = z;
    this.vx = vx;
    this.vy = vy;
    this.vz = vz;
    this.mass = mass;
}

function advance(bodies, dt) {
    var size = bodies.length;
    for (var i = 0; i < size; i++) {
        var bodyi = bodies[i];
        for (var j = i + 1; j < size; j++) {
            var bodyj = bodies[j];
            var dx = bodyi.x - bodyj.x;
            var dy = bodyi.y - bodyj.y;
            var dz = bodyi.z - bodyj.z;
            var distance = Math.sqrt(dx * dx + dy * dy + dz * dz);
            var mag = dt / (distance * distance * distance);
            bodyi.vx = bodyi.vx - dx * bodyj.mass * mag;
            bodyi.vy = bodyi.vy - dy * bodyj.mass * mag;
            bodyi.vz = bodyi.vz - dz * bodyj.mass * mag;
            bodyj.vx = bodyj.vx + dx * bodyi.mass * mag;
            bodyj.vy = bodyj.vy + dy * bodyi.mass * mag;
            bodyj.vz = bodyj.vz + dz * bodyi.mass * mag;
        }
    }
    for (var i = 0; i < size; i++) {
        var body = bodies[i];
        body.x = body.x + dt * body.vx;
        body.y = body.y + dt * body.vy;
        body.z = body.z + dt * body.vz;
    }
}

function energy(bodies) {
    var e = 0;
    var size = bodies.length;
    for (var i = 0; i < size; i++) {
        var bodyi = bodies[i];
        e = e + 0.5 * bodyi.mass * (bodyi.vx * bodyi.vx + bodyi.vy * bodyi.vy + bodyi.vz * bodyi.vz);
        for (var j = i + 1; j < size; j++) {
            var bodyj = bodies[j];
            var dx = bodyi.x - bodyj.x;
            var dy = bodyi.y - bodyj.y;
            var dz = bodyi.z - bodyj.z;
            var distance = Math.sqrt(dx * dx + dy * dy + dz * dz);
            e = e - (bodyi.mass * bodyj.mass) / distance;
        }
    }
    return e;
}

var sun = new Body(0, 0, 0, 0, 0, 0, SOLAR_MASS);
var jupiter = new Body(
    4.84143144246472090,  -1.16032004402742839, -1.03622044471123109e-01,
    1.66007664274403694e-03 * DAYS_PER_YEAR, 7.69901118419740425e-03 * DAYS_PER_YEAR, -6.90460016972063023e-05 * DAYS_PER_YEAR,
    9.54791938424326609e-04 * SOLAR_MASS
);
var saturn = new Body(
    8.34336671824457987, 4.12479856412430479, -4.03523417114321381e-01,
    -2.76742510726862411e-03 * DAYS_PER_YEAR, 4.99852801234917238e-03 * DAYS_PER_YEAR, 2.30417297573763929e-05 * DAYS_PER_YEAR,
    2.85885980666130812e-04 * SOLAR_MASS
);

var bodies = [sun, jupiter, saturn];

for (var i = 0; i < 20000; i++) {
    advance(bodies, 0.01);
}
console.log(Math.round(energy(bodies) * 1000000) / 1000000);
