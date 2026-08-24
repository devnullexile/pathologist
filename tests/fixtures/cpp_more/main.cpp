#include "cpp_more.hpp"

// Overload tie: same arity, different types — both targets emitted.
void tie(int x) { (void)x; }
void tie(long x) { (void)x; }

int Widget::make() { return 1; }

Base::Base(int v) { (void)v; }

Member::Member() {}

int S::Make() { return 3; }

static int sink_int(int v) { return v; }
static int sink_w(Widget *w) { return w->make(); }

int drive() {
    tie(0);
    tie(0L);

    Box<Widget> b;
    D d2(5);
    Widget gw;
    b.put(&gw);
    Widget *w = b.get();
    AB ab;
    A *pa = &ab;
    pa->fa();

    S::Make();
    return sink_int(S::Make()) + sink_w(w) + d2.base_value();
}
