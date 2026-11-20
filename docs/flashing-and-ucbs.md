# Flashing and UCBs

## Flashing

- With `"enableFlashing": true`, the images in `flashFiles` are programmed when the debug session starts.
- **Flash Target** (debug toolbar) programs them again, e.g. after a rebuild.
- Afterwards the target is reset with `resetType` and runs, or halts at the reset vector with `"resetSuspend": true`.
- Only sectors whose content differs are erased and programmed; the others are left alone.
- PFLASH is programmed by a small loader that runs on cpu0: a full 6 MB image takes well under half a minute, a small change about a second.
  - The loader uses the start of cpu0's PSPR and DSPR, and turns off the CPU0 and safety watchdogs until the next reset. Flashing from VS Code ends with a reset, so the application starts as usual.
  - If the loader cannot run, the debugger programs the flash itself, which is much slower (minutes on a BlueBox). `monitor flash loader off` forces that.

### Flash files

`flashFiles` takes ELF, Intel HEX, S-record (`.srec`, `.s19`, `.s28`, `.s37`, `.mot`) and binary files. They are merged into one image; where they overlap, later files win.

An entry is a path, or an object:

| Key | Description |
|---|---|
| `path` | File path |
| `offset` | Added to the file's addresses. A binary file is placed at its `offset`. |
| `type` | `elf`, `ihex`, `srec`, `bin` (default: detected from the file) |

```json
"flashFiles": [
  "${workspaceFolder}/build/app.hex",
  { "path": "${workspaceFolder}/build/cal.bin", "offset": "0x80300000" }
]
```

- Images other than ELF carry no debug information: list the ELF in `symbolFiles` too. `symbolFiles` are never flashed.
- Use the post-build HEX if the build patches CRCs or fills gaps after linking.
- Image data outside the flash (e.g. initialized RAM in a post-build HEX) is skipped with a warning.
- `verifyFlash` (default `true`) reads the programmed sectors back and compares them with the image.
- UCB records in a HEX file are not programmed with the flash. With `ucbWrite`, they are reviewed after flashing and written only when you confirm them in the UCBs panel (see [Writing](#writing)).

### Reset

- **Reset** resets the target with `resetType` (default `reset`).
- It runs afterwards, or halts at the reset vector with `resetSuspend`.
- **Show Reset Capabilities** lists the reset types the debug server offers.

## UCBs

The TC3xx user configuration blocks (UCBs) hold the boot headers, flash protection, passwords, the debug lock and more. **Show UCBs** opens a panel that reads all 48 UCBs from the device.

### Reading

- Every field is shown by name and meaning, e.g. a boot header's start address or a protected sector list.
- Highlighted: fields that differ from the delivery state, UCBs whose ORIG and COPY differ, changes the device installs only at the next reset.
- The UCB list and the field list have a text filter and **Only changed**.
- Passwords are masked; **Show** reads them.
- Reading and exporting work without `ucbWrite`.

### Comparing with a HEX file

![Comparing the device's boot headers with a HEX file](../vscode-aurix-tcf-debugger/media/readme/ucb-compare.png)

- **Compare with HEX…** compares the device's UCBs field by field with the UCB records of image files, e.g. the post-build HEX.
- Take the file's values for everything, one UCB or single fields; they are then written like any other change.
- Read-only UCBs are shown but not applied.
- Passwords are compared without being shown.

### Exporting

**Export…** writes selected UCBs to an Intel HEX file. Filters (name, confirmed or unconfirmed, group, only changed, ORIG and/or COPY) tick the matching UCBs; adjust them one by one.

### Writing

Fields are edited in place; each change stays pending (with undo) until written. Writing needs:

1. `"ucbWrite": true` in the launch configuration. Cannot be turned on from the Debug Console, by Claude or by a script.
2. Your confirmation after the debug server's review. The review lists blocking problems and warnings per UCB, e.g.:
   - a valid boot header must remain, and its start address must hold code
   - the HSM boot code must be present before HSM boot is enabled
   - the debugger must not lock itself out

   Changes that need the password to undo need a stronger confirmation. For changes that can never be undone, you type the device name.

- A backup of all UCBs is saved before writing.
- COPY is written before ORIG, so a power loss during the write leaves a valid UCB.
- Claude and scripts can read UCBs and run the review, but cannot confirm a write.

> **Warning:** a wrong UCB can make the device unbootable or lock it for good. Read the review carefully.
