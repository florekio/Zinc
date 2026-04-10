// SunSpider benchmark: access-binary-trees
// Binary tree creation and traversal

function TreeNode(left, right, item) {
    this.left = left;
    this.right = right;
    this.item = item;
}

function bottomUpTree(item, depth) {
    if (depth > 0) {
        return new TreeNode(
            bottomUpTree(2 * item - 1, depth - 1),
            bottomUpTree(2 * item, depth - 1),
            item
        );
    } else {
        return new TreeNode(null, null, item);
    }
}

function itemCheck(node) {
    if (node.left === null) {
        return node.item;
    }
    return node.item + itemCheck(node.left) - itemCheck(node.right);
}

var minDepth = 4;
var n = 12;
var maxDepth = (minDepth + 2 > n) ? minDepth + 2 : n;
var stretchDepth = maxDepth + 1;

var check = itemCheck(bottomUpTree(0, stretchDepth));

var longLivedTree = bottomUpTree(0, maxDepth);

for (var depth = minDepth; depth <= maxDepth; depth = depth + 2) {
    var iterations = 1;
    for (var i = 0; i < maxDepth - depth + minDepth; i = i + 1) {
        iterations = iterations * 2;
    }
    check = 0;
    for (var i = 1; i <= iterations; i = i + 1) {
        check = check + itemCheck(bottomUpTree(i, depth));
        check = check + itemCheck(bottomUpTree(-i, depth));
    }
}

itemCheck(longLivedTree);
console.log("done");
