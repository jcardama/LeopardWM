<p align="center">
  <img src="assets/leopardwm.png" alt="LeopardWM" width="128" />
</p>

# LeopardWM

[![CI](https://github.com/jcardama/LeopardWM/actions/workflows/ci.yml/badge.svg)](https://github.com/jcardama/LeopardWM/actions/workflows/ci.yml)
[![License: GPL-3.0](https://img.shields.io/badge/License-GPL--3.0-blue.svg)](LICENSE)
![Platform: Windows 10/11](https://img.shields.io/badge/Platform-Windows%2010%2F11-0078D4)
[![Buy Me a Coffee](https://img.shields.io/badge/Buy%20Me%20a%20Coffee-ffdd00?logo=buy-me-a-coffee&logoColor=000)](https://buymeacoffee.com/jcardama)

A scrollable tiling window manager for Windows.

## What Makes It Different

Most Windows tilers use tree or BSP layouts. LeopardWM is **scroll-first**: windows sit on a horizontal strip, and your monitor acts as a viewport that scrolls over them. Navigation stays spatially consistent as windows are added — you move through context instead of constantly rebuilding split trees.

- **Vsync-aligned animations** — smooth scrolling powered by a `DwmFlush`-driven animation engine
- **Written in Rust** — safe, fast, and easy to hack on

## Features

- Multi-monitor workspaces with monitor-aware focus and move
- Global hotkeys with live config reload
- Smooth scroll animations with layout transition effects
- Touchpad gestures with configurable swipe actions
- Drag-and-drop column reorder (Shift+drag to merge windows)
- Floating and fullscreen toggles
- Width and height presets with column equalization
- System tray with pause, reload, settings, and diagnostics
- WebView-based settings GUI
- Safe mode for troubleshooting (`--safe-mode`)
- Built-in diagnostics (`lwm doctor`)
- Workspace persistence and session recovery
- Autostart via Registry

## Installation

Download the latest release from [GitHub Releases](https://github.com/jcardama/LeopardWM/releases):

1. Extract `LeopardWM-x.y.z-x86_64-windows.zip` to a permanent location
2. Run `leopardwm.exe`
3. (Optional) Enable autostart: `lwm autostart enable` (or `leopardwm-cli autostart enable`)

Releases are signed via the [SignPath Foundation](https://signpath.org/) program.

## Quick Start (from source)

Prerequisites: [Rust](https://rustup.rs) with the MSVC toolchain (`stable-x86_64-pc-windows-msvc`)

```bash
git clone https://github.com/jcardama/LeopardWM.git
cd LeopardWM
cargo build --release
```

Start the daemon:

```bash
./target/release/leopardwm.exe
```

A default config is created automatically at `%APPDATA%\leopardwm\config\config.toml`. Customize via the tray icon → Settings, or edit the file directly.

## Default Hotkeys

All hotkeys use `Ctrl+Alt` as the base modifier. Layered pattern: base = focus, +Shift = move, +Win = monitor scope.

| Key | Action |
|---|---|
| `Ctrl+Alt+H/L/J/K` | Focus left / right / down / up |
| `Ctrl+Alt+Shift+H/L` | Move column left / right |
| `Ctrl+Alt+Shift+J/K` | Move window down / up in column |
| `Ctrl+Alt+[` / `]` | Move window to left / right column |
| `Ctrl+Alt+Shift+[` / `]` | Expel window to new column left / right |
| `Ctrl+Alt+Minus` / `Ctrl+Alt+Equals` | Cycle column width down / up |
| `Ctrl+Alt+Shift+Minus` / `Ctrl+Alt+Shift+Equals` | Cycle window height down / up |
| `Ctrl+Alt+0` | Equalize all column widths |
| `Ctrl+Alt+Shift+0` | Equalize window heights in column |
| `Ctrl+Alt+Win+,`/`.` | Focus monitor left / right |
| `Ctrl+Alt+Win+Shift+,`/`.` | Move window to monitor |
| `Ctrl+Alt+W` | Close focused window |
| `Ctrl+Alt+F` | Toggle floating |
| `Ctrl+Alt+Shift+F` | Toggle fullscreen |
| `Ctrl+Alt+P` | Toggle pause |
| `Ctrl+Alt+R` | Refresh (re-enumerate windows) |
| `Ctrl+Alt+Shift+R` | Reload config |
| `Win+Ctrl+Escape` | Emergency restore + panic-revert |

## CLI

LeopardWM ships two interchangeable CLI binaries — both invoke the same code:

| Binary | When to use |
|---|---|
| `leopardwm-cli` | Canonical name. Use in docs, scripts, and shared examples. |
| `lwm` | Short alias for daily typing. |

Examples below use whichever is shorter for the line.

### Daemon lifecycle

```bash
lwm run                # start the daemon (idempotent — no-op if already running)
lwm stop               # stop the daemon
lwm status             # show version, monitor count, window count, uptime
```

### Query state

```bash
lwm query workspace    # current workspace placements as JSON
lwm query focused      # focused window info
lwm query all-windows  # every managed window across all workspaces
```

### Layout commands

Most users drive the layout via hotkeys, but every hotkey has a CLI equivalent — useful for scripting or AutoHotkey integration.

```bash
lwm focus left | right | up | down
lwm move left | right                  # move focused column
lwm move-window up | down              # reorder within a column
lwm workspace 3                        # switch to workspace 3
lwm toggle-floating
lwm toggle-fullscreen
```

### Autostart (boot with Windows)

```bash
lwm autostart enable   # writes HKCU\Software\Microsoft\Windows\CurrentVersion\Run
lwm autostart disable  # removes it
```

This is also exposed as a Settings UI toggle and a tray menu item.

### Troubleshooting

```bash
lwm doctor             # diagnostic checks (config valid, daemon reachable, hotkey conflicts, etc.)
lwm collect-logs       # bundles logs + crash reports into a zip for bug reports
lwm reload             # reload config from disk without restarting
lwm refresh            # re-enumerate windows after weird state
lwm panic-revert       # emergency: uncloak everything, drop daemon out of management
```

Run `lwm help` (or `lwm <subcommand> --help`) for the full surface — there are ~40 subcommands.

## Config & Runtime Paths

> **Note:** Crate names and on-disk paths still use `leopardwm` internally. A full crate rename is future work.

| Item | Path |
|---|---|
| Config | `%APPDATA%\leopardwm\config\config.toml` |
| State | `%APPDATA%\leopardwm\data\workspace-state.json` |
| Log (stdout) | `%TEMP%\leopardwm-daemon.log` |
| Log (stderr) | `%TEMP%\leopardwm-daemon.err.log` |

## Architecture

LeopardWM is a Rust workspace with five crates:

| Crate | Responsibility |
|---|---|
| `leopardwm-core-layout` | Platform-agnostic scrolling layout engine |
| `leopardwm-platform-win32` | Win32 integration, window operations, DwmFlush animation engine |
| `leopardwm-ipc` | Named-pipe command/response protocol |
| `leopardwm-daemon` | Runtime event loop, state management, dedicated message-pump threads |
| `leopardwm-cli` | User-facing CLI (also installed as `lwm` for shorter typing) |

## Platform Constraints

LeopardWM is a **window controller**, not a compositor. DWM remains the compositor. Elevated or protected windows may reject placement/styling changes, and behavior can vary across app frameworks (Win32, WPF, Electron, UWP).

## Built-in Window Exclusions

LeopardWM automatically skips certain windows that should never be tiled. You can add your own rules via `[[window_rules]]` in the config, but these are always active.

### Skipped window classes (platform layer)

These windows are filtered out during enumeration and never enter the layout engine:

| Class | Why |
|---|---|
| `Progman` | Program Manager (desktop) |
| `Shell_TrayWnd` / `Shell_SecondaryTrayWnd` | Taskbar |
| `WorkerW` | Desktop worker |
| `Windows.UI.Core.CoreWindow` | UWP system windows |
| `XamlExplorerHostIslandWindow` / `TopLevelWindowForOverflowXamlIsland` | XAML islands |
| `RAIL_WINDOW` | WSLg RemoteApp — RDP-projected Linux windows that break when repositioned |
| `Ghost` | DWM hung-window replacement — tiling would duplicate the original |
| `#32770` | Standard Win32 dialog (Open/Save/Print/Properties) |
| `Chrome_RenderWidgetHostHWND` | Internal Electron/Chrome render widget, not a real window |

### Ignored executables (window rules)

These processes are ignored via built-in window rules (action = `ignore`):

| Executable | Why |
|---|---|
| `smartscreen.exe` | Windows Defender SmartScreen |
| `consent.exe` | UAC elevation prompt |
| `msiexec.exe` | Windows Installer |
| `CredentialUIBroker.exe` | Windows credential/login prompt |
| `SnippingTool.exe` | Screen capture overlay |

## Support

If you find LeopardWM useful, consider supporting development:

[![Buy Me a Coffee](https://img.shields.io/badge/Buy%20Me%20a%20Coffee-ffdd00?logo=buy-me-a-coffee&logoColor=000)](https://buymeacoffee.com/jcardama)

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

[GPL-3.0](LICENSE)
