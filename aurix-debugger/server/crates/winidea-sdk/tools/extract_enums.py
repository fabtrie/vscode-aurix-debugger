# /// script
# requires-python = ">=3.10"
# dependencies = []
# ///
"""Extract every enum constant from IConnectCSLib.dll's IL into JSON.

Enum discriminants are the one place a wrapper can be silently wrong: passing
the wrong integer configures the wrong sampling rate or the wrong shutdown
policy, and nothing fails loudly. The values are literals in the managed
assembly, so there is no reason to guess them.

Emits `enums.json`: {"CDAQController/EDAQSamplingFlags": {"daqSample1ms": 1, ...}}

Run:
  uv run --script extract_enums.py csil.txt enums.json
"""

from __future__ import annotations

import json
import re
import sys
from pathlib import Path

# .field public static literal valuetype si.isystem.iconnect.IConnect/EDetachFlags
#        dfCloseServerIfLastClient = int32(0x00000001)
FIELD_RE = re.compile(
    r"\.field\s+public\s+static\s+literal\s+valuetype\s+"
    r"(?P<type>[\w.]+(?:/\w+)*)\s+"
    r"(?P<name>\w+)\s*=\s*"
    r"(?P<kind>int8|int16|int32|int64|uint8|uint16|uint32|uint64)"
    r"\(\s*(?P<value>-?(?:0x[0-9A-Fa-f]+|\d+))\s*\)"
)


def short_type(t: str) -> str:
    """`si.isystem.iconnect.IConnect/EDetachFlags` -> `IConnect/EDetachFlags`."""
    return t.rsplit(".", 1)[-1] if "/" not in t else t.split(".")[-1]


def main() -> int:
    if len(sys.argv) < 3:
        print(__doc__)
        return 2

    text = Path(sys.argv[1]).read_text(errors="replace")
    out = Path(sys.argv[2])

    enums: dict[str, dict[str, int]] = {}
    for m in FIELD_RE.finditer(text):
        raw = m.group("value")
        val = int(raw, 16) if raw.lower().startswith(("0x", "-0x")) else int(raw)
        enums.setdefault(short_type(m.group("type")), {})[m.group("name")] = val

    out.write_text(json.dumps(enums, indent=1, sort_keys=True))

    total = sum(len(v) for v in enums.values())
    print(f"enum types : {len(enums)}")
    print(f"constants  : {total}")
    print(f"wrote      : {out}")

    # Show the ones the safe wrappers currently depend on, so a mismatch after
    # an SDK update is obvious in the diff.
    for want in ("CDAQController/EDAQSamplingFlags", "IConnect/EDetachFlags"):
        for key, vals in enums.items():
            if key.endswith(want.split("/")[-1]):
                print(f"\n{key}:")
                for n, v in sorted(vals.items(), key=lambda kv: kv[1]):
                    print(f"   {v:#010x}  {n}")
                break
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
