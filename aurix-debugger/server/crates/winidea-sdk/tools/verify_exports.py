# /// script
# requires-python = ">=3.10"
# dependencies = []
# ///
"""Verify every symbol the generated bindings resolve actually exists in the DLL.

`Sdk::load` resolves all ~6k symbols eagerly, so a single bad name makes the
whole load fail at runtime -- on a machine that has winIDEA, which is not
necessarily the machine that built the code. This check catches that at
development time instead, without needing the SDK installed.

It is the strongest verification available without a live winIDEA: it proves the
symbol NAMES are right. It cannot prove the SIGNATURES are right (nothing can,
short of calling them), though those come from the SDK's own P/Invoke metadata.

Run:
  uv run --script verify_exports.py ../src/generated/ffi.rs csexp.txt
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

# lib.get(b"CSharp_..._foo___\0")?;
BOUND_RE = re.compile(r'lib\.get\(b"([^"\\]+)\\0"\)')
# dumpbin /EXPORTS:  "    1    0 0005F960 CSharp_..."
EXPORT_RE = re.compile(r"^\s*\d+\s+[0-9A-Fa-f]+\s+[0-9A-Fa-f]{8}\s+(\S+)\s*$", re.M)


def main() -> int:
    if len(sys.argv) < 3:
        print(__doc__)
        return 2

    bound = set(BOUND_RE.findall(Path(sys.argv[1]).read_text(errors="replace")))
    exports = set(EXPORT_RE.findall(Path(sys.argv[2]).read_text(errors="replace")))

    if not bound:
        print("!! no bindings found -- did the generator output change shape?")
        return 1
    if not exports:
        print("!! no exports found -- is arg 2 really dumpbin /EXPORTS output?")
        return 1

    missing = sorted(bound - exports)
    unbound = sorted(exports - bound)

    print(f"bound in ffi.rs : {len(bound)}")
    print(f"exports in DLL  : {len(exports)}")
    print(f"bound but ABSENT: {len(missing)}   <-- these would fail Sdk::load")
    for m in missing[:20]:
        print(f"    ! {m}")
    print(f"exported, unbound: {len(unbound)}")
    for u in unbound[:20]:
        print(f"    . {u}")

    if missing:
        print("\nFAIL: generated bindings reference symbols the DLL does not export.")
        return 1
    print("\nOK: every bound symbol exists in the DLL.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
