# Third-Party Notices

The MIT license in [`LICENSE`](LICENSE) covers only the source code written for
this project (everything under `src/`, `scripts/`, `media/`, and the project
configuration files).

It does **not** cover the files shipped under `bin/`: `aurix-dbg.exe` is built
from the separate aurix-debugger project and keeps its own license, the SVD
files under `bin/svd/` are owned by Infineon Technologies AG and redistributed
under their own license, and the OpenOCD scripts under `bin/openocd/` keep
their own licenses (see below).

## OpenOCD Tcl scripts

`bin/openocd/tcl/` contains the configuration scripts (board, target, interface
and helper files) of OpenOCD, taken unmodified from the TriCore fork at
https://github.com/tricore-oss/openocd. They are licensed under the GNU General
Public License, version 2 or later, unless a file states otherwise in its SPDX
header. The license texts are included in `bin/openocd/COPYING` and
`bin/openocd/LICENSES/`; the exact source commit is pinned in the aurix-debugger
project (`server/crates/openocd-tcl/build.rs`).
