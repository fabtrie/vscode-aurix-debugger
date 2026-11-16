# Debugger Actions

`actions` in the launch configuration adds the **Debugger Actions** button to the debug toolbar. It offers your own commands: an entry with `items` opens a submenu, an entry with `tcl` runs Tcl commands on the debug server (their output goes to the Debug Console), and an entry with `command` runs a command of this extension.

```jsonc
"actions": [
  {
    "label": "Mass Erase",
    "icon": "trash",
    "items": [
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
    "label": "Chip status",
    "icon": "info",
    "tcl": "tc3xx_status",
    "showOutput": true
  },
  {
    "label": "Show UCBs",
    "icon": "shield",
    "command": "aurix-debugger.showUcbs"
  }
]
```

| Property | Description |
|---|---|
| `label` | Name shown in the list (required) |
| `description`, `detail` | Additional text |
| `icon` | [Codicon](https://microsoft.github.io/vscode-codicons/dist/codicon.html) name |
| `items` | Submenu entries (same format) |
| `tcl` | Tcl command or list of commands; stops at the first error |
| `command` | A command of this extension (`aurix-debugger.*`); runs after `tcl` if both are given |
| `args` | Argument for `command` |
| `confirm` | Confirmation text shown before running |
| `resetAfter` | Reset the target afterwards, like the Reset button |
| `showOutput` | Show the Tcl output in a notification |
| `timeout` | Timeout per command in seconds (default 900) |
| `id` | Lets you run the action from a keybinding |

The action is responsible for the target state it needs, e.g. halting the cores first. The available Tcl commands are in the [Tcl command reference](../aurix-debugger/server/docs/commands.md).

## Keybindings

An action with an `id` can be run from a keybinding (`keybindings.json`):

```json
{ "key": "ctrl+alt+e", "command": "aurix-debugger.runAction", "args": { "id": "eraseDflash" }, "when": "debugType == aurixdbg" }
```
