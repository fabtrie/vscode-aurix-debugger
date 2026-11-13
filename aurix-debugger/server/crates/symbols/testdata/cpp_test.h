// A class with a virtual function defined out of line (its key function): GCC writes the class
// out in full only in cpp_test.cpp, which has the vtable; cpp_test2.cpp only declares it.
namespace app {
class Widget {
public:
    virtual int tick();
    int count;
    short level;
    static int made;
};
} // namespace app

int widget_tick();
