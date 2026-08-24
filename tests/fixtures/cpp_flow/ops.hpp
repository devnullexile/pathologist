#ifndef OPS_HPP
#define OPS_HPP

// C-compatible interface: both the C impl (ops.c) and the C++ impl
// (impl.cpp) register into the same ops table, mirroring the HDF
// sbuf pattern where hdf_sbuf_impl_hipc.cpp extends a C framework.

struct Ops {
    int (*read)(void *self, unsigned char *out, unsigned len);
};

struct OpsImpl {
    struct Ops *impl;
};

#endif
