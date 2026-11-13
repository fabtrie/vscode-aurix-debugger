// The second compilation unit of the C++ test program (see cpp_test.cpp).
#include "cpp_test.h"

app::Widget g_widget;

int widget_tick() {
    g_widget.level = 3;
    return g_widget.tick();
}
