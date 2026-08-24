#ifndef UTIL_HPP
#define UTIL_HPP

namespace gfx {

class Shape {
public:
    Shape();
    virtual ~Shape();
    virtual int area() const;
    int common();
private:
    int w_;
};

class Circle : public Shape {
public:
    Circle();
    ~Circle();
    int area() const;
    int radius() { return 42; }
};

} // namespace gfx

#endif
