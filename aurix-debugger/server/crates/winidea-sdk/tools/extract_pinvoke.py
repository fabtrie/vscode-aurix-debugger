# /// script
# requires-python = ">=3.10"
# dependencies = []
# ///
"""Extract every P/Invoke declaration from IConnectCSLib.dll into JSON.

The managed assembly that ships with the winIDEA C# SDK contains one
`pinvokeimpl` entry per native export, carrying the exact export name plus
return/parameter types. That is the authoritative type source for the Rust
bindings -- nothing here is inferred.

Input : ildasm /TEXT output of IConnectCSLib.dll
        + the DLL's real export list (dumpbin /EXPORTS isystemConnect.dll)
Output: JSON [{export, name, ret, params:[{type,name}], class, method}]

The export list is REQUIRED, not optional: **ildasm truncates string literals at
80 characters**, so long export names come out cut off -- and several distinct
SWIG overloads share an 80-char prefix, e.g. both of

    ..._CAddressController_getSourceLocationAtPathSource__SWIG_0___
    ..._CAddressController_getSourceLocationAtPathSource__SWIG_1___

appear in the IL as the same `..._getSourceLocationAtPathSource__SWI`. Binding
against those truncated names would fail to resolve at runtime. We therefore
take types from the IL but resolve every export name against the DLL's own
export table, which is authoritative and untruncated.

Run:
  ildasm /TEXT /NOBAR IConnectCSLib.dll > csil.txt
  dumpbin /EXPORTS isystemConnect.dll   > csexp.txt
  uv run --script extract_pinvoke.py csil.txt csexp.txt pinvoke.json
"""

from __future__ import annotations

import json
import re
import sys
from collections import Counter
from pathlib import Path

# .method public hidebysig static pinvokeimpl("isystemConnect" as "CSharp_..." winapi)
#         uint64  CDAQSample_getTime(valuetype ...HandleRef jarg1) cil managed preservesig
DECL_RE = re.compile(
    r'pinvokeimpl\(\s*"isystemConnect"'          # the native module
    r'(?:\s+as\s+"(?P<export>[^"]+)")?'          # optional explicit export name
    r'[^)]*\)'                                   # calling convention etc.
    r'\s*(?P<sig>.*?)'                           # return type + name + params
    r'\s*cil\s+managed',
    re.S,
)

# Split "<ret> <Name>(<params>)" -- the name is the last token before '('.
SIG_RE = re.compile(r'^(?P<ret>.+?)\s+(?P<name>[A-Za-z_][\w.]*)\s*\((?P<params>.*)\)\s*$', re.S)


def clean_type(t: str) -> str:
    """Normalise an IL type to a short canonical form."""
    t = " ".join(t.split())
    t = re.sub(r'\[mscorlib\]', '', t)
    t = re.sub(r'\bmarshal\(\s*[^)]*\)', '', t)
    t = re.sub(r'^\[in\]\s*|^\[out\]\s*', '', t)
    t = re.sub(r'\bmodopt\([^)]*\)', '', t)
    # HandleRef is SWIG's opaque object pointer.
    if "HandleRef" in t:
        return "handle"
    if t.startswith("valuetype "):
        t = t[len("valuetype "):]
    if t.startswith("class "):
        t = t[len("class "):]
    return " ".join(t.split())


def split_params(s: str) -> list[str]:
    """Split a parameter list on top-level commas (types may contain commas)."""
    out, depth, cur = [], 0, []
    for ch in s:
        if ch in "([<":
            depth += 1
        elif ch in ")]>":
            depth -= 1
        if ch == "," and depth == 0:
            out.append("".join(cur))
            cur = []
        else:
            cur.append(ch)
    if "".join(cur).strip():
        out.append("".join(cur))
    return [p.strip() for p in out if p.strip()]


def parse_param(p: str) -> dict:
    p = " ".join(p.split())
    m = re.match(r'^(?P<type>.+?)\s+(?P<name>[A-Za-z_]\w*)$', p)
    if m:
        return {"type": clean_type(m.group("type")), "name": m.group("name")}
    return {"type": clean_type(p), "name": ""}


def split_class_method(name: str) -> tuple[str, str]:
    """SWIG names members <Class>_<method>; free functions have no underscore.

    Ambiguity is unavoidable from the name alone (class names themselves may
    contain underscores), so this is a best-effort split used only for grouping
    and readability -- never for the ABI, which uses `export` verbatim.
    """
    if "_" not in name:
        return "", name
    cls, meth = name.split("_", 1)
    return cls, meth


EXPORT_RE = re.compile(r'^\s*\d+\s+[0-9A-Fa-f]+\s+[0-9A-Fa-f]{8}\s+(\S+)\s*$', re.M)


def load_exports(path: Path) -> list[str]:
    """Real, untruncated export names from `dumpbin /EXPORTS`."""
    return sorted(set(EXPORT_RE.findall(path.read_text(errors="replace"))))


def resolve_export(il_name: str, exports: set[str], by_prefix: dict) -> tuple[str | None, str]:
    """Map a possibly-80-char-truncated IL export name to the real one."""
    if il_name in exports:
        return il_name, "exact"
    cands = by_prefix.get(il_name, [])
    if len(cands) == 1:
        return cands[0], "prefix"
    if len(cands) > 1:
        return None, f"ambiguous({len(cands)})"
    return None, "missing"


def main() -> int:
    if len(sys.argv) < 4:
        print(__doc__)
        return 2
    text = Path(sys.argv[1]).read_text(errors="replace")
    exports = load_exports(Path(sys.argv[2]))
    out_path = Path(sys.argv[3])

    if not exports:
        print("!! no exports parsed -- is arg 2 really dumpbin /EXPORTS output?")
        return 1

    exp_set = set(exports)
    # Truncation is exactly 80 chars, so group real exports by that prefix.
    by_prefix: dict[str, list[str]] = {}
    for e in exports:
        by_prefix.setdefault(e[:80], []).append(e)

    fns, skipped = [], []
    for m in DECL_RE.finditer(text):
        sig = " ".join(m.group("sig").split())
        sm = SIG_RE.match(sig)
        if not sm:
            skipped.append(sig[:120])
            continue
        name = sm.group("name")
        il_export = m.group("export") or name
        cls, meth = split_class_method(name)
        fns.append({
            "il_export": il_export,
            "name": name,
            "class": cls,
            "method": meth,
            "ret": clean_type(sm.group("ret")),
            "params": [parse_param(p) for p in split_params(sm.group("params"))],
        })

    # --- resolve truncated export names against the DLL's export table -------
    # An ambiguous 80-char prefix means several SWIG overloads collided. They
    # differ only by the __SWIG_<n> suffix, and the IL preserves declaration
    # order, so assign the sorted real exports in the order the overloads appear.
    ambig: dict[str, list[dict]] = {}
    for f in fns:
        real, how = resolve_export(f["il_export"], exp_set, by_prefix)
        f["export"], f["_how"] = real, how
        if how.startswith("ambiguous"):
            ambig.setdefault(f["il_export"], []).append(f)

    for prefix, group in ambig.items():
        cands = by_prefix[prefix]
        if len(group) == len(cands):
            for f, real in zip(group, cands):
                f["export"], f["_how"] = real, "ordered"

    unresolved = [f for f in fns if not f["export"]]
    fns = [f for f in fns if f["export"]]
    for f in fns:
        f.pop("il_export", None)
        f.pop("_how", None)

    # Exports are unique in a DLL; duplicates mean resolution went wrong.
    seen = Counter(f["export"] for f in fns)
    dupes = [e for e, n in seen.items() if n > 1]

    fns.sort(key=lambda f: f["export"])
    out_path.write_text(json.dumps(fns, indent=1))

    print(f"exports in DLL : {len(exports)}")
    print(f"parsed         : {len(fns)} P/Invoke declarations")
    print(f"skipped        : {len(skipped)}")
    for s in skipped[:5]:
        print(f"    ? {s}")
    print(f"unresolved     : {len(unresolved)}")
    for u in unresolved[:5]:
        print(f"    ! {u['il_export']} ({u['_how']})")
    print(f"duplicate      : {len(dupes)}")
    for d in dupes[:5]:
        print(f"    ! {d}")
    covered = len({f["export"] for f in fns})
    print(f"coverage       : {covered}/{len(exports)} exports bound")

    print("\nreturn types:")
    for t, c in Counter(f["ret"] for f in fns).most_common(15):
        print(f"  {c:5d}  {t}")
    print("\nparam types:")
    pt = Counter(p["type"] for f in fns for p in f["params"])
    for t, c in pt.most_common(15):
        print(f"  {c:5d}  {t}")
    print(f"\nclasses  : {len({f['class'] for f in fns if f['class']})}")
    print(f"wrote    : {out_path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
