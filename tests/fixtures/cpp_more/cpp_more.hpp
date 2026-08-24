#ifndef CPP_MORE_HPP
#define CPP_MORE_HPP

class Base {
public:
    Base(int v);
    int base_value() { return 1; }
};

class Widget {
public:
    int make();
};

template <class T>
class Box : public Base {
public:
    void put(T *item) { slot = item; }
    T *get() { return slot; }
private:
    T *slot;
};

class A {
public:
    virtual int fa() { return 1; }
};

class B {
public:
    virtual int fb() { return 2; }
};

class AB : public A, public B {
public:
    int fa() { return 10; }
    int fb() { return 20; }
};

struct Member {
    Member();
};

class D : public Base {
public:
    D(int v) : Base(v), m() {}
private:
    Member m;
};

class S {
public:
    static int Make();
};

#endif
