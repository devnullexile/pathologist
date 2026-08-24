#include "util.hpp"

static int mark_i() { return 1; }
static int mark_d() { return 2; }

static int add(int a, int b) {
    mark_i();
    return a + b;
}

static double add(double a) {
    mark_d();
    return a * 2.0;
}

namespace util {
int tag() { return 7; }
} // namespace util

namespace {
int hidden() { return util::tag(); }
} // namespace

gfx::Shape::Shape() : w_(0) {}

gfx::Shape::~Shape() {}

int gfx::Shape::area() const { return w_; }

int gfx::Shape::common() { return 1; }

gfx::Circle::Circle() {}

gfx::Circle::~Circle() {}

int gfx::Circle::area() const { return 2; }

int main() {
    gfx::Circle *c = new gfx::Circle();
    gfx::Shape *s = c;

    int a = s->area();
    int rad = s->radius();
    int com = s->common();

    delete s;

    int r = add(2, 3);
    double d = add(1.5);
    int t = util::tag();
    int h = hidden();

    return a + rad + com + r + t + h + (int)d;
}
