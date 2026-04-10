// SunSpider benchmark: 3d-cube
// 3D cube rotation using trigonometric functions

var Q = [];
var MTrans = [];
var MQube = [];
var I = [];
var DepthQube = 0;
var ViewDist = 0;
var MAXPTS = 4;

function DrawLine(From, To) {}
function CalcCross(V0, V1) { return [V0[1]*V1[2]-V0[2]*V1[1], V0[2]*V1[0]-V0[0]*V1[2], V0[0]*V1[1]-V0[1]*V1[0]]; }
function IsStand(Rone, Origin) { var NormX = CalcCross(Rone[1], Rone[2]); return ((IsPosit(NormX, [Rone[0][0]-Origin[0], Rone[0][1]-Origin[1], Rone[0][2]-Origin[2]])) ? 1 : 0); }
function IsPosit(NormX, P) { var DotProd = NormX[0]*P[0]+NormX[1]*P[1]+NormX[2]*P[2]; return (DotProd > 0); }

function MMulti(M1, M2) {
    var Res = [];
    Res[0] = [M1[0][0]*M2[0][0]+M1[0][1]*M2[1][0]+M1[0][2]*M2[2][0], M1[0][0]*M2[0][1]+M1[0][1]*M2[1][1]+M1[0][2]*M2[2][1], M1[0][0]*M2[0][2]+M1[0][1]*M2[1][2]+M1[0][2]*M2[2][2]];
    Res[1] = [M1[1][0]*M2[0][0]+M1[1][1]*M2[1][0]+M1[1][2]*M2[2][0], M1[1][0]*M2[0][1]+M1[1][1]*M2[1][1]+M1[1][2]*M2[2][1], M1[1][0]*M2[0][2]+M1[1][1]*M2[1][2]+M1[1][2]*M2[2][2]];
    Res[2] = [M1[2][0]*M2[0][0]+M1[2][1]*M2[1][0]+M1[2][2]*M2[2][0], M1[2][0]*M2[0][1]+M1[2][1]*M2[1][1]+M1[2][2]*M2[2][1], M1[2][0]*M2[0][2]+M1[2][1]*M2[1][2]+M1[2][2]*M2[2][2]];
    return Res;
}

function VMulti(M, V) {
    return [M[0][0]*V[0]+M[0][1]*V[1]+M[0][2]*V[2], M[1][0]*V[0]+M[1][1]*V[1]+M[1][2]*V[2], M[2][0]*V[0]+M[2][1]*V[1]+M[2][2]*V[2]];
}

function VMulti2(M, V) {
    return [M[0][0]*V[0]+M[0][1]*V[1]+M[0][2]*V[2]+M[0][3], M[1][0]*V[0]+M[1][1]*V[1]+M[1][2]*V[2]+M[1][3], M[2][0]*V[0]+M[2][1]*V[1]+M[2][2]*V[2]+M[2][3]];
}

function SetupCube() {
    Q[0] = [-1, 1, 1]; Q[1] = [1, 1, 1]; Q[2] = [1, -1, 1]; Q[3] = [-1, -1, 1];
    Q[4] = [-1, 1, -1]; Q[5] = [1, 1, -1]; Q[6] = [1, -1, -1]; Q[7] = [-1, -1, -1];
    MTrans = [[1, 0, 0, 0], [0, 1, 0, 0], [0, 0, 1, 0]];
    ViewDist = 6;
}

function RotateStuff(Rone) {
    var sinx = Math.sin(Lone[0]);
    var cosx = Math.cos(Lone[0]);
    var siny = Math.sin(Lone[1]);
    var cosy = Math.cos(Lone[1]);
    var sinz = Math.sin(Lone[2]);
    var cosz = Math.cos(Lone[2]);
    var MRotx = [[1, 0, 0], [0, cosx, -sinx], [0, sinx, cosx]];
    var MRoty = [[cosy, 0, siny], [0, 1, 0], [-siny, 0, cosy]];
    var MRotz = [[cosz, -sinz, 0], [sinz, cosz, 0], [0, 0, 1]];
    var result = MMulti(MMulti(MRotx, MRoty), MRotz);
    for (var i = 0; i < 8; i = i + 1) {
        Q[i] = VMulti(result, Rone[i]);
    }
}

SetupCube();
var Lone = [0.01227184630308513, 0.01640993178738, 0.016789620414914288];

for (var i = 0; i < 20000; i = i + 1) {
    var sinx = Math.sin(Lone[0]);
    var cosx = Math.cos(Lone[0]);
    var siny = Math.sin(Lone[1]);
    var cosy = Math.cos(Lone[1]);
    var sinz = Math.sin(Lone[2]);
    var cosz = Math.cos(Lone[2]);
    var MRotx = [[1, 0, 0], [0, cosx, -sinx], [0, sinx, cosx]];
    var MRoty = [[cosy, 0, siny], [0, 1, 0], [-siny, 0, cosy]];
    var MRotz = [[cosz, -sinz, 0], [sinz, cosz, 0], [0, 0, 1]];
    var M = MMulti(MMulti(MRotx, MRoty), MRotz);
    for (var j = 0; j < 8; j = j + 1) {
        Q[j] = VMulti(M, Q[j]);
    }
    Lone[0] = Lone[0] + 0.01;
    Lone[1] = Lone[1] + 0.02;
    Lone[2] = Lone[2] + 0.015;
}
console.log("done");
