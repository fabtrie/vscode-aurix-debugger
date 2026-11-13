// C++ test program for the symbols crate: namespaces, classes, out-of-line members,
// constructors and destructors, static members, overloads, enum class, templates, references.
//
// Built with the TriCore GCC 11 of AURIX Development Studio (in this directory):
//   tricore-elf-g++ -mcpu=tc39xx -g -O0 -fno-exceptions -fno-rtti -nostartfiles -nostdlib
//       -fdebug-prefix-map=$PWD=. -T cpp_test.ld -o cpp_test_O0.elf cpp_test.cpp
// and the same with -O2 into cpp_test_O2.elf. The program is never run.

namespace app {
namespace motor {

struct Base {
    int id;
    virtual ~Base() {}
    virtual int run() { return id; }
};

class Controller : public Base {
public:
    Controller(int s) : speed(s), limit(100) { id = 7; }
    int run() override { return step(3); }
    int step(int n);
    static int instances;
    int speed;
    int &ref() { return speed; }

private:
    int limit;
};

int Controller::instances = 0;

int Controller::step(int n) {
    int acc = 0;
    for (int i = 0; i < n; ++i)
        acc += speed + i;
    if (acc > limit)
        acc = limit;
    return acc;
}

enum class Mode : unsigned char { Off, Slow, Fast };
Mode g_mode = Mode::Slow;
Controller g_ctrl(42);
int counter;

union Word {
    unsigned int u;
    unsigned char b[4];
};
Word g_word = {0x11223344u};

} // namespace motor

template <typename T, int N> struct Ring {
    T buf[N];
    int head;
    void push(T v) { buf[head++ % N] = v; }
};
Ring<short, 4> g_ring;

} // namespace app

namespace {
int hidden = 9;
}

static int file_local = 5;
int plain_global = 3;

extern "C" int c_func(int x) { return x + file_local + hidden; }
int overloaded(int a) { return a + 1; }
int overloaded(float f) { return (int)f; }

int add_ref(int &r, const int &k) {
    r += k;
    return r;
}

int main() {
    app::motor::Controller *p = &app::motor::g_ctrl;
    int &r = p->ref();
    r += 1;
    app::motor::counter += p->run() + overloaded(2) + overloaded(2.0f) + c_func(1);
    app::motor::counter += add_ref(app::motor::counter, plain_global);
    app::g_ring.push(5);
    app::motor::Controller::instances++;
    while (1) {
        app::motor::counter++;
    }
}

// Runtime support a hosted toolchain would bring.
void *__dso_handle = 0;
extern "C" int __cxa_atexit(void (*)(void *), void *, void *) { return 0; }
extern "C" void __cxa_pure_virtual() {
    while (1) {
    }
}
void operator delete(void *, unsigned long) {}
