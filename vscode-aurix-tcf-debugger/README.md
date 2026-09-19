# AURIX&trade; Debugger for VS Code

> **Disclaimer:** This is a personal project, built and maintained by one developer in my own time. It is not an official Infineon product and has nothing to do with Infineon Technologies AG. It comes with no warranty and no promise of support or future updates. Use it at your own risk.

The extension uses `aurix-dbg`, a debug server that reaches the target through Infineon's `tas_server` and runs the OpenOCD configuration scripts. `aurix-dbg.exe` is bundled with the extension; the setting `aurix-debugger.agentPath` or the launch attribute `agentPath` point to a different build.

VS Code extension for debugging Infineon AURIX&trade; devices. It supports all AURIX&trade; TC2x, TC3x, and TC4x microcontrollers, including their TriCore, PPU, and CDSP cores.

## Features

- Multi-core debugging
- Line breakpoints and instruction/address breakpoints
- Step in/out, step over, continue/pause, restart, terminate
- Peripheral view (read and write SVD registers)
- Reset and halt at first breakpoint
- Call stack and variables
- Live watch of global variable
- Disassembly view
- Instruction stepping mode 
- Memory Inspector (read and write memeory)
- Flashing through aurix-dbg (`enableFlashing`, Flash Target toolbar button)
- Custom Tcl debugger actions and `monitor` commands in the Debug Console
- Debug the PPU and CDSP cores

## Requirements

- Visual Studio Code 1.79.0 or higher
- Infineon `tas_server` (TAS) and a miniWiggler/DAP probe
- OpenOCD configuration scripts for the board (e.g. the OpenOCD `tcl` directory)
- Peripheral Inspector extension (installed automatically as a dependency)
- Memory Inspector extension (installed automatically as a dependency)

## Backend binaries (`bin/`)

- `aurix-dbg.exe` — the debug server. Not in the repository: `npm run copy-agent` (also run when packaging) copies it from the `aurix-debugger` build.
- `svd/` — SVD files for the supported devices

## Build

```bash
npm install
npm run package
```

`npm run package` compiles the TypeScript sources, copies the static assets and `aurix-dbg.exe`, and produces `aurix-debugger.vsix` in the project root.

`aurix-dbg.exe` is taken from the environment variable `AURIX_DBG_EXE` if set, otherwise from `../aurix-debugger/server/target/release/` or `../aurix-debugger/server/target/debug/`.

## Installation

### Install from VSIX

1. Build the project (see above) or download the `.vsix` from [Releases](https://github.com/ZeZhan/vscode-aurix-tcf-debugger/releases)
2. Open the Extensions view by selecting the Extensions icon in the Activity Bar, or use the (`Ctrl+Shift+X`) keyboard shortcut.
3. Click `...` and choose "Install from VSIX..."
4. Select the `.vsix` file
5. Open Command Palette and run `Developer: Reload Window`

## Usage

### Launch configuration

Create `.vscode/launch.json`:

```json
{
  "version": "0.2.0",
  "configurations": [
    {
      "type": "aurixdbg",
      "request": "launch",
      "name": "AURIX Debug",
      "host": "127.0.0.1",
      "port": 1534,
      "autoStartAgent": true,
      "searchDirs": ["${workspaceFolder}/tcl"],
      "configFiles": ["board/infineon/kit_a2g_tc377tx_3v3_trb.cfg"],
      "symbolFiles": ["${workspaceFolder}/temp/TC375_CE/TriCore Debug (GCC)/TC375_CE.elf"],
      "flashFiles": ["${workspaceFolder}/temp/TC375_CE/TriCore Debug (GCC)/TC375_CE.hex"],
      "device": "TC37x",
      "workspace": "${workspaceFolder}\\temp\\TC375_CE"
    }
  ]
}
```

- `searchDirs`, `configFiles`, `configCommands` are passed to aurix-dbg as `-s`, `-f`, `-c`; `additionalAgentOptions` is appended as-is.
- `autoStartAgent` starts the bundled `aurix-dbg.exe` (or `agentPath`); without it, the extension connects to an aurix-dbg already listening on `host:port`.
- `symbolFiles` lists the ELF files with debug information: the application and, if needed, bootloader, HSM or other images. A file applies to all cores of its architecture unless it has a `cores` filter.
- `enableFlashing` programs `flashFiles` before the session; the Flash Target toolbar button uses the same list. `verifyFlash` (default true) reads the programmed sectors back.
- After flashing and with the Reset button, the target is reset with `resetType` (default `reset`). With `resetSuspend: true` it halts at the reset vector; otherwise it runs, and the toolbar buttons pause all cores afterwards.
- `device` selects the SVD file bundled with the extension for the Peripheral View: `TC22x`, `TC23x`, `TC26x`, `TC27x`, `TC29x`, `TC33x`, `TC33xEXT`, `TC35x`, `TC36x`, `TC37x`, `TC37xEXT`, `TC38x`, `TC39x`, `TC4Dx`.

### Watch global variables

Variables and expressions can be watched in the Run and Debug view's **WATCH** section.

![add-to-watch](media/add-to-watch.gif)

### Live Watch View

The Live Watch view periodically refreshes selected global variables. Use the view's Settings button to configure the refresh interval (the minimum enforced value is 200 ms).

Once configured, the active interval appears in the view header (highlighted in orange). Resume the target to run the cores and observe the selected global variables update automatically.

- Entries look like the WATCH section: `name: value` in the theme's debug colors, with changed values highlighted. Structs and arrays show a short preview (`{a: 1, b: 2, …}`) and can be expanded. Pointers can be expanded to show the value they point to.
- Double-click a value (or press Enter) to change it. Double-click the name (or press F2) to edit the expression, e.g. to fix a typo. The context menu also copies the value or the expression.
- Right-click a register or field in the **XPeripherals** view and choose **Add to Live Watch**. Registers show as hex and can be set. Fields show their bits and are read-only; change them in the XPeripherals view.
- **Pause Updates** on an entry moves it to the **Paused** group, which is not refreshed while the target runs. **Resume Updates** moves it back.
- Paused entries still update like the WATCH section: when the target stops and when you press Refresh.
- The WATCH section only evaluates while the target is stopped. Use the Live Watch view to follow values while the target runs.

![live-watch](media/live-watch.gif)

### Peripheral View

This Peripheral View depends on the extension `mcu-debug.peripheral-viewer`. The register definitions come from the SVD file bundled for the `device` set in the launch configuration.

![peripheral-view](media/peripheral-view.gif)

### Memory Inspector

The extension depends on `eclipse-cdt.memory-inspector`. Use the Command Palette and run
`Memory Inspector: Show Memory Inspector` to open the view, then enter an
address or expression once the target is paused.

![memory-view](media/memory-view.gif)

### Disassembly View & Instruction Stepping Mode

The disassembly view opens automatically side-by-side with the source code and stays synchronized with the currently selected frame. The toolbar includes an `Enable Instruction Stepping` button, which allows stepping through the program's disassembly instructions. 

![enable-instruction-stepping](media/enable-instruction-stepping.gif)

**Note:** If the `Enable Instruction Stepping` icon is grayed out, it indicates that instruction stepping is already enabled. To switch back to source code stepping, click the icon `Disable Instruction Stepping` again to disable instruction stepping.

![set-instruction-bp](media/set-instruction-bp.gif)

Instruction breakpoints can be set in the same way as in the source code.

If the disassembly view is closed, use the Command Palette and run `Open Disassembly View` to open the view

The setting `aurix-debugger.disassembly.opcodeBytes` places the opcode bytes of each instruction `left` (between address and instruction, default), `right` (after the instruction) or `hidden`. A change applies to instructions loaded after the next stop.

To show any other code location while the target is halted, click the search icon in the disassembly view's title bar (or press `Ctrl+G` in the view) and pick a function or enter an address (`0x80001234`) or `symbol+offset`. VS Code's disassembly view can only follow the selected stack frame, so the target appears as a `[Disassembly] ...` frame at the bottom of the call stack; it disappears when the target resumes.

### PPU / CDSP Cores

In addition to the TriCore cores, the debugger supports the cores: **PPU** and **CDSP**. Assign an ELF file to each core in the launch configuration:

- `ppuElfPath` — a single ELF file for the PPU
- `cdspElfPaths` — up to 12 ELF files (CDSP0 – CDSP11)

The assigned ELF files provide the debug symbols used for source-level debugging on the corresponding core.

### Debugger Actions

The launch configuration can define custom Tcl commands in `actions`. They are offered by the `Debugger Actions` toolbar button, which only appears when `actions` is set. An entry with `items` opens a submenu, an entry with `tcl` runs its command(s) in order on the debug server. The output is written to the Debug Console.

```jsonc
"actions": [
  {
    "label": "Mass Erase",
    "icon": "trash",
    "items": [
      {
        "id": "erasePflash",
        "label": "Erase PFlash",
        "tcl": "<tcl command for pflash mass erase>",
        "confirm": "Erase the complete PFlash?",
        "resetAfter": true
      },
      {
        "id": "eraseDflash",
        "label": "Erase DFlash",
        "tcl": ["reset init", "flash erase_sector dfls0 0 last"],
        "confirm": "Erase the complete DFlash?",
        "resetAfter": true
      }
    ]
  },
  {
    "label": "Read Device ID",
    "icon": "info",
    "tcl": "<tcl command>",
    "showOutput": true
  }
]
```

| Property | Description |
|---|---|
| `label` | Name shown in the list (required) |
| `description`, `detail`, `icon` | Additional text and codicon name |
| `items` | Submenu entries (same format) |
| `tcl` | Tcl command or list of commands; stops at the first error |
| `confirm` | Confirmation text shown before running |
| `resetAfter` | Reset the target afterwards like the Reset button |
| `showOutput` | Show the Tcl output in a notification |
| `timeout` | Timeout per command in seconds (default 900) |
| `id` | Lets you run the action from a keybinding |

The action is responsible for the target state it needs (e.g. halting the cores first). To run an action from a keybinding:

```json
{ "key": "ctrl+alt+e", "command": "aurix-debugger.runAction", "args": { "id": "erasePflash" }, "when": "debugType == aurixdbg" }
```


## License

The code in this repository is released under the MIT License - see [`LICENSE`](LICENSE).

The prebuilt tools under `bin/` are not covered by that license. They come from AURIX&trade; Configuration Studio and keep their own licenses - see [`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md).
