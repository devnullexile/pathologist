#include <stddef.h>
#include "ops.hpp"

struct OpsImpl g_s = { NULL };

void Setup(void);
int Read(struct OpsImpl *s, unsigned char *out, unsigned len);

int main(void) {
    unsigned char buf[8];
    RegisterOps(&g_s);
    return Read(&g_s, buf, 8);
}
