"""Builds the register list of the TC3xx standby controller (SCR, XC800) from the TC3xx user's manual.

Input: the text of "AURIX TC3xx User's Manual, Part 2" (V2.0.0, 2021-02), chapter 49 "SCR", as
`pdftotext -layout infineon-aurix-tc3xx-part2-usermanual-en.pdf um2.txt` writes it.

    python extract-scr-registers.py um2.txt

Writes scr_registers.json (the list, input for an SVD) and SCR_REGISTERS.md (for reading) next to
this script.

Every register description in chapter 49 starts with a header:

    SCU_CMCON                       (0FAH)                 Reset Value: Table 571
    Clock Control Register
    RMAP: 0, PAGE: SCU_PAGE=1

and its reset values are in the table it names ("LVD Reset XXXX X1X0B", "Generated Reset 00H").
Registers described in several sections are listed once; templates (`P0n_IOCRk`, `RTC_CNTn`,
`T2CCU_CCxL`, `WCAN_DATAn_CTRL`) are expanded. The CPU registers the debugger shows as core
registers (ACC, B, SP, PSW, DPL, DPH) are left out. The debug monitor's own registers (MMCR, ...)
have no address in the public manual and are not described there.
"""

import json
import os
import re
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
CORE = {"ACC", "B", "SP", "PSW", "DPL", "DPH"}

# Module of a register by its name; description of each module.
MODULES = [
    ("SYSTEM", ("SYSCON0", "PASSWD", "XADDRH"), "System control: SFR map (RMAP), bit protection, XRAM address extension"),
    ("CPU", ("EO", "PCON"), "XC800 CPU control: extended operation (TRAP enable, DPTR select) and power control"),
    ("INT", ("IEN0", "IEN1", "IP", "IPH", "IP1", "IPH1"), "Interrupt enable and priority"),
    ("SCU", ("SCU_", "DBG_MODSUSP", "NMICON", "NMISR", "EXICON", "IRCON", "SCRINTEXCHG", "TCINTEXCHG", "MODPISEL"),
     "System control unit: SFR paging, clocks, resets, power management, NMI, external interrupts, TriCore data exchange, input selection"),
    ("WDT", ("WDT_",), "Watchdog timer"),
    ("PORT", ("IO_PAGE", "P0"), "General purpose I/O ports SCR_P00 (P33.0-P33.7) and SCR_P01 (P34.1, P33.8-P33.15)"),
    ("RTC", ("RTC_",), "Real-time clock"),
    ("T01", ("T01_",), "Timer 0 and timer 1"),
    ("T2CCU", ("T2_", "T2CCU_"), "Timer 2 and capture/compare unit"),
    ("UART", ("UART_",), "Universal asynchronous receiver/transmitter (with LIN support)"),
    ("SSC", ("SSC_",), "Synchronous serial channel"),
    ("ADCOMP", ("ADCOMP_",), "ADC comparator unit"),
    ("WCAN", ("WCAN_",), "Wake-up CAN filter"),
]

# Page registers: (name, SFR address).
PAGE_REGS = {"SCU_PAGE": 0xF1, "IO_PAGE": 0x8F, "T2CCU_PAGE": 0xC7, "WCAN_PAGE": 0xB8}


def module_of(name):
    for mod, prefixes, _ in MODULES:
        for p in prefixes:
            if name == p or (p.endswith("_") or p in ("EXICON", "IRCON", "MODPISEL", "P0")) and name.startswith(p):
                return mod
    raise ValueError(f"no module for {name}")


def parse_headers(lines):
    start = next(i for i, l in enumerate(lines) if re.match(r"^\f?49\.1 Features of the SCR\s*$", l))
    end = next(i for i, l in enumerate(lines) if i > start and re.match(r"^\f?49\.20 Single Pin DAP \(SPD\)\s*$", l))
    addr_re = re.compile(r"\((0[0-9A-F]+H(?:[^()]|\([^()]*\))*)\)")
    out = []
    for i in range(start, end):
        mm = re.match(r"^\s*RMAP:\s*([^,]+),\s*PAGE:\s*(.+?)\s*$", lines[i].replace("\f", ""))
        if not mm:
            continue
        h = next(k for k in range(i - 1, i - 7, -1) if "Reset Value" in lines[k])
        hl = lines[h].replace("\f", "").strip()
        name = hl.split()[0]
        idx = re.match(r"^\S+\s+\(([a-z]=[^)]*)\)", hl)
        block = " ".join(lines[k].replace("\f", "").strip() for k in range(h, i))
        addr = addr_re.search(block).group(1)
        table = re.search(r"Reset Value:\s*Table (\d+)", hl).group(1)
        long = None
        for k in range(h + 1, i):
            t = addr_re.sub("", lines[k]).strip()
            t = re.sub(r"^\d+\s+", "", t)  # a bit number of the register diagram run into the line
            if t:
                long = t
                break
        out.append({"name": name, "index": idx.group(1) if idx else None, "addr": addr, "table": table,
                    "long": long, "rmap": mm.group(1).strip(), "page": mm.group(2).split()[0]})
    return out


def parse_resets(lines):
    res = {}
    for i, l in enumerate(lines):
        m = re.match(r"^\f?Table (\d+)\s+Reset Values? of ", l.strip())
        if not m:
            continue
        rows = {}
        for k in range(i + 1, i + 14):
            mm = re.match(r"^\s*(LVD|Generated|Application|Power-on) Reset\s+([0-9A-FX\-]{2}H|[01X\-]{4}\s?[01X\-]{4}B)", lines[k])
            if mm:
                rows[mm.group(1)] = mm.group(2).replace(" ", "")
        res[m.group(1)] = rows
    return res


def bits(v):
    """'00H', 'X1H' or 'XXXX X1X0B' -> (value, mask of defined bits, text). X: undefined, -: unchanged."""
    if v.endswith("H"):
        digits = v[:-1]
        value = int("".join("0" if c in "X-" else c for c in digits), 16)
        mask = int("".join("0" if c in "X-" else "F" for c in digits), 16)
        return value, mask, v
    b = v[:-1]
    value = int("".join("1" if c == "1" else "0" for c in b), 2)
    mask = int("".join("0" if c in "X-" else "1" for c in b), 2)
    return value, mask, v


def expand(h):
    """The registers a header stands for: (name, SFR address, long name)."""
    expr = h["addr"].replace("H", "")
    base = expr
    if not h["index"]:
        return [(h["name"], int(base, 16), h["long"])]
    ranges = {}
    for part in h["index"].split(";"):
        var, rng = part.split("=")
        a, b = rng.split("-")
        ranges[var.strip()] = range(int(a), int(b) + 1)
    out = []
    names = list(ranges)
    def walk(env, rest):
        if not rest:
            # Address: hex base plus an expression of the index variables.
            e = re.sub(r"^0([0-9A-F]+)", lambda m: str(int(m.group(1), 16)), expr)
            addr = eval(e, {}, env)  # noqa: S307 - our own manual text, digits and index letters only
            n = h["name"]
            long = h["long"]
            for var, val in env.items():
                n = n.replace(f"P0{var}", f"P0{val}") if var == "n" and n.startswith("P0") else n
                n = re.sub(rf"(?<=[A-Z]){var}(?=_|$|[A-Z])", str(val), n) if not (var == "n" and n.startswith("P0")) else n
                long = re.sub(rf"\b{var}\b", str(val), long).replace(f"P0{var}", f"P0{val}")
            out.append((n, addr, long))
            return
        v = rest[0]
        for val in ranges[v]:
            walk({**env, v: val}, rest[1:])
    walk({}, names)
    return out


def main():
    lines = open(sys.argv[1], encoding="utf-8", errors="replace").read().split("\n")
    headers = parse_headers(lines)
    resets = parse_resets(lines)
    regs, seen = [], set()
    for h in headers:
        for name, addr, long in expand(h):
            if name in CORE or name in seen:
                continue
            seen.add(name)
            page_reg, page = None, None
            if "=" in h["page"]:
                page_reg, page = h["page"].split("=")
                page = int(page)
            rmap = None if h["rmap"] == "X" else int(h["rmap"])
            r = resets.get(h["table"], {})
            lvd = bits(r["LVD"]) if "LVD" in r else None
            gen = bits(r["Generated"]) if "Generated" in r else None
            # SCR virtual address (TC3xx UM Table 558): bit 18, page 17:15, page register 14:8, RMAP 7, SFR 6:0.
            vaddr = 0x40000 | ((page or 0) << 15) | ((PAGE_REGS[page_reg] & 0x7F) << 8 if page_reg else 0) | ((rmap or 0) << 7) | (addr & 0x7F)
            regs.append({
                "module": module_of(name), "name": name, "description": long, "sfr": f"0x{addr:02X}",
                "rmap": rmap, "pageRegister": page_reg, "page": page, "virtualAddress": f"0x{vaddr:05X}",
                "resetLvd": lvd[2] if lvd else None, "reset": f"0x{lvd[0]:02X}" if lvd else None, "resetMask": f"0x{lvd[1]:02X}" if lvd else None,
                "resetGenerated": gen[2] if gen else None, "manualTable": int(h["table"]),
            })
    order = [m[0] for m in MODULES]
    regs.sort(key=lambda r: (order.index(r["module"]), r["pageRegister"] or "", r["page"] or 0, int(r["sfr"], 16), r["name"]))
    doc = {
        "$comment": [
            "Registers of the AURIX TC3xx standby controller (SCR, XC800), without the CPU core registers (ACC, B, SP, PSW, DPL, DPH).",
            "Source: AURIX TC3xx User's Manual Part 2, V2.0.0 (2021-02), chapter 49; generated by extract-scr-registers.py.",
            "sfr: SFR address (0x80-0xFF). rmap: the SYSCON0.RMAP value the register needs (null: any). pageRegister/page: the module page register and the PAGE value selecting the register (null: not paged).",
            "virtualAddress: the SCR virtual address of TC3xx UM Table 558 (bit 18 set, page in bits 17:15, page register address bits 6:0 in bits 14:8, RMAP in bit 7, SFR address bits 6:0 in bits 6:0).",
            "reset: value after an LVD (power-on) reset; resetMask: its defined bits (X in the manual: undefined). resetGenerated: after a generated reset (watchdog, soft reset; '-': unchanged).",
            "EO, PCON, IEN0, IP, IPH: the manual's reset tables show '-' in the LVD row and 'X' in the generated-reset row (swapped); both are undefined bits here.",
            "Not listed: the debug monitor's registers (MMCR, MMCR2, MMSR, MMBPCR, MMICR, MMDR, HWBPSR, HWBPDR, MMWR1, MMWR2): no addresses in the public manual (OCDS details are under NDA).",
        ],
        "modules": [{"name": m, "description": d} for m, _, d in MODULES],
        "registers": regs,
    }
    with open(os.path.join(HERE, "scr_registers.json"), "w", encoding="utf-8", newline="\n") as f:
        json.dump(doc, f, indent=1)
        f.write("\n")
    md = ["# SCR registers (TC3xx standby controller, XC800)", "",
          "Generated by `extract-scr-registers.py` from the AURIX TC3xx User's Manual Part 2 (V2.0.0, chapter 49); the data is in `scr_registers.json`.",
          "Without the CPU core registers ACC, B, SP, PSW, DPL, DPH. RMAP `any`: the register is in both SFR maps. Reset: after an LVD (power-on) reset, X = undefined.", ""]
    for m, _, d in MODULES:
        rs = [r for r in regs if r["module"] == m]
        md += [f"## {m}: {d} ({len(rs)})", "", "| Register | Description | SFR | RMAP | Page | Virtual address | Reset |", "|---|---|---|---|---|---|---|"]
        for r in rs:
            page = f"{r['pageRegister']}={r['page']}" if r["pageRegister"] else ""
            rmap = "any" if r["rmap"] is None else str(r["rmap"])
            md.append(f"| {r['name']} | {r['description']} | {r['sfr']} | {rmap} | {page} | {r['virtualAddress']} | {r['resetLvd'] or ''} |")
        md.append("")
    md += ["## Not listed", "",
           "- The debug monitor's registers (MMCR, MMCR2, MMSR, MMBPCR, MMICR, MMDR, HWBPSR, HWBPDR, MMWR1, MMWR2): the public manual names them in chapter 49.5.2 but gives no addresses (OCDS details are under NDA).",
           "- In the SCR firmware's `xc800SfrAddr.h` but not in the manual: SCR_P00_PDR1 (0x98), SCR_P00_PDR3 (0xA8), SCR_MOD_PAGE (0x00).", "",
           "## Differences found", "",
           "- The firmware's `xc800SfrAddr.h` puts SCR_SYSCON0 at 0x00 and SCR_P00_PDR2 at 0xA0; the manual has SYSCON0 at 0x88 and P00_PDR2 at 0x91 (IO_PAGE=2). Its other 61 register addresses match.",
           "- For EO, PCON, IEN0, IP and IPH the manual's reset tables have the two rows swapped: the LVD (power-on) row shows `-` (unchanged) and the generated-reset row `X`. Both mean \"not defined by this reset\"; the reset value and mask here are the same either way.", ""]
    with open(os.path.join(HERE, "SCR_REGISTERS.md"), "w", encoding="utf-8", newline="\n") as f:
        f.write("\n".join(md))
    print(f"{len(regs)} registers")


if __name__ == "__main__":
    main()
