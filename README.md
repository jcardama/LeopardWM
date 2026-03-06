<p align="center">
  <img src="assets/leopardwm.png" alt="LeopardWM" width="128" />
</p>

# LeopardWM

[![CI](https://github.com/jcardama/LeopardWM/actions/workflows/ci.yml/badge.svg)](https://github.com/jcardama/LeopardWM/actions/workflows/ci.yml)
[![License: GPL-3.0](https://img.shields.io/badge/License-GPL--3.0-blue.svg)](LICENSE)
![Platform: Windows 10/11](https://img.shields.io/badge/Platform-Windows%2010%2F11-0078D4)

A scrollable tiling window manager for Windows.

## What Makes It Different

Most Windows tilers use tree or BSP layouts. LeopardWM is **scroll-first**: windows sit on a horizontal strip, and your monitor acts as a viewport that scrolls over them. Navigation stays spatially consistent as windows are added — you move through context instead of constantly rebuilding split trees.

- **Vsync-aligned animations** — smooth scrolling powered by a `DwmFlush`-driven animation engine
- **Written in Rust** — safe, fast, and easy to hack on

## Features

- Multi-monitor workspaces with monitor-aware focus and move
- Global hotkeys with live config reload
- Smooth scroll animations and touchpad gestures
- Floating and fullscreen toggles
- Width presets and column equalization
- System tray with pause, reload, settings, and diagnostics
- WebView-based settings GUI
- Safe mode for troubleshooting (`--safe-mode`)
- Built-in diagnostics (`leopardwm-cli doctor`)
- Workspace persistence and session recovery
- Autostart via Registry

## Installation

Download the latest release from [GitHub Releases](https://github.com/jcardama/LeopardWM/releases):

1. Extract `LeopardWM-x.y.z-x86_64-windows.zip` to a permanent location
2. Run `leopardwm.exe`
3. (Optional) Enable autostart: `leopardwm-cli autostart enable`

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
| `Ctrl+Alt+Minus` / `Ctrl+Alt+Equals` | Shrink / grow column |
| `Ctrl+Alt+1` / `2` / `3` | Set width to 1/3, 1/2, 2/3 |
| `Ctrl+Alt+0` | Equalize all column widths |
| `Ctrl+Alt+Win+,`/`.` | Focus monitor left / right |
| `Ctrl+Alt+Win+Shift+,`/`.` | Move window to monitor |
| `Ctrl+Alt+W` | Close focused window |
| `Ctrl+Alt+F` | Toggle floating |
| `Ctrl+Alt+Shift+F` | Toggle fullscreen |
| `Ctrl+Alt+P` | Toggle pause |
| `Ctrl+Alt+R` | Refresh (re-enumerate windows) |
| `Ctrl+Alt+Shift+R` | Reload config |
| `Win+Ctrl+Escape` | Emergency restore + panic-revert |

## Autostart

```bash
leopardwm-cli autostart enable   # writes HKCU\...\Run entry
leopardwm-cli autostart disable  # removes it
```

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
| `leopardwm-cli` | User-facing CLI |

## Platform Constraints

LeopardWM is a **window controller**, not a compositor. DWM remains the compositor. Elevated or protected windows may reject placement/styling changes, and behavior can vary across app frameworks (Win32, WPF, Electron, UWP).

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

[GPL-3.0](LICENSE)
