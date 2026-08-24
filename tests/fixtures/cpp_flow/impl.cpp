#include "ops.hpp"

extern "C" void RegisterOps(struct OpsImpl *s);

static int MParcelImplRead(void *self, unsigned char *out, unsigned len) {
    (void)self;
    (void)out;
    return (int)len + 1;
}

static struct Ops parcel_ops = { MParcelImplRead };

extern "C" void RegisterOps(struct OpsImpl *s) {
    s->impl = &parcel_ops;
}
