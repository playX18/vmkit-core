/* Binary trees implemented in C using the BDWGC API. When running in VMKit build it, then link with `libvmkit.so` instead of `libgc.so`. */

#include <gc.h>
#include <stdio.h>
#include <time.h>

typedef struct Node {
    struct Node *left;
    struct Node *right;
} Node;

Node* leaf() {
    return GC_malloc(sizeof(Node));
}

Node* new_node(Node* left, Node* right) {
    Node* node = GC_malloc(sizeof(Node));
    node->left = left;
    node->right = right;
    return node;
}

int itemCheck(Node* node) {
    if (node->left == NULL) {
        return 1;
    }
    return 1 + itemCheck(node->left) + itemCheck(node->right);
}

Node* bottomUpTree(int depth) {
    if (depth > 0) {
        return new_node(bottomUpTree(depth - 1), bottomUpTree(depth - 1));
    }
    return leaf();
}

extern void* __data_start;
extern void* _end;

Node* longLivedTree;

int main() {
    printf("DATA START: %p\n", &__data_start);
    printf("DATA END: %p\n", &_end);
    GC_use_entire_heap = 1;
    GC_init();
    

    int maxDepth = 18;
    int stretchDepth = maxDepth + 1;
    int start = clock();
    Node* stretchTree = bottomUpTree(stretchDepth);
    printf("stretch tree of depth %d\n", stretchDepth);
    printf("time: %f\n", ((double)clock() - start) / CLOCKS_PER_SEC);

    longLivedTree = bottomUpTree(maxDepth);
    GC_gcollect();
    printf("long lived tree of depth %d\t check: %d\n", maxDepth, itemCheck(longLivedTree));
    for (int d = 4; d <= maxDepth; d += 2) {
        int iterations = 1 << (maxDepth - d + 4);
        int check = 0;
        for (int i = 0; i < iterations; i++) {
            Node* treeNode = bottomUpTree(d);
            check += itemCheck(treeNode);
        }
        printf("%d\t trees of depth %d\t check: %d\n", iterations, d, check);
    }

    printf("long lived tree of depth %d\t check: %d\n", maxDepth, itemCheck(longLivedTree));
    printf("time: %f\n", ((double)clock() - start) / CLOCKS_PER_SEC);

    return 0;
}
