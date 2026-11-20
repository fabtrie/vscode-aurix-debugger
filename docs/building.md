# Building

## Extension and debug server

From the repository root:

```bash
npm install
npm run package
```

- Builds the debug server and packages the extension into `vscode-aurix-tcf-debugger/aurix-debugger.vsix`.
- The server build needs Rust and MinGW: see the [server README](../aurix-debugger/server/README.md#build).

`npm run package` in `vscode-aurix-tcf-debugger` skips the server build: it compiles the TypeScript sources, copies the server and the OpenOCD scripts into `bin/` and packages the VSIX. Sources:

| File | Environment variable | Default |
|---|---|---|
| `aurix-dbg.exe` | `AURIX_DBG_EXE` | `aurix-debugger/server/target/release/`, else `.../debug/` |
| `aurix-hub.exe` | `AURIX_HUB_EXE` | next to `aurix-dbg.exe` |
| OpenOCD scripts | `AURIX_OPENOCD_DIR` | `aurix-debugger/server/target/openocd-tcl` (fetched by the server build) |

## Bundled files (`vscode-aurix-tcf-debugger/bin/`)

| File | Content |
|---|---|
| `aurix-hub.exe` | Debugger hub (MCP, REST, sessions) |
| `aurix-dbg.exe` | Debug server of one board, started by the hub |
| `THIRD_PARTY_LICENSES.txt` | License texts of the third-party code in `aurix-dbg.exe` |
| `openocd/` | OpenOCD's Tcl scripts, passed to the server as a search directory |
| `svd/` | SVD files for the Peripheral view |

Only `svd/` is in the repository. `npm run copy-agent` (also run when packaging) copies the rest from the server build and generates the license file.

## SVD files of the HSM and the SCR

The `HSM_*` and `SCR_*` peripherals are merged into the TC3xx SVD files when the extension loads them.

- **HSM:** `npm run svd:hsm` generates `bin/svd/HSM_TC3xx.svd.gz` from `scripts/hsm-svd/hsm_tc3xx.json`.
  - Also writes a readable copy, `scripts/hsm-svd/HSM_TC3xx.svd`: a standalone Cortex-M3 SVD for the HSM, including `HSM_BRIDGE_HSMSIDE` (bridge registers only the HSM can access).
- **SCR:**
  1. `scripts/scr-svd/extract-scr-registers.py` and `extract-scr-fields.py` extract registers and bit fields from the TC3xx user manual (part 2, chapter 49) into `scr_registers.json` and `scr_fields.json`.
  2. `npm run svd:scr` writes `bin/svd/SCR_TC3xx.svd.gz` and a readable copy, `scripts/scr-svd/SCR_TC3xx.svd`.
  - `scripts/scr-svd/SCR_REGISTERS.md` lists the registers and where they differ from the SCR firmware's headers.
  - Register addresses are in the SCR debug monitor's virtual address map (TC3xx UM, Table 558): `0x40000 | page << 15 | (page register & 0x7F) << 8 | (SFR & 0x7F)`.
