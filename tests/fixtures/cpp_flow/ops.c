#include "ops.hpp"

static int RawImplRead(void *self, unsigned char *out, unsigned len) {
    (void)self;
    (void)out;
    return (int)len;
}

static struct Ops raw_ops = { RawImplRead };

int Read(struct OpsImpl *s, unsigned char *out, unsigned len) {
    if (s->impl->read == 0) {
        return -1;
    }
    return s->impl->read(s->impl, out, len);
}

struct Ops *RawOps(void) {
    return &raw_ops;
}
