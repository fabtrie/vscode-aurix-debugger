"""Extracts the bit fields of the SCR registers from the TC3xx user manual part 2 (PDF).

Usage: python extract-scr-fields.py <infineon-aurix-tc3xx-part2-usermanual-en.pdf>

Needs PyMuPDF (fitz). Reads scr_registers.json (from extract-scr-registers.py) for the
manual's reset table numbers and writes scr_fields.json:

    {"<table>": [{"name", "msb", "lsb", "type", "title", "description", "enum": [[value, name, text]]}]}

The field tables of chapter 49 all share one layout: columns Field / Bits / Type / Description at
fixed x positions, enumerations as "<value><radix subscript> <bold name>, <text>". The parser works on
the text spans (PyMuPDF's table finder misses columns on some pages), from the "Field Bits Type
Description" header after "Reset Value: Table <n>" to the caption "Table <n> Reset Values of ...",
across page breaks. Reserved fields (named 0 or 1) are left out; templated fields such as
"PDm (m=0-1)" with bits "4*m+1:4*m" are expanded.
"""
import json
import os
import re
import sys

import fitz

HERE = os.path.dirname(os.path.abspath(__file__))
FIRST = re.compile(r"^49\.1\s+Features of the SCR\s*$", re.M)
LAST = re.compile(r"^49\.20\s+Single Pin DAP \(SPD\)\s*$", re.M)
MARK = re.compile(r"Reset Value:\s*Table (\d+)")
CAPTION = re.compile(r"^Table (\d+) Reset Values of")
TYPES = {"r", "w", "rw", "rh", "rwh", "rwp", "wh", "rhw", "rc", "rwc", "rwhp", "rX"}
# Column left edges of the field tables (points); a span belongs to the column it starts in.
COLUMNS = [(0, 110), (110, 172), (172, 216), (216, 600)]
TOP, BOTTOM = 60, 800  # page header and footer
SUBSCRIPT = 9.0  # radix subscripts (B, H) are set in 7.4 pt, text in 11 pt


def chapter_pages(doc):
    start = None
    for i in range(len(doc)):
        t = doc[i].get_text()
        if start is None and FIRST.search(t) and ". . ." not in t:
            start = i
        elif start is not None and LAST.search(t):
            return start, i
    sys.exit("chapter 49 not found")


def spans(doc, start, end):
    """All text spans of the chapter in reading order: (page, y, x, size, bold, text)."""
    out = []
    for p in range(start, end + 1):
        for b in doc[p].get_text("dict")["blocks"]:
            for line in b.get("lines", []):
                for s in line["spans"]:
                    x0, y0 = s["bbox"][0], s["bbox"][1]
                    if TOP < y0 < BOTTOM and s["text"].strip():
                        out.append((p, y0, x0, s["size"], "Bold" in s["font"], s["text"].replace("’", "'")))
    out.sort(key=lambda s: (s[0], round(s[1], 0), s[2]))
    return out


def lines_of(sp):
    """Groups spans into lines; subscripts join the line they hang from."""
    lines = []
    for s in sorted((s for s in sp if s[3] >= SUBSCRIPT), key=lambda s: (s[0], s[1], s[2])):
        if lines and lines[-1][0] == s[0] and abs(lines[-1][1] - s[1]) < 2:
            lines[-1][2].append(s)
        else:
            lines.append([s[0], s[1], [s]])
    for s in (s for s in sp if s[3] < SUBSCRIPT):
        host = [ln for ln in lines if ln[0] == s[0] and 0 <= s[1] - ln[1] < 8]
        if host:
            host[-1][2].append(s)
    for ln in lines:
        ln[2].sort(key=lambda s: s[2])
    return lines


def text(spans_):
    return re.sub(r"\s+", " ", "".join(s[5] for s in spans_)).strip()


def column(x):
    return next(i for i, (a, b) in enumerate(COLUMNS) if a <= x < b)


def table_regions(sp):
    """Yields (table number, spans of its field table without header rows)."""
    i, n = 0, len(sp)
    while i < n:
        m = MARK.search(sp[i][5])
        if not m and sp[i][5].strip().startswith("Reset Value:") and i + 1 < n:
            m = MARK.search(sp[i][5] + sp[i + 1][5])
        if not m:
            i += 1
            continue
        table = int(m.group(1))
        j = i + 1
        # Skip the bit diagram up to the first "Field" header.
        while j < n and not (sp[j][5].strip() == "Field" and sp[j][2] < 50 and sp[j][4]):
            if MARK.search(sp[j][5]) or CAPTION.match(sp[j][5].strip()):
                break
            j += 1
        body = []
        while j < n:
            t = sp[j][5].strip()
            if MARK.search(t) or (sp[j][2] < 50 and t.startswith("Table ") and sp[j][4]):
                break
            body.append(sp[j])
            j += 1
        # Header rows ("Field Bits Type Description", repeated on every page).
        heads = {(s[0], round(s[1])) for s in body if s[5].strip() == "Bits" and s[4]}
        body = [s for s in body if (s[0], round(s[1])) not in heads]
        yield table, body
        i = j


def rows_of(body):
    anchors = [s for s in body if column(s[2]) == 2 and s[3] >= SUBSCRIPT and s[5].strip() in TYPES]
    rows = []
    for k, a in enumerate(anchors):
        lo = (a[0], a[1] - 2)
        hi = (anchors[k + 1][0], anchors[k + 1][1] - 2) if k + 1 < len(anchors) else (1 << 30, 0)
        rows.append([s for s in body if lo <= (s[0], s[1]) < hi])
    return rows


VALUE = re.compile(r"^([0-9A-FX]+)\s*([BH])?$")


def parse_description(sp):
    """Title, text and enumeration of a Description cell."""
    lines = lines_of(sp)
    title, paras, enum = "", [], []
    for k, ln in enumerate(lines):
        segs = ln[2]
        t = text(segs)
        if k == 0 and all(s[4] for s in segs):
            title = t
            continue
        first = segs[0]
        # Enumeration line: "<digits><radix subscript> [<bold name>][, ]<text>".
        m = None
        if first[2] < 230 and len(segs) > 1 and segs[1][3] < SUBSCRIPT:
            m = re.fullmatch(r"([0-9A-FX]+)", first[5].strip()) and re.fullmatch(r"([BH])", segs[1][5].strip())
        if m:
            value = first[5].strip() + segs[1][5].strip()
            rest = segs[2:]
            while rest and not rest[0][5].strip():
                rest = rest[1:]
            name = ""
            if rest and rest[0][4]:
                name = rest[0][5].strip(" ,")
                rest = rest[1:]
            desc = text(rest).lstrip(", ").strip()
            if name or desc[:1].isupper() or desc[:1].isdigit():
                enum.append([value, name, desc])
                continue
            # Otherwise prose that wrapped right before a number ("... between 11B and 00B ...").
        if enum and first[2] > 235:
            enum[-1][2] = (enum[-1][2] + " " + t).strip()
        elif paras and (first[2] > 235 or not paras[-1].endswith((".", ":"))):
            paras[-1] += " " + t
        else:
            paras.append(t)
    return title, " ".join(paras).strip(), enum


def eval_bits(expr, var, v):
    e = expr.replace(" ", "")
    if var:
        e = re.sub(rf"(?<![A-Za-z]){var}(?![A-Za-z])", str(v), e)
    if not re.fullmatch(r"(?:[1-9][0-9]*|0)(?:[+\-*](?:[1-9][0-9]*|0))*", e):
        raise ValueError(expr)
    return eval(e)  # digits and + - * only


def fields_of(table, body, problems):
    out = []
    for row in rows_of(body):
        cols = [[s for s in row if column(s[2]) == c] for c in range(4)]
        name = text(lines_of(cols[0]) and [s for ln in lines_of(cols[0]) for s in ln[2]])
        bits = "".join(s[5] for s in sorted(cols[1], key=lambda s: (s[0], s[1], s[2]))).replace(" ", "")
        typ = text(cols[2])
        title, desc, enum = parse_description(cols[3])
        if re.fullmatch(r"[01]", name) or "reserved" in (name + " " + title).lower():
            continue
        m = re.fullmatch(r"(\S+?)\s*\(([a-z])\s*=\s*(\d+)\s*-\s*(\d+)\)", name)
        instances = [(name, None, 0)]
        if m:
            base, var, lo, hi = m.group(1), m.group(2), int(m.group(3)), int(m.group(4))
            instances = [(re.sub(var, str(v), base, count=1), var, v) for v in range(lo, hi + 1)]
        for nm, var, v in instances:
            try:
                msb, lsb = (bits.split(":") + [None])[:2]
                msb = eval_bits(msb, var, v)
                lsb = msb if lsb is None else eval_bits(lsb, var, v)
            except ValueError:
                problems.append(f"table {table}: field {name}: bits {bits!r}")
                continue
            if not re.fullmatch(r"[A-Z][A-Z0-9_]*", nm):
                problems.append(f"table {table}: odd field name {nm!r}")
            ttl = re.sub(rf"\b{var}\b", str(v), title) if var else title
            out.append({"name": nm, "msb": msb, "lsb": lsb, "type": typ, "title": ttl,
                        "description": desc, "enum": enum})
    return out


def main():
    doc = fitz.open(sys.argv[1])
    start, end = chapter_pages(doc)
    regs = json.load(open(os.path.join(HERE, "scr_registers.json")))["registers"]
    wanted = {r["manualTable"] for r in regs}
    problems, result = [], {}
    for table, body in table_regions(spans(doc, start, end)):
        if table in wanted:
            result[table] = fields_of(table, body, problems)
    for t in sorted(wanted):
        f = result.get(t)
        if not f:
            problems.append(f"table {t}: no fields")
            continue
        used = 0
        for x in f:
            mask = ((1 << (x["msb"] - x["lsb"] + 1)) - 1) << x["lsb"]
            if x["msb"] > 7 or x["lsb"] < 0 or x["lsb"] > x["msb"]:
                problems.append(f"table {t}: {x['name']} bits {x['msb']}:{x['lsb']}")
            elif used & mask:
                problems.append(f"table {t}: {x['name']} overlaps")
            used |= mask
    with open(os.path.join(HERE, "scr_fields.json"), "w", encoding="utf-8") as fh:
        json.dump({"$comment": "Generated by extract-scr-fields.py from the AURIX TC3xx user manual part 2, chapter 49.",
                   "tables": {str(k): v for k, v in sorted(result.items())}}, fh, indent=1, ensure_ascii=False)
    n = sum(len(v) for v in result.values())
    print(f"pages {start + 1}-{end + 1}: {len(result)} of {len(wanted)} tables, {n} fields")
    for p in problems:
        print("  " + p)


main()
