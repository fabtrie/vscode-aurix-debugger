# Building

## Extension and debug server

From the repository root, to build the debug server and package the extension with it:

```bash
npm install
npm run package
```

This produces `vscode-aurix-tcf-debugger/aurix-debugger.vsix`. Building the server needs Rust and MinGW; see the [server README](../aurix-debugger/server/README.md#build).

In `vscode-aurix-tcf-debugger`, `npm run package` only compiles the TypeScript sources, copies the server and the OpenOCD scripts into `bin/` and packages the VSIX. The server is taken from:

- `aurix-dbg.exe`: the environment variable `AURIX_DBG_EXE`, else `aurix-debugger/server/target/release/` or `.../debug/`
- `aurix-hub.exe`: `AURIX_HUB_EXE`, else next to `aurix-dbg.exe`
- OpenOCD's scripts: `AURIX_OPENOCD_DIR`, else `aurix-debugger/server/target/openocd-tcl` (fetched by the server build)

## Bundled files (`vscode-aurix-tcf-debugger/bin/`)

| File | Content |
|---|---|
| `aurix-hub.exe` | The debugger hub (MCP, REST, sessions) |
| `aurix-dbg.exe` | The debug server of one board, started by the hub |
| `THIRD_PARTY_LICENSES.txt` | The license texts of the third-party code in `aurix-dbg.exe` |
| `openocd/` | OpenOCD's Tcl scripts, passed to the server as a search directory |
| `svd/` | SVD files for the Peripheral view |

All but `svd/` are not in the repository: `npm run copy-agent` (also run when packaging) copies them from the server build and generates the license file.

## SVD files of the HSM and the SCR

The `HSM_*` and `SCR_*` peripherals are merged into the TC3xx SVD files when the extension loads them.

- **HSM:** `npm run svd:hsm` generates `bin/svd/HSM_TC3xx.svd.gz` from `scripts/hsm-svd/hsm_tc3xx.json`. It also writes a readable copy, `scripts/hsm-svd/HSM_TC3xx.svd`, which works as a standalone Cortex-M3 SVD for the HSM and also contains `HSM_BRIDGE_HSMSIDE`, the bridge registers only the HSM can access.
- **SCR:** `scripts/scr-svd/extract-scr-registers.py` and `extract-scr-fields.py` extract the registers and bit fields from the TC3xx user manual (part 2, chapter 49) into `scr_registers.json` and `scr_fields.json`. `npm run svd:scr` then writes `bin/svd/SCR_TC3xx.svd.gz` and a readable copy, `scripts/scr-svd/SCR_TC3xx.svd`. `scripts/scr-svd/SCR_REGISTERS.md` lists the registers and what differs from the SCR firmware's own headers.

The SCR register addresses are those of the SCR debug monitor's virtual address map (TC3xx UM, Table 558): `0x40000 | page << 15 | (page register & 0x7F) << 8 | (SFR & 0x7F)`.
